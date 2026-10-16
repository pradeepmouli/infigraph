//! Clone detection: the one `find_clones` the MCP tool, the CLI command and the
//! web API render.
//!
//! A pair of functions is compared by the strongest signal both have:
//!
//! 1. [`Signal::ExactBody`]: the graph's `signature_hash` (a hash of the body)
//!    is equal. Score 1.
//! 2. [`Signal::BodyEmbedding`]: both bodies have an embedding in
//!    `clone-bodies.bin`; the score is their cosine.
//! 3. [`Signal::TokenFallback`]: both have a body sketch but one has no
//!    embedding (no model, or the embedder failed); the score is the sketch's
//!    Jaccard estimate.
//! 4. [`Signal::Signature`]: one of them has no entry in the sidecar at all
//!    (not built yet, the file changed since it was indexed, a remote graph);
//!    the score is the cosine of the signature-text vectors in
//!    `embeddings.bin`, the signal this module replaces.
//!
//! Every pair says which signal scored it, so a consumer can tell a pair of
//! bodies from a pair of names. Nothing here embeds anything at query time on
//! a local graph: a function without a vector is compared by a weaker signal
//! or, when it has none, listed in [`Report::not_compared`].

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use anyhow::Result;

use super::bodies::{self, BODIES_FILE};
use super::sketch::{self, Sketch};
use crate::embed::{cosine_similarity, EmbedProvider};
use crate::graph::GraphBackend;
use crate::path_scope::PathScope;

crate::settings! {
    clones {
        // Function bodies of fewer normalized tokens are not clones of
        // anything: getters, delegators, empty stubs. The default keeps 84% of
        // this repository's 4,301 functions and methods (p10 is 23 tokens,
        // median 78).
        min_tokens: u64 = 30,
        // Up to this many functions every pair is scored exactly; above it LSH
        // proposes the pairs. Brute force is about n^2/2 cosine products: 10k
        // functions is 50M, seconds.
        exact_below: u64 = 10_000,
    }
}

/// Which signal scored a pair; see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    ExactBody,
    BodyEmbedding,
    TokenFallback,
    Signature,
}

impl Signal {
    pub fn label(self) -> &'static str {
        match self {
            Signal::ExactBody => "exact-body",
            Signal::BodyEmbedding => "body-embedding",
            Signal::TokenFallback => "token-fallback",
            Signal::Signature => "signature",
        }
    }
}

/// How candidate pairs were chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Every pair of functions was scored.
    Exact,
    /// Candidate pairs came from LSH over the body sketches (and from groups
    /// of equal bodies); the symbols without a body were scored among
    /// themselves only if they were few enough.
    Lsh,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Exact => "exact",
            Mode::Lsh => "lsh",
        }
    }
}

/// What a function's body contributes: present only for a function the sidecar
/// describes as it is now.
#[derive(Debug, Clone)]
pub struct Body {
    /// The graph's `signature_hash` of the body, as `bodies::signature_of`.
    pub hash: u64,
    pub token_count: u32,
    pub sketch: Sketch,
    pub embedding: Option<Vec<f32>>,
}

