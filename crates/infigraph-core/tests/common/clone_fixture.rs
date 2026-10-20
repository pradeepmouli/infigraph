//! A generated project of function families for the clone tests, shared by
//! `clone_detection.rs` and the MCP crate's `clone_signals.rs` (each includes
//! this file with `#[path]`), and a hermetic stand-in for the embedding model.

#![allow(dead_code)]

use std::path::Path;

use infigraph_core::embed::EmbedProvider;
use infigraph_core::CodeIndex;
use infigraph_languages::bundled_registry;

/// A stand-in for a text embedding model, with no model to load or download.
/// Short texts (a symbol's signature: kind, name, parameters, path, docstring)
/// are embedded by their words, so names and docstrings decide; long texts (a
/// normalized function body) by their word 4-grams, so shape decides. Equal
/// texts have equal vectors.
pub struct WordEmbedder;

impl EmbedProvider for WordEmbedder {
    fn name(&self) -> &'static str {
        "test-words"
    }
    fn dimension(&self) -> usize {
        512
    }
    fn embed_batch(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(texts
            .iter()
            .map(|text| {
                let words: Vec<&str> = text.split_whitespace().collect();
                let grams: Vec<String> = if words.len() < 24 {
                    words.iter().map(|w| w.to_string()).collect()
                } else {
                    words.windows(4).map(|g| g.join(" ")).collect()
                };
                let mut v = vec![0f32; 512];
                for gram in grams {
                    v[infigraph_core::embed::fnv1a64(gram.as_bytes()) as usize % 512] += 1.0;
                }
                let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1.0);
                v.iter().map(|x| x / norm).collect()
            })
            .collect())
    }
}

/// One valid Python statement whose shape depends on `seed` and `i`; `callee`
/// wraps the first operand in a call. (Invalid Python would lose its body to
/// error recovery and every such function would hash alike.)
pub fn statement(seed: u32, i: u32, callee: Option<&str>) -> String {
    const OPS: [&str; 8] = ["+", "-", "*", "/", "%", "<", ">", "=="];
    let h = infigraph_core::embed::fnv1a64(format!("{seed}:{i}").as_bytes()) as usize;
    let a = callee.map_or("a".to_string(), |c| format!("{c}(a)"));
    let (op1, op2) = (OPS[(h / 8) % 8], OPS[(h / 64) % 8]);
    let expr = format!("{a} {op1} b {op2} c");
    match h % 6 {
        0 => format!("    if {expr}: pass"),
        1 => format!("    while {expr}: break"),
        2 => format!("    for q in {expr}: pass"),
        3 => format!("    assert {expr}"),
        4 => format!("    return {expr}"),
        _ => format!("    raise {expr}"),
    }
}

/// A function body of `n` statements whose shape depends on `seed`.
pub fn body(seed: u32, n: u32) -> String {
    (0..n)
        .map(|i| statement(seed, i, None))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `body`'s shape with a call to `callee` in every statement.
pub fn calling(seed: u32, callee: &str) -> String {
    (0..30)
        .map(|i| statement(seed, i, Some(callee)))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn function(name: &str, doc: &str, code: &str) -> String {
    format!("def {name}(a, b):\n    \"\"\"{doc}\"\"\"\n{code}\n")
}

pub fn write(root: &Path, file: &str, text: &str) {
    let path = root.join(file);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// Four families, each in two files:
/// - `renamed` / `copied`: the same code under other names and another docstring;
/// - `process` in a.py and in b.py: the same name, parameters and docstring,
///   different bodies (the signature-text comparison calls these clones);
/// - `twin_one` / `twin_two`: byte-identical bodies under different
///   signatures (it cannot see these);
/// - `check` in g.py and in h.py: the same name, parameters, docstring, shape
///   and locals, one calling `validate` and the other `remove` in every
///   statement: two bodies that do different things.
///
/// Indexed, and embedded with [`WordEmbedder`]. The handle is returned open:
/// a test that goes on to open the project again must drop it first.
pub fn project() -> (tempfile::TempDir, CodeIndex) {
    let dir = tempfile::tempdir().unwrap();
    let r = dir.path();
    write(
        r,
        "src/a.py",
        &function("process", "Process the batch.", &body(1, 30)),
    );
    write(
        r,
        "src/b.py",
        &function("process", "Process the batch.", &body(2, 30)),
    );
    write(
        r,
        "src/c.py",
        &function("renamed", "Add things.", &body(3, 30)),
    );
    write(
        r,
        "src/d.py",
        &function("copied", "Combine items.", &body(3, 30)),
    );
    write(
        r,
        "src/e.py",
        &format!("def twin_one(x, y):\n{}\n", body(4, 30)),
    );
    write(
        r,
        "src/f.py",
        &format!("def twin_two(left, right):\n{}\n", body(4, 30)),
    );
    write(
        r,
        "src/g.py",
        &function("check", "Check the batch.", &calling(5, "validate")),
    );
    write(
        r,
        "src/h.py",
        &function("check", "Check the batch.", &calling(5, "remove")),
    );
    let mut ig = CodeIndex::open(r, bundled_registry().unwrap()).unwrap();
    ig.init().unwrap();
    ig.index().unwrap();
    infigraph_core::embed::update_embeddings_with(ig.backend().unwrap(), r, &[], &|| {
        Box::new(WordEmbedder)
    })
    .unwrap();
    (dir, ig)
}
