use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};

use crate::extract;
use crate::lang::LanguageRegistry;

use super::{ChangeKind, FlatSym, SymbolChange, SymbolDiff};

/// Compute a symbol-level diff between `old_ref` and `new_ref` in `project_root`.
///
/// Uses `git archive` to extract each ref into a temp directory so no
/// working-tree modifications are needed.
pub fn semantic_diff(
    project_root: &Path,
    old_ref: &str,
    new_ref: &str,
    registry: &LanguageRegistry,
) -> Result<SymbolDiff> {
    let changed = compute_changed_files(project_root, old_ref, new_ref);

    let (old_filter, new_filter) = match &changed {
        Some(cf) => (Some(&cf.old_ref_files), Some(&cf.new_ref_files)),
        None => (None, None),
    };

    let old_symbols = extract_ref_symbols(project_root, old_ref, registry, old_filter)
        .with_context(|| format!("failed to extract symbols for ref '{}'", old_ref))?;
    let new_symbols = extract_ref_symbols(project_root, new_ref, registry, new_filter)
        .with_context(|| format!("failed to extract symbols for ref '{}'", new_ref))?;

    Ok(diff_symbol_maps(old_ref, new_ref, old_symbols, new_symbols))
}

struct ChangedFiles {
    old_ref_files: HashSet<String>,
    new_ref_files: HashSet<String>,
}

fn compute_changed_files(
    project_root: &Path,
    old_ref: &str,
    new_ref: &str,
) -> Option<ChangedFiles> {
    let output = std::process::Command::new("git")
        .args(["diff", "--name-status", "--no-renames", old_ref, new_ref])
        .current_dir(project_root)
        .output()
        .ok()?;

    if !output.status.success() {
        eprintln!(
            "infigraph: git diff --name-status failed for {}..{}, falling back to full extraction",
            old_ref, new_ref
        );
        return None;
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let mut old_ref_files = HashSet::new();
    let mut new_ref_files = HashSet::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(2, '\t');
        let status = parts.next().unwrap_or("").trim();
        let path = match parts.next() {
            Some(p) => p.trim().to_string(),
            None => continue,
        };

        match status {
            "A" => {
                new_ref_files.insert(path);
            }
            "D" => {
                old_ref_files.insert(path);
            }
            _ => {
                old_ref_files.insert(path.clone());
                new_ref_files.insert(path);
            }
        }
    }

    Some(ChangedFiles {
        old_ref_files,
        new_ref_files,
    })
}

// ---------------------------------------------------------------------------
// Extract symbols for a git ref
// ---------------------------------------------------------------------------

/// Extract all symbols from a git ref by using `git archive | tar -x` into a
/// temp directory, then walking files through the language registry.
const MAX_ARCHIVE_ARGS: usize = 500;

fn extract_ref_symbols(
    project_root: &Path,
    git_ref: &str,
    registry: &LanguageRegistry,
    file_filter: Option<&HashSet<String>>,
) -> Result<HashMap<String, FlatSym>> {
    if let Some(filter) = file_filter {
        if filter.is_empty() {
            return Ok(HashMap::new());
        }
    }

    let is_working_tree = git_ref == "HEAD" || git_ref == "WORKING";

    if is_working_tree {
        return extract_dir_symbols(project_root, project_root, registry, file_filter);
    }

    let tmp = tempfile::tempdir().context("failed to create temp dir")?;

    let use_filtered_archive = file_filter
        .map(|f| f.len() <= MAX_ARCHIVE_ARGS)
        .unwrap_or(false);

    let archive_output = if use_filtered_archive {
        let filter = file_filter.unwrap();
        let mut args: Vec<&str> = vec!["archive", "--format=tar", git_ref, "--"];
        args.extend(filter.iter().map(|s| s.as_str()));
        std::process::Command::new("git")
            .args(&args)
            .current_dir(project_root)
            .output()
            .context("git archive (filtered) failed")?
    } else {
        std::process::Command::new("git")
            .args(["archive", "--format=tar", git_ref])
            .current_dir(project_root)
            .output()
            .context("git archive failed")?
    };

    if !archive_output.status.success() {
        let err = String::from_utf8_lossy(&archive_output.stderr);
        if use_filtered_archive {
            eprintln!(
                "infigraph: filtered git archive for {} failed, falling back to full archive: {}",
                git_ref,
                err.trim()
            );
            let full_output = std::process::Command::new("git")
                .args(["archive", "--format=tar", git_ref])
                .current_dir(project_root)
                .output()
                .context("git archive (full fallback) failed")?;
            if !full_output.status.success() {
                let err2 = String::from_utf8_lossy(&full_output.stderr);
                anyhow::bail!("git archive {} failed: {}", git_ref, err2.trim());
            }
            return untar_and_extract(tmp.path(), &full_output.stdout, registry, file_filter);
        }
        anyhow::bail!("git archive {} failed: {}", git_ref, err.trim());
    }

    untar_and_extract(tmp.path(), &archive_output.stdout, registry, file_filter)
}

