//! The Claude Code enforcement hook is a shell script, so its source-file glob
//! cannot read `infigraph_core::lang`'s extension declaration. This keeps the
//! two from drifting: every extension the JavaScript and TypeScript packs
//! claim must be in the glob, or an edit to a `.mts` file would not be held to
//! the tool-routing rule that an edit to a `.ts` file is.

use std::path::Path;

use infigraph_core::lang::{JAVASCRIPT, TSX, TYPESCRIPT};

#[test]
fn the_enforcement_hook_globs_every_javascript_and_typescript_extension() {
    let hook = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("resources/integrations/claude-code/.claude/hooks/infigraph-enforce.sh");
    let script = std::fs::read_to_string(&hook).unwrap();
    let globs: std::collections::BTreeSet<&str> = script
        .split(|c: char| c.is_whitespace() || matches!(c, '|' | '\\' | ')'))
        .filter(|token| token.starts_with("*."))
        .collect();

    let missing: Vec<String> = [TYPESCRIPT, TSX, JAVASCRIPT]
        .iter()
        .flat_map(|pack| pack.dotted.iter())
        .map(|ext| format!("*{ext}"))
        .filter(|glob| !globs.contains(glob.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "{} does not glob {missing:?}; add them to its source-file case, next to *.ts",
        hook.display()
    );
}
