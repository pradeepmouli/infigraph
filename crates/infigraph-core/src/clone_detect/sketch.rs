//! A fixed-size MinHash sketch of a body's token shingles, and the banded LSH
//! index that turns a set of sketches into candidate pairs.
//!
//! The sketch is what lets two bodies be compared without either's text: the
//! fraction of equal slots estimates the Jaccard similarity of their 4-token
//! shingle sets. It is stored beside the embedding, and it is the whole of the
//! signal for a symbol that has no embedding.

use std::collections::{HashMap, HashSet};

use super::normalize::Token;

/// Slots per sketch: 64 x 4 bytes = 256 bytes a symbol.
pub const SKETCH_LEN: usize = 64;
/// Tokens per shingle.
pub const SHINGLE: usize = 4;
/// LSH shape: `BANDS` bands of `ROWS` slots. Two bodies at Jaccard `s` share a
/// bucket in some band with probability `1 - (1 - s^ROWS)^BANDS`: about 0.64
/// at 0.5, 0.98 at 0.7 and 1.0 from 0.9.
pub const BANDS: usize = 16;
pub const ROWS: usize = SKETCH_LEN / BANDS;
/// A bucket holding more sketches than this is boilerplate shared by that many
/// bodies, not a pair of clones; it yields no candidates and is counted.
pub const MAX_BUCKET: usize = 256;

pub type Sketch = [u32; SKETCH_LEN];

fn splitmix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// The sketch of `tokens`, or `None` for no tokens.
pub fn sketch(tokens: &[Token]) -> Option<Sketch> {
    if tokens.is_empty() {
        return None;
    }
    let mut slots = [u32::MAX; SKETCH_LEN];
    // A body shorter than one shingle is its own single shingle.
    let width = SHINGLE.min(tokens.len());
    for shingle in tokens.windows(width) {
        let base = shingle
            .iter()
            .fold(0u64, |h, &t| splitmix(h.rotate_left(5) ^ t));
        for (i, slot) in slots.iter_mut().enumerate() {
            let hashed = (splitmix(base ^ splitmix(i as u64)) >> 32) as u32;
            *slot = (*slot).min(hashed);
        }
    }
    Some(slots)
}

/// The estimated Jaccard similarity of the token sets two sketches summarize.
pub fn jaccard(a: &Sketch, b: &Sketch) -> f32 {
    let equal = a.iter().zip(b).filter(|(x, y)| x == y).count();
    equal as f32 / SKETCH_LEN as f32
}

/// The pairs LSH proposes, and how many buckets it set aside.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Candidates {
    /// Index pairs `(i, j)` with `i < j`, each once.
    pub pairs: Vec<(usize, usize)>,
    /// Buckets over [`MAX_BUCKET`] that proposed nothing.
    pub oversized_buckets: usize,
}

/// Candidate pairs among `sketches`; an entry that is `None` takes no part.
pub fn candidates(sketches: &[Option<&Sketch>]) -> Candidates {
    let mut pairs: HashSet<(usize, usize)> = HashSet::new();
    let mut oversized_buckets = 0;
    for band in 0..BANDS {
        let mut buckets: HashMap<u64, Vec<usize>> = HashMap::new();
        for (index, sketch) in sketches.iter().enumerate() {
            let Some(sketch) = sketch else { continue };
            let rows = &sketch[band * ROWS..(band + 1) * ROWS];
            let key = rows
                .iter()
                .fold(0u64, |h, &row| splitmix(h ^ u64::from(row)));
            buckets.entry(key).or_default().push(index);
        }
        for members in buckets.values() {
            if members.len() > MAX_BUCKET {
                oversized_buckets += 1;
                continue;
            }
            for (at, &i) in members.iter().enumerate() {
                for &j in &members[at + 1..] {
                    pairs.insert((i, j));
                }
            }
        }
    }
    let mut pairs: Vec<(usize, usize)> = pairs.into_iter().collect();
    pairs.sort_unstable();
    Candidates {
        pairs,
        oversized_buckets,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic pseudo-random token sequence.
    fn tokens(seed: u64, len: usize) -> Vec<Token> {
        (0..len as u64)
            .map(|i| splitmix(seed ^ (i * 7919)) % 40)
            .collect()
    }

    fn sk(t: &[Token]) -> Sketch {
        sketch(t).unwrap()
    }

    #[test]
    fn no_tokens_have_no_sketch() {
        assert!(sketch(&[]).is_none());
    }

    #[test]
    fn the_same_tokens_give_the_same_sketch_and_similarity_one() {
        let t = tokens(1, 120);
        assert_eq!(sk(&t), sk(&t));
        assert_eq!(jaccard(&sk(&t), &sk(&t)), 1.0);
    }

    #[test]
    fn a_body_shorter_than_one_shingle_still_has_a_sketch() {
        let short = sk(&[3, 4]);
        assert_eq!(jaccard(&short, &sk(&[3, 4])), 1.0);
        assert!(jaccard(&short, &sk(&[3, 5])) < 0.2);
    }

    #[test]
    fn a_near_copy_scores_high_and_an_unrelated_body_low() {
        let original = tokens(1, 200);
        let mut edited = original.clone();
        edited[100] = 99; // one token changed: 4 of ~197 shingles differ
        let unrelated = tokens(2, 200);
        let near = jaccard(&sk(&original), &sk(&edited));
        let far = jaccard(&sk(&original), &sk(&unrelated));
        assert!(near > 0.85, "near copy scored {near}");
        assert!(far < 0.25, "unrelated bodies scored {far}");
    }

    #[test]
    fn the_estimate_tracks_the_true_shingle_jaccard() {
        // The second body is the first with its back half replaced: the
        // shingle sets overlap by a known amount.
        let a = tokens(5, 160);
        let mut b = a[..80].to_vec();
        b.extend(tokens(6, 80));
        let set = |t: &[Token]| -> HashSet<Vec<Token>> {
            t.windows(SHINGLE).map(<[Token]>::to_vec).collect()
        };
        let (sa, sb) = (set(&a), set(&b));
        let truth = sa.intersection(&sb).count() as f32 / sa.union(&sb).count() as f32;
        let estimate = jaccard(&sk(&a), &sk(&b));
        assert!(
            (estimate - truth).abs() < 0.15,
            "estimate {estimate} vs true {truth}"
        );
    }

    #[test]
    fn lsh_proposes_the_near_copy_and_not_the_stranger() {
        let original = tokens(1, 200);
        let mut edited = original.clone();
        edited[100] = 99;
        let (a, b, c) = (sk(&original), sk(&edited), sk(&tokens(2, 200)));
        let found = candidates(&[Some(&a), Some(&b), Some(&c)]);
        assert_eq!(found.pairs, vec![(0, 1)]);
        assert_eq!(found.oversized_buckets, 0);
    }

    #[test]
    fn lsh_skips_entries_without_a_sketch_and_lists_each_pair_once() {
        let t = sk(&tokens(1, 100));
        let found = candidates(&[Some(&t), None, Some(&t), Some(&t)]);
        assert_eq!(found.pairs, vec![(0, 2), (0, 3), (2, 3)]);
    }

    #[test]
    fn a_bucket_of_boilerplate_proposes_nothing_and_is_counted() {
        let t = sk(&tokens(1, 100));
        let many = vec![Some(&t); MAX_BUCKET + 1];
        let found = candidates(&many);
        assert!(found.pairs.is_empty());
        assert_eq!(found.oversized_buckets, BANDS);
    }
}