fn untar_and_extract(
    tmp_dir: &Path,
    tar_data: &[u8],
    registry: &LanguageRegistry,
    file_filter: Option<&HashSet<String>>,
) -> Result<HashMap<String, FlatSym>> {
    let mut tar = std::process::Command::new("tar")
        .args(["-x", "-C", tmp_dir.to_str().unwrap_or(".")])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn tar")?;

    if let Some(stdin) = tar.stdin.take() {
        use std::io::Write;
        let mut w = stdin;
        w.write_all(tar_data)?;
    }
    tar.wait().context("tar wait failed")?;

    extract_dir_symbols(tmp_dir, tmp_dir, registry, file_filter)
}

fn extract_dir_symbols(
    root: &Path,
    dir: &Path,
    registry: &LanguageRegistry,
    file_filter: Option<&HashSet<String>>,
) -> Result<HashMap<String, FlatSym>> {
    let mut map = HashMap::new();
    collect_symbols(root, dir, registry, file_filter, &mut map)?;
    Ok(map)
}

static SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    ".venv",
    "venv",
    "target",
    "build",
    "dist",
    "__pycache__",
    ".tox",
    ".infigraph",
];

fn collect_symbols(
    root: &Path,
    dir: &Path,
    registry: &LanguageRegistry,
    file_filter: Option<&HashSet<String>>,
    map: &mut HashMap<String, FlatSym>,
) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if path.is_dir() {
            if !SKIP_DIRS.contains(&name_str.as_ref()) && !name_str.starts_with('.') {
                collect_symbols(root, &path, registry, file_filter, map)?;
            }
        } else if path.is_file() {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if let Some(filter) = file_filter {
                if !filter.contains(&rel) {
                    continue;
                }
            }
            let Ok(source) = std::fs::read(&path) else {
                continue;
            };
            let Some(pack) = registry.for_file_with_content(&rel, &source) else {
                continue;
            };
            let Ok(extraction) = extract::extract_file(&rel, &source, pack) else {
                continue;
            };
            let file = extraction.file.clone();
            for sym in &extraction.symbols {
                let kind_str = sym.kind.as_str().to_string();
                // Key: "file::name::kind" — stable across refs
                let key = format!("{}::{}::{}", file, sym.name, kind_str);
                map.insert(
                    key,
                    FlatSym {
                        file: file.clone(),
                        name: sym.name.clone(),
                        kind: kind_str,
                        sig_hash: sym.signature_hash.clone(),
                        params: sym.parameters.clone().unwrap_or_default(),
                        return_type: sym.return_type.clone().unwrap_or_default(),
                    },
                );
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Diff two symbol maps
// ---------------------------------------------------------------------------

pub(crate) fn sig_matches(a: &FlatSym, b: &FlatSym) -> bool {
    a.params == b.params && a.return_type == b.return_type
}

pub(crate) fn diff_symbol_maps(
    old_ref: &str,
    new_ref: &str,
    old: HashMap<String, FlatSym>,
    new: HashMap<String, FlatSym>,
) -> SymbolDiff {
    let mut changes = Vec::new();

    // Build name→sym map for old (for move detection)
    let old_by_name: HashMap<String, &FlatSym> = old
        .values()
        .map(|s| (format!("{}::{}", s.name, s.kind), s))
        .collect();

    // Check new symbols against old
    for (key, new_sym) in &new {
        if let Some(old_sym) = old.get(key) {
            // Same file+name+kind — classify change type
            if old_sym.sig_hash == new_sym.sig_hash
                || old_sym.sig_hash.is_empty()
                || new_sym.sig_hash.is_empty()
            {
                continue;
            }
            if !sig_matches(old_sym, new_sym) {
                changes.push(SymbolChange {
                    name: new_sym.name.clone(),
                    kind: new_sym.kind.clone(),
                    file: new_sym.file.clone(),
                    change: ChangeKind::SignatureChanged,
                    caller_count: 0,
                });
            } else {
                changes.push(SymbolChange {
                    name: new_sym.name.clone(),
                    kind: new_sym.kind.clone(),
                    file: new_sym.file.clone(),
                    change: ChangeKind::BodyChanged,
                    caller_count: 0,
                });
            }
        } else {
            // Not in old by key. Check if name+kind existed in a different file (move).
            let name_key = format!("{}::{}", new_sym.name, new_sym.kind);
            if let Some(old_sym) = old_by_name.get(&name_key) {
                if old_sym.file != new_sym.file {
                    changes.push(SymbolChange {
                        name: new_sym.name.clone(),
                        kind: new_sym.kind.clone(),
                        file: new_sym.file.clone(),
                        change: ChangeKind::Moved {
                            from_file: old_sym.file.clone(),
                        },
                        caller_count: 0,
                    });
                    continue;
                }
            }
            // Truly new
            changes.push(SymbolChange {
                name: new_sym.name.clone(),
                kind: new_sym.kind.clone(),
                file: new_sym.file.clone(),
                change: ChangeKind::Added,
                caller_count: 0,
            });
        }
    }

    // Removed: in old but not in new (excluding moves already captured)
    let moved_names: std::collections::HashSet<String> = changes
        .iter()
        .filter_map(|c| {
            if matches!(c.change, ChangeKind::Moved { .. }) {
                Some(format!("{}::{}", c.name, c.kind))
            } else {
                None
            }
        })
        .collect();

    for (key, old_sym) in &old {
        if !new.contains_key(key) {
            let name_key = format!("{}::{}", old_sym.name, old_sym.kind);
            if !moved_names.contains(&name_key) {
                changes.push(SymbolChange {
                    name: old_sym.name.clone(),
                    kind: old_sym.kind.clone(),
                    file: old_sym.file.clone(),
                    change: ChangeKind::Removed,
                    caller_count: 0,
                });
            }
        }
    }

    // Rename detection: match Added+Removed pairs in same file+kind by structural similarity
    let added: Vec<usize> = changes
        .iter()
        .enumerate()
        .filter(|(_, c)| c.change == ChangeKind::Added)
        .map(|(i, _)| i)
        .collect();
    let removed: Vec<usize> = changes
        .iter()
        .enumerate()
        .filter(|(_, c)| c.change == ChangeKind::Removed)
        .map(|(i, _)| i)
        .collect();

    let mut rename_pairs: Vec<(usize, usize, String)> = Vec::new();
    let mut used_removed: HashSet<usize> = HashSet::new();

    for &ai in &added {
        let a = &changes[ai];
        for &ri in &removed {
            if used_removed.contains(&ri) {
                continue;
            }
            let r = &changes[ri];
            if a.file != r.file || a.kind != r.kind {
                continue;
            }
            // Same file, same kind, different name — check structural match via sig_hash
            let a_key = format!("{}::{}::{}", a.file, a.name, a.kind);
            let r_key = format!("{}::{}::{}", r.file, r.name, r.kind);
            if let (Some(a_sym), Some(r_sym)) = (new.get(&a_key), old.get(&r_key)) {
                if a_sym.sig_hash == r_sym.sig_hash && !a_sym.sig_hash.is_empty() {
                    rename_pairs.push((ai, ri, r.name.clone()));
                    used_removed.insert(ri);
                    break;
                }
            }
        }
    }

    let mut remove_indices: HashSet<usize> = HashSet::new();
    for (ai, ri, old_name) in &rename_pairs {
        changes[*ai].change = ChangeKind::Renamed {
            old_name: old_name.clone(),
        };
        remove_indices.insert(*ri);
    }

    if !remove_indices.is_empty() {
        let mut idx = 0;
        changes.retain(|_| {
            let keep = !remove_indices.contains(&idx);
            idx += 1;
            keep
        });
    }

    // Sort: Removed first, then signature changes, body changes, moves, renames, added
    changes.sort_by_key(|c| match &c.change {
        ChangeKind::Removed => 0,
        ChangeKind::SignatureChanged => 1,
        ChangeKind::BodyChanged => 2,
        ChangeKind::Moved { .. } => 3,
        ChangeKind::Renamed { .. } => 4,
        ChangeKind::Added => 5,
    });

    SymbolDiff {
        old_ref: old_ref.to_string(),
        new_ref: new_ref.to_string(),
        changes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sym(file: &str, name: &str, kind: &str, sig_hash: &str, params: &str, ret: &str) -> FlatSym {
        FlatSym {
            file: file.to_string(),
            name: name.to_string(),
            kind: kind.to_string(),
            sig_hash: sig_hash.to_string(),
            params: params.to_string(),
            return_type: ret.to_string(),
        }
    }

    fn key(file: &str, name: &str, kind: &str) -> String {
        format!("{}::{}::{}", file, name, kind)
    }

    #[test]
    fn test_body_change_classified_as_body_changed() {
        let mut old = HashMap::new();
        let mut new = HashMap::new();
        let k = key("app.py", "validate_email", "Function");
        old.insert(
            k.clone(),
            sym(
                "app.py",
                "validate_email",
                "Function",
                "hash_v1",
                "(addr: str)",
                "bool",
            ),
        );
        new.insert(
            k.clone(),
            sym(
                "app.py",
                "validate_email",
                "Function",
                "hash_v2",
                "(addr: str)",
                "bool",
            ),
        );

        let diff = diff_symbol_maps("old", "new", old, new);
        assert_eq!(diff.changes.len(), 1);
        assert_eq!(diff.changes[0].change, ChangeKind::BodyChanged);
        assert_eq!(diff.changes[0].name, "validate_email");
    }

    #[test]
    fn test_signature_change_params_differ() {
        let mut old = HashMap::new();
        let mut new = HashMap::new();
        let k = key("app.py", "process", "Function");
        old.insert(
            k.clone(),
            sym(
                "app.py", "process", "Function", "hash_v1", "(x: int)", "None",
            ),
        );
        new.insert(
            k.clone(),
            sym(
                "app.py",
                "process",
                "Function",
                "hash_v2",
                "(x: int, y: int)",
                "None",
            ),
        );

        let diff = diff_symbol_maps("old", "new", old, new);
        assert_eq!(diff.changes.len(), 1);
        assert_eq!(diff.changes[0].change, ChangeKind::SignatureChanged);
    }

    #[test]
    fn test_signature_change_return_type_differs() {
        let mut old = HashMap::new();
        let mut new = HashMap::new();
        let k = key("app.py", "get_value", "Function");
        old.insert(
            k.clone(),
            sym("app.py", "get_value", "Function", "hash_v1", "()", "int"),
        );
        new.insert(
            k.clone(),
            sym("app.py", "get_value", "Function", "hash_v2", "()", "str"),
        );

        let diff = diff_symbol_maps("old", "new", old, new);
        assert_eq!(diff.changes.len(), 1);
        assert_eq!(diff.changes[0].change, ChangeKind::SignatureChanged);
    }

    #[test]
    fn test_rename_same_file_identical_body() {
        let mut old = HashMap::new();
        let mut new = HashMap::new();
        old.insert(
            key("calculator.py", "calculate_order_total", "Function"),
            sym(
                "calculator.py",
                "calculate_order_total",
                "Function",
                "body_hash_abc",
                "(items: list[Item])",
                "Decimal",
            ),
        );
        new.insert(
            key("calculator.py", "compute_order_sum", "Function"),
            sym(
                "calculator.py",
                "compute_order_sum",
                "Function",
                "body_hash_abc",
                "(items: list[Item])",
                "Decimal",
            ),
        );

        let diff = diff_symbol_maps("old", "new", old, new);
        let renamed: Vec<_> = diff
            .changes
            .iter()
            .filter(|c| matches!(&c.change, ChangeKind::Renamed { .. }))
            .collect();
        assert_eq!(
            renamed.len(),
            1,
            "Expected 1 rename, got: {:?}",
            diff.changes
                .iter()
                .map(|c| format!("{}: {}", c.name, c.change))
                .collect::<Vec<_>>()
        );
        assert_eq!(renamed[0].name, "compute_order_sum");
        if let ChangeKind::Renamed { old_name } = &renamed[0].change {
            assert_eq!(old_name, "calculate_order_total");
        }
        let removed: Vec<_> = diff
            .changes
            .iter()
            .filter(|c| c.change == ChangeKind::Removed)
            .collect();
        assert_eq!(removed.len(), 0, "Old name should not appear as Removed");
    }

    #[test]
    fn test_rename_not_detected_different_body() {
        let mut old = HashMap::new();
        let mut new = HashMap::new();
        old.insert(
            key("app.py", "old_func", "Function"),
            sym("app.py", "old_func", "Function", "hash_A", "()", ""),
        );
        new.insert(
            key("app.py", "new_func", "Function"),
            sym("app.py", "new_func", "Function", "hash_B", "()", ""),
        );

        let diff = diff_symbol_maps("old", "new", old, new);
        let renamed: Vec<_> = diff
            .changes
            .iter()
            .filter(|c| matches!(&c.change, ChangeKind::Renamed { .. }))
            .collect();
        assert_eq!(renamed.len(), 0);
        assert!(diff.changes.iter().any(|c| c.change == ChangeKind::Added));
        assert!(diff.changes.iter().any(|c| c.change == ChangeKind::Removed));
    }

    #[test]
    fn test_move_across_files() {
        let mut old = HashMap::new();
        let mut new = HashMap::new();
        old.insert(
            key("old_file.py", "helper", "Function"),
            sym("old_file.py", "helper", "Function", "hash_1", "()", ""),
        );
        new.insert(
            key("new_file.py", "helper", "Function"),
            sym("new_file.py", "helper", "Function", "hash_1", "()", ""),
        );

        let diff = diff_symbol_maps("old", "new", old, new);
        let moved: Vec<_> = diff
            .changes
            .iter()
            .filter(|c| matches!(&c.change, ChangeKind::Moved { .. }))
            .collect();
        assert_eq!(moved.len(), 1);
        if let ChangeKind::Moved { from_file } = &moved[0].change {
            assert_eq!(from_file, "old_file.py");
        }
    }

    #[test]
    fn test_added_and_removed() {
        let mut old = HashMap::new();
        let mut new = HashMap::new();
        old.insert(
            key("app.py", "removed_fn", "Function"),
            sym("app.py", "removed_fn", "Function", "hash_r", "()", ""),
        );
        new.insert(
            key("app.py", "added_fn", "Function"),
            sym("app.py", "added_fn", "Function", "hash_a", "()", ""),
        );

        let diff = diff_symbol_maps("old", "new", old, new);
        assert!(diff
            .changes
            .iter()
            .any(|c| c.change == ChangeKind::Added && c.name == "added_fn"));
        assert!(diff
            .changes
            .iter()
            .any(|c| c.change == ChangeKind::Removed && c.name == "removed_fn"));
    }

    #[test]
    fn test_no_change_same_hash() {
        let mut old = HashMap::new();
        let mut new = HashMap::new();
        let k = key("app.py", "stable_fn", "Function");
        old.insert(
            k.clone(),
            sym("app.py", "stable_fn", "Function", "same_hash", "()", "int"),
        );
        new.insert(
            k.clone(),
            sym("app.py", "stable_fn", "Function", "same_hash", "()", "int"),
        );

        let diff = diff_symbol_maps("old", "new", old, new);
        assert_eq!(diff.changes.len(), 0);
    }

    #[test]
    fn test_modified_helper_returns_all_change_types() {
        let mut old = HashMap::new();
        let mut new = HashMap::new();

        // BodyChanged
        let k1 = key("a.py", "fn_body", "Function");
        old.insert(
            k1.clone(),
            sym("a.py", "fn_body", "Function", "h1", "()", ""),
        );
        new.insert(
            k1.clone(),
            sym("a.py", "fn_body", "Function", "h2", "()", ""),
        );

        // SignatureChanged
        let k2 = key("a.py", "fn_sig", "Function");
        old.insert(
            k2.clone(),
            sym("a.py", "fn_sig", "Function", "h3", "(x: int)", ""),
        );
        new.insert(
            k2.clone(),
            sym("a.py", "fn_sig", "Function", "h4", "(x: str)", ""),
        );

        // Moved
        old.insert(
            key("old.py", "fn_moved", "Function"),
            sym("old.py", "fn_moved", "Function", "h5", "()", ""),
        );
        new.insert(
            key("new.py", "fn_moved", "Function"),
            sym("new.py", "fn_moved", "Function", "h5", "()", ""),
        );

        // Renamed
        old.insert(
            key("a.py", "old_name", "Function"),
            sym("a.py", "old_name", "Function", "h6", "()", ""),
        );
        new.insert(
            key("a.py", "new_name", "Function"),
            sym("a.py", "new_name", "Function", "h6", "()", ""),
        );

        let diff = diff_symbol_maps("old", "new", old, new);
        let modified: Vec<_> = diff.modified().collect();
        assert_eq!(
            modified.len(),
            4,
            "modified() should include all 4 change types, got: {:?}",
            modified
                .iter()
                .map(|c| format!("{}: {}", c.name, c.change))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_combined_rename_and_body_change() {
        let mut old = HashMap::new();
        let mut new = HashMap::new();
        old.insert(
            key("app.py", "calc_total", "Function"),
            sym(
                "app.py",
                "calc_total",
                "Function",
                "hash_old",
                "(items: list)",
                "float",
            ),
        );
        new.insert(
            key("app.py", "compute_sum", "Function"),
            sym(
                "app.py",
                "compute_sum",
                "Function",
                "hash_new",
                "(items: list)",
                "float",
            ),
        );

        let diff = diff_symbol_maps("old", "new", old, new);
        let renamed: Vec<_> = diff
            .changes
            .iter()
            .filter(|c| matches!(&c.change, ChangeKind::Renamed { .. }))
            .collect();
        assert_eq!(
            renamed.len(),
            0,
            "Should NOT detect rename when body hash differs"
        );
        assert!(diff.changes.iter().any(|c| c.change == ChangeKind::Added));
        assert!(diff.changes.iter().any(|c| c.change == ChangeKind::Removed));
    }
}
