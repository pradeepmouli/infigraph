//! Every production crate root shadows std's `eprintln!`/`eprint!` with the ones that can survive a failed write.
//!
//! std's versions panic when the write to stderr fails, so a daemon whose log file hit a size limit or a
//! full disk, or an MCP server whose client closed its pipe, died of its own logging (exit 101), and its
//! replacement died the same way while the cause lasted. `infigraph_core::eprintln!` (`stderr_macros.rs`)
//! calls std's own until the process's `main` has called `proclog::keep_running_on_stderr_failure()`, and
//! then ignores the failure; a crate takes the macros by naming them on its `extern crate` at the root, which
//! shadows std's in every module below with no change to a call site.
//!
//! This test reads the workspace's source and fails on
//!
//! * (a) a crate root (`src/lib.rs`, `src/main.rs`, `src/bin/*.rs`) of a crate that depends on core without the
//!   shadow (core itself declares `mod stderr_macros;` first, `#[macro_use]`), and
//! * (b) non-test source that names std's macro directly (`std::eprintln!`, `std::eprint!`), which would bypass
//!   the shadow. The one place that may is `proclog::eprintln_args` / `eprint_args`, the functions the macros
//!   expand to: with the switch off they call std's, which libtest captures.
//!
//! `EXEMPT` lists what is deliberately outside (a), with the reason.

use std::path::{Path, PathBuf};

/// Roots that are not shadowed, and why.
const EXEMPT: &[(&str, &str)] = &[
    (
        "crates/lsp-to-scip/",
        "standalone: no dependency on core, so it cannot name the macros; a command-line tool, not run \
         inside a daemon or an MCP server",
    ),
    (
        "crates/tree-sitter-vb6/",
        "a generated grammar crate with no logging and no dependency on core",
    ),
    (
        "crates/infigraph-cli/src/bin/",
        "developer tools (golden exports, the cozo benchmarks): run by hand, never inside a daemon or an MCP \
         server",
    ),
];

const SHADOW: &str = "macro_use(eprintln, eprint)";

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the workspace root")
        .to_path_buf()
}

fn rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/")
}

fn exempt(relative: &str) -> bool {
    EXEMPT
        .iter()
        .any(|(prefix, _)| relative.starts_with(prefix))
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// A crate's roots: the files a compiled target starts from.
fn roots_of(crate_dir: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for name in ["lib.rs", "main.rs"] {
        let p = crate_dir.join("src").join(name);
        if p.exists() {
            roots.push(p);
        }
    }
    if let Ok(entries) = std::fs::read_dir(crate_dir.join("src").join("bin")) {
        for entry in entries.flatten() {
            if entry.path().extension().is_some_and(|e| e == "rs") {
                roots.push(entry.path());
            }
        }
    }
    roots
}

#[test]
fn every_production_root_shadows_the_panicking_eprintln() {
    let root = workspace_root();
    let mut missing = Vec::new();
    for entry in std::fs::read_dir(root.join("crates")).unwrap().flatten() {
        let crate_dir = entry.path();
        let manifest = std::fs::read_to_string(crate_dir.join("Cargo.toml")).unwrap_or_default();
        let is_core = crate_dir.ends_with("infigraph-core");
        let uses_core = manifest
            .lines()
            .any(|l| l.trim_start().starts_with("infigraph-core"));
        for file in roots_of(&crate_dir) {
            let relative = rel(&root, &file);
            if exempt(&relative) {
                continue;
            }
            let text = std::fs::read_to_string(&file).unwrap();
            if is_core {
                // Declared before every other module, so its textual scope covers them all.
                let declared = text.find("mod stderr_macros;");
                let first_other = text
                    .lines()
                    .position(|l| {
                        let l = l.trim_start();
                        (l.starts_with("mod ") || l.starts_with("pub mod "))
                            && !l.contains("stderr_macros")
                    })
                    .map(|n| text.lines().take(n).map(|l| l.len() + 1).sum::<usize>());
                let ok = matches!((declared, first_other), (Some(d), Some(o)) if d < o)
                    && text.contains("#[macro_use]\nmod stderr_macros;");
                if !ok {
                    missing.push(format!(
                        "{relative}: must declare `#[macro_use] mod stderr_macros;` before every other module"
                    ));
                }
            } else if uses_core {
                if !(text.contains(SHADOW) && text.contains("extern crate infigraph_core;")) {
                    missing.push(format!(
                        "{relative}: needs `#[{SHADOW}] extern crate infigraph_core;` \
                         at the top, or an EXEMPT entry saying why not"
                    ));
                }
            } else {
                missing.push(format!(
                    "{relative}: a root of a crate that does not depend on infigraph-core; add it to EXEMPT \
                     with the reason, or give the crate the dependency"
                ));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "production crate roots without the non-panicking eprintln!:\n  {}",
        missing.join("\n  ")
    );
}

#[test]
fn no_source_names_stds_panicking_eprintln_directly() {
    let root = workspace_root();
    let mut files = Vec::new();
    rust_files(&root.join("crates"), &mut files);
    let mut hits = Vec::new();
    for file in files {
        let relative = rel(&root, &file);
        if relative.contains("/tests/") || relative.contains("/benches/") || exempt(&relative) {
            continue;
        }
        let text = std::fs::read_to_string(&file).unwrap();
        // Non-test code only: everything before a `#[cfg(test)] mod`.
        let end = text
            .find("#[cfg(test)]\nmod ")
            .or_else(|| text.find("#[cfg(test)]\npub mod "))
            .unwrap_or(text.len());
        for (n, line) in text[..end].lines().enumerate() {
            let code = line.trim_start();
            if code.starts_with("//") {
                continue;
            }
            if code.contains("std::eprintln!") || code.contains("std::eprint!") {
                // The one allowed place: the two functions the macros expand to.
                let enclosing = text[..end]
                    .lines()
                    .take(n)
                    .filter(|l| {
                        l.trim_start().starts_with("pub fn ") || l.trim_start().starts_with("fn ")
                    })
                    .last()
                    .unwrap_or("");
                let allowed = relative == "crates/infigraph-core/src/proclog.rs"
                    && (enclosing.contains("fn eprintln_args(")
                        || enclosing.contains("fn eprint_args("));
                if !allowed {
                    hits.push(format!("{relative}:{}: {}", n + 1, code.trim()));
                }
            }
        }
    }
    assert!(
        hits.is_empty(),
        "std's panicking eprintln! named directly (use the shadowed `eprintln!`):\n  {}",
        hits.join("\n  ")
    );
}