/// A function or method to compare.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub id: String,
    pub name: String,
    pub file: String,
    pub body: Option<Body>,
    /// The signature-text vector from `embeddings.bin`, when there is one.
    pub signature_vector: Option<Vec<f32>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Side {
    pub id: String,
    pub name: String,
    pub file: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClonePair {
    pub score: f32,
    pub signal: Signal,
    pub a: Side,
    pub b: Side,
}

#[derive(Debug, Clone)]
pub struct Options {
    /// Report pairs scoring at least this.
    pub threshold: f32,
    /// Report at most this many pairs.
    pub limit: usize,
    /// Function bodies of fewer normalized tokens are not clones of anything:
    /// getters, delegators, empty stubs.
    pub min_tokens: usize,
    /// Up to this many functions every pair is scored; above it LSH proposes
    /// the pairs.
    pub exact_below: usize,
    pub kinds: Vec<String>,
    pub scope: PathScope,
}

impl Options {
    /// The options for a run over the project at `root`: the caller's
    /// threshold, limit, comma-separated `kinds` and scope, and `[clones]`
    /// from the project's and the user's config. A `[clones]` value that does
    /// not parse is an error, as for every settings group.
    pub fn for_project(
        root: &Path,
        threshold: f32,
        limit: usize,
        kinds: &str,
        scope: PathScope,
    ) -> Result<Self> {
        let settings = Clones::resolve(
            RawClones::default(),
            crate::settings_file::ConfigScope::Project(root),
        )?;
        Ok(Self {
            threshold,
            limit,
            min_tokens: settings.min_tokens as usize,
            exact_below: settings.exact_below as usize,
            kinds: kinds
                .split(',')
                .map(|k| k.trim().to_string())
                .filter(|k| !k.is_empty())
                .collect(),
            scope,
        })
    }
}

impl Default for Options {
    fn default() -> Self {
        Self {
            threshold: 0.92,
            limit: 20,
            min_tokens: 30,
            exact_below: 10_000,
            kinds: vec!["Function".to_string(), "Method".to_string()],
            scope: PathScope::everything(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Report {
    /// The best `limit` pairs at or above the threshold, best first.
    pub pairs: Vec<ClonePair>,
    /// More pairs reached the threshold than `limit` allowed.
    pub truncated: bool,
    /// Pairs at or above the threshold, before the limit.
    pub total_found: usize,
    pub mode: Mode,
    /// Pairs that were scored.
    pub candidates_verified: usize,
    /// Functions considered (in scope, of a requested kind, not below
    /// `min_tokens`).
    pub symbols: usize,
    /// Of those, functions with no body entry: compared by `signature` only.
    pub signature_only: usize,
    /// Functions that took part in no comparison: no vector of any kind, or
    /// too many signature-only functions to compare them all.
    pub not_compared: usize,
    /// LSH buckets and groups of equal bodies that were skipped for holding
    /// more functions than a clone family plausibly has.
    pub skipped_families: usize,
    /// The graph is remote, which has no clone-bodies sidecar: every function
    /// was compared by `signature`.
    pub remote: bool,
}

/// A group of equal bodies larger than this is boilerplate, not a clone pair.
const MAX_FAMILY: usize = 256;

/// Pairs among functions of the same file are skipped, since method-sized
/// look-alikes within a file are rarely copy-paste; a scope holding one file
/// has no other kind of pair, so there they count.
fn same_file_counts(candidates: &[Candidate]) -> bool {
    candidates.windows(2).all(|w| w[0].file == w[1].file)
}

/// Scores `a` against `b`, or `None` if nothing they share can compare them.
fn score(a: &Candidate, b: &Candidate) -> Option<(f32, Signal)> {
    if let (Some(x), Some(y)) = (&a.body, &b.body) {
        if x.hash == y.hash {
            return Some((1.0, Signal::ExactBody));
        }
        return match (&x.embedding, &y.embedding) {
            (Some(p), Some(q)) => Some((cosine_similarity(p, q), Signal::BodyEmbedding)),
            _ => Some((sketch::jaccard(&x.sketch, &y.sketch), Signal::TokenFallback)),
        };
    }
    match (&a.signature_vector, &b.signature_vector) {
        (Some(p), Some(q)) => Some((cosine_similarity(p, q), Signal::Signature)),
        _ => None,
    }
}

/// Every pair of `candidates` that scores at least the threshold, by the
/// strongest signal each pair has.
pub fn compare(candidates: Vec<Candidate>, options: &Options) -> Report {
    let candidates: Vec<Candidate> = candidates
        .into_iter()
        .filter(|c| {
            c.body
                .as_ref()
                .is_none_or(|b| b.token_count as usize >= options.min_tokens)
        })
        .collect();
    let symbols = candidates.len();
    let signature_only = candidates.iter().filter(|c| c.body.is_none()).count();
    let allow_same_file = same_file_counts(&candidates);
    let mode = if symbols <= options.exact_below {
        Mode::Exact
    } else {
        Mode::Lsh
    };

    let mut skipped_families = 0;
    let mut not_compared = 0;
    let pair_indexes: Vec<(usize, usize)> = match mode {
        Mode::Exact => {
            not_compared = candidates
                .iter()
                .filter(|c| c.body.is_none() && c.signature_vector.is_none())
                .count();
            (0..symbols)
                .flat_map(|i| ((i + 1)..symbols).map(move |j| (i, j)))
                .collect()
        }
        Mode::Lsh => {
            let mut pairs: HashSet<(usize, usize)> = HashSet::new();
            let sketches: Vec<Option<&Sketch>> = candidates
                .iter()
                .map(|c| c.body.as_ref().map(|b| &b.sketch))
                .collect();
            let found = sketch::candidates(&sketches);
            skipped_families += found.oversized_buckets;
            pairs.extend(found.pairs);
            // Equal bodies share every bucket, but a big family of them
            // overflows one: take them from their hash instead.
            let mut equal: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
            for (i, c) in candidates.iter().enumerate() {
                if let Some(b) = &c.body {
                    equal.entry(b.hash).or_default().push(i);
                }
            }
            for members in equal.values().filter(|m| m.len() > 1) {
                if members.len() > MAX_FAMILY {
                    skipped_families += 1;
                    continue;
                }
                for (at, &i) in members.iter().enumerate() {
                    pairs.extend(members[at + 1..].iter().map(|&j| (i, j)));
                }
            }
            // Functions with no body are compared among themselves, if few.
            let bare: Vec<usize> = (0..symbols)
                .filter(|&i| candidates[i].body.is_none())
                .collect();
            if bare.len() <= options.exact_below {
                for (at, &i) in bare.iter().enumerate() {
                    pairs.extend(bare[at + 1..].iter().map(|&j| (i, j)));
                }
            } else {
                not_compared = bare.len();
            }
            let mut sorted: Vec<_> = pairs.into_iter().collect();
            sorted.sort_unstable();
            sorted
        }
    };

    let mut verified = 0;
    let mut found: Vec<ClonePair> = Vec::new();
    for (i, j) in pair_indexes {
        let (a, b) = (&candidates[i], &candidates[j]);
        if !allow_same_file && a.file == b.file {
            continue;
        }
        let Some((value, signal)) = score(a, b) else {
            continue;
        };
        verified += 1;
        if value >= options.threshold {
            found.push(ClonePair {
                score: value,
                signal,
                a: Side {
                    id: a.id.clone(),
                    name: a.name.clone(),
                    file: a.file.clone(),
                },
                b: Side {
                    id: b.id.clone(),
                    name: b.name.clone(),
                    file: b.file.clone(),
                },
            });
        }
    }
    found.sort_by(|x, y| {
        y.score
            .partial_cmp(&x.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| (&x.a.id, &x.b.id).cmp(&(&y.a.id, &y.b.id)))
    });
    let total_found = found.len();
    found.truncate(options.limit);

    Report {
        truncated: total_found > found.len(),
        pairs: found,
        total_found,
        mode,
        candidates_verified: verified,
        symbols,
        signature_only,
        not_compared,
        skipped_families,
        remote: false,
    }
}

/// Only plain identifiers go into the symbol query: a kind comes from a
/// caller's argument.
fn cypher_list(kinds: &[String]) -> String {
    kinds
        .iter()
        .filter(|k| !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        .map(|k| format!("'{k}'"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Finds the near-duplicate functions of the project at `root`.
///
/// `signature_embedder` is for a remote graph only, which has no local
/// vectors: it embeds the signature text of the functions that have none.
/// A local graph is passed `None` and never embeds anything here.
pub fn find_clones(
    backend: &dyn GraphBackend,
    root: &Path,
    options: &Options,
    signature_embedder: Option<&dyn EmbedProvider>,
) -> Result<Report> {
    let kinds = cypher_list(&options.kinds);
    let rows = backend.raw_query(&format!(
        "MATCH (s:Symbol) WHERE s.kind IN [{kinds}] \
         RETURN s.id, s.name, s.kind, s.file, s.docstring, s.signature_hash"
    ))?;
    let scope = options.scope.clone().within(backend.repo_filter());
    let mut rows = scope.retain(rows, |r| r.get(3).map(String::as_str).unwrap_or(""));
    rows.sort_by(|a, b| (&a[3], &a[0]).cmp(&(&b[3], &b[0])));

    let remote = crate::daemon::lifecycle::is_remote_backend();
    let ig = root.join(".infigraph");
    let entries: HashMap<String, bodies::BodyEntry> = if remote {
        HashMap::new()
    } else {
        bodies::load(&ig.join(BODIES_FILE))
            .unwrap_or_default()
            .into_iter()
            .map(|e| (e.id.clone(), e))
            .collect()
    };
    let vectors: HashMap<String, Vec<f32>> = if remote {
        HashMap::new()
    } else {
        let path = ig.join("embeddings.bin");
        if path.exists() {
            crate::embed::load_embeddings_cached(&path)?
                .into_iter()
                .collect()
        } else {
            HashMap::new()
        }
    };

    let candidates: Vec<Candidate> = rows
        .iter()
        .map(|row| {
            let (id, name, kind, file) = (&row[0], &row[1], &row[2], &row[3]);
            let doc = row.get(4).map(String::as_str).unwrap_or("");
            let signature = bodies::signature_of(row.get(5).map(String::as_str).unwrap_or(""));
            // An entry counts only if it describes the symbol as it is now.
            let body = entries
                .get(id)
                .filter(|e| e.signature == signature)
                .map(|e| Body {
                    hash: e.signature,
                    token_count: e.token_count,
                    sketch: e.sketch,
                    embedding: (!e.embedding.is_empty()).then(|| e.embedding.clone()),
                });
            let signature_vector = vectors.get(id).cloned().or_else(|| {
                let embedder = signature_embedder?;
                let text = if doc.is_empty() {
                    format!("{kind} {name}")
                } else {
                    format!("{kind} {name}: {doc}")
                };
                embedder.embed(&text).ok().filter(|v| !v.is_empty())
            });
            Candidate {
                id: id.clone(),
                name: name.clone(),
                file: file.clone(),
                body,
                signature_vector,
            }
        })
        .collect();

    let mut report = compare(candidates, options);
    report.remote = remote;
    Ok(report)
}

/// Writes a `SIMILAR_TO` edge for each pair, returning one line per edge that
/// could not be stored. A failure to store never fails the detection.
pub fn store_similar_edges(backend: &dyn GraphBackend, pairs: &[ClonePair]) -> Vec<String> {
    pairs
        .iter()
        .filter_map(|p| {
            backend
                .upsert_similar_edge(&p.a.id, &p.b.id, p.score)
                .err()
                .map(|e| {
                    format!(
                        "failed to store similarity edge {} <-> {}: {e}",
                        p.a.id, p.b.id
                    )
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clone_detect::{normalize::normalize, sketch::sketch as make_sketch};

    fn body(code: &str, embedding: Option<Vec<f32>>) -> Body {
        let tokens = normalize(code, "python");
        Body {
            hash: crate::embed::fnv1a64(code.as_bytes()),
            token_count: tokens.len() as u32,
            sketch: make_sketch(&tokens).unwrap(),
            embedding,
        }
    }

    /// A body whose statements, not just their names, depend on `seed`:
    /// identifiers and numbers normalize away, so only shape tells bodies
    /// apart. 648 statement shapes keep two seeds' shingles almost disjoint.
    fn long(seed: u32) -> String {
        const KEYWORDS: [&str; 8] = [
            "if", "while", "for", "return", "await", "yield", "raise", "try",
        ];
        const OPS: [&str; 9] = ["+", "-", "*", "/", "%", "<", ">", "==", "&"];
        (0..40)
            .map(|i| {
                let h = crate::embed::fnv1a64(format!("{seed}:{i}").as_bytes()) as usize;
                format!(
                    "{} a {} b {} c",
                    KEYWORDS[h % 8],
                    OPS[(h / 8) % 9],
                    OPS[(h / 72) % 9]
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn cand(id: &str, file: &str, body: Option<Body>, signature: Option<Vec<f32>>) -> Candidate {
        Candidate {
            id: id.to_string(),
            name: id.to_string(),
            file: file.to_string(),
            body,
            signature_vector: signature,
        }
    }

    fn unit(axis: usize) -> Vec<f32> {
        let mut v = vec![0.0; 8];
        v[axis] = 1.0;
        v
    }

    fn options() -> Options {
        Options {
            min_tokens: 10,
            ..Options::default()
        }
    }

    #[test]
    fn equal_bodies_are_an_exact_body_pair() {
        let code = long(3);
        let report = compare(
            vec![
                cand("a", "a.py", Some(body(&code, Some(unit(0)))), None),
                cand("b", "b.py", Some(body(&code, Some(unit(1)))), None),
            ],
            &options(),
        );
        assert_eq!(report.pairs.len(), 1);
        assert_eq!(report.pairs[0].signal, Signal::ExactBody);
        assert_eq!(report.pairs[0].score, 1.0);
    }

    #[test]
    fn a_different_name_with_the_same_body_is_found_and_the_same_name_with_another_body_is_not() {
        // The signature vectors are what the legacy signal compared: `same`
        // and `twin` read alike, `other` does not.
        let (x, y) = (long(3), long(5));
        let report = compare(
            vec![
                cand("same", "a.py", Some(body(&x, Some(unit(0)))), Some(unit(7))),
                cand("twin", "b.py", Some(body(&y, Some(unit(1)))), Some(unit(7))),
                cand(
                    "other",
                    "c.py",
                    Some(body(&x, Some(unit(0)))),
                    Some(unit(6)),
                ),
            ],
            &options(),
        );
        let named: Vec<(&str, &str)> = report
            .pairs
            .iter()
            .map(|p| (p.a.id.as_str(), p.b.id.as_str()))
            .collect();
        assert_eq!(named, vec![("same", "other")], "{:?}", report.pairs);
    }

    #[test]
    fn bodies_with_close_embeddings_are_a_body_embedding_pair() {
        let (x, y) = (long(3), long(5));
        let mut near = unit(0);
        near[1] = 0.1;
        let report = compare(
            vec![
                cand("a", "a.py", Some(body(&x, Some(unit(0)))), None),
                cand("b", "b.py", Some(body(&y, Some(near))), None),
            ],
            &options(),
        );
        assert_eq!(report.pairs[0].signal, Signal::BodyEmbedding);
        assert!(report.pairs[0].score > 0.99);
    }

    #[test]
    fn without_embeddings_the_sketch_decides_and_says_so() {
        let x = long(3);
        let mut y = x.clone();
        y.push_str("\nv99 = g(v1)"); // a near copy: one statement added
        let report = compare(
            vec![
                cand("a", "a.py", Some(body(&x, None)), None),
                cand("b", "b.py", Some(body(&y, None)), None),
                cand("c", "c.py", Some(body(&long(5), None)), None),
            ],
            &Options {
                threshold: 0.8,
                ..options()
            },
        );
        assert_eq!(report.pairs.len(), 1, "{:?}", report.pairs);
        assert_eq!(report.pairs[0].signal, Signal::TokenFallback);
        assert_eq!(
            (report.pairs[0].a.id.as_str(), report.pairs[0].b.id.as_str()),
            ("a", "b")
        );
    }

    #[test]
    fn a_function_with_no_body_entry_is_compared_by_signature_and_labeled() {
        let report = compare(
            vec![
                cand("a", "a.py", None, Some(unit(2))),
                cand("b", "b.py", None, Some(unit(2))),
                cand(
                    "c",
                    "c.py",
                    Some(body(&long(3), Some(unit(2)))),
                    Some(unit(2)),
                ),
                cand("lost", "d.py", None, None),
            ],
            &options(),
        );
        let labels: Vec<_> = report.pairs.iter().map(|p| p.signal.label()).collect();
        assert!(labels.iter().all(|l| *l == "signature"), "{labels:?}");
        assert_eq!(report.pairs.len(), 3);
        assert_eq!(report.signature_only, 3);
        assert_eq!(report.not_compared, 1);
    }

    #[test]
    fn bodies_under_min_tokens_are_not_clones_of_anything() {
        let tiny = "return x";
        let report = compare(
            vec![
                cand("a", "a.py", Some(body(tiny, Some(unit(0)))), None),
                cand("b", "b.py", Some(body(tiny, Some(unit(0)))), None),
            ],
            &Options {
                min_tokens: 10,
                ..Options::default()
            },
        );
        assert!(report.pairs.is_empty());
        assert_eq!(report.symbols, 0);
    }

    #[test]
    fn pairs_within_one_file_are_skipped_unless_the_scope_is_one_file() {
        let code = long(3);
        let two_files = compare(
            vec![
                cand("a", "x.py", Some(body(&code, None)), None),
                cand("b", "x.py", Some(body(&code, None)), None),
                cand("c", "y.py", Some(body(&long(5), None)), None),
            ],
            &options(),
        );
        assert!(two_files.pairs.is_empty());
        let one_file = compare(
            vec![
                cand("a", "x.py", Some(body(&code, None)), None),
                cand("b", "x.py", Some(body(&code, None)), None),
            ],
            &options(),
        );
        assert_eq!(one_file.pairs.len(), 1);
    }

    #[test]
    fn the_report_is_truncated_and_says_so() {
        let code = long(3);
        let report = compare(
            vec![
                cand("a", "a.py", Some(body(&code, None)), None),
                cand("b", "b.py", Some(body(&code, None)), None),
                cand("c", "c.py", Some(body(&code, None)), None),
            ],
            &Options {
                limit: 2,
                ..options()
            },
        );
        assert_eq!(report.pairs.len(), 2);
        assert_eq!(report.total_found, 3);
        assert!(report.truncated);
    }

    #[test]
    fn above_the_size_threshold_lsh_proposes_the_pairs_and_still_finds_the_copy() {
        let original = long(3);
        let mut edited = original.clone();
        edited.push_str("\nv99 = g(v1)");
        let mut candidates = vec![
            cand("copy_a", "a.py", Some(body(&original, None)), None),
            cand("copy_b", "b.py", Some(body(&edited, None)), None),
        ];
        for seed in 20..60 {
            candidates.push(cand(
                &format!("other{seed}"),
                &format!("o{seed}.py"),
                Some(body(&long(seed), None)),
                None,
            ));
        }
        let total_pairs = candidates.len() * (candidates.len() - 1) / 2;
        let report = compare(
            candidates,
            &Options {
                threshold: 0.8,
                exact_below: 10,
                ..options()
            },
        );
        assert_eq!(report.mode, Mode::Lsh);
        assert!(
            report
                .pairs
                .iter()
                .any(|p| p.a.id == "copy_a" && p.b.id == "copy_b"),
            "{:?}",
            report.pairs
        );
        assert!(
            report.candidates_verified < total_pairs,
            "LSH verified {} of {total_pairs} pairs",
            report.candidates_verified
        );
    }

    #[test]
    fn a_family_of_equal_bodies_larger_than_a_plausible_clone_family_is_skipped_and_counted() {
        let code = long(3);
        let candidates: Vec<Candidate> = (0..MAX_FAMILY + 1)
            .map(|i| {
                cand(
                    &format!("f{i}"),
                    &format!("f{i}.py"),
                    Some(body(&code, None)),
                    None,
                )
            })
            .collect();
        let report = compare(
            candidates,
            &Options {
                exact_below: 0,
                ..options()
            },
        );
        assert!(report.pairs.is_empty());
        assert!(report.skipped_families >= 1);
    }

    #[test]
    fn small_projects_score_every_pair() {
        let report = compare(
            vec![
                cand("a", "a.py", Some(body(&long(3), None)), None),
                cand("b", "b.py", Some(body(&long(5), None)), None),
                cand("c", "c.py", Some(body(&long(7), None)), None),
            ],
            &options(),
        );
        assert_eq!(report.mode, Mode::Exact);
        assert_eq!(report.candidates_verified, 3);
    }
}
