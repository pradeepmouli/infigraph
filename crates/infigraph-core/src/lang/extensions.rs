//! The extensions the JavaScript and TypeScript families claim, declared once.
//!
//! `.mts` and `.cts` are TypeScript and `.cjs` is JavaScript: the same grammars
//! as `.ts` and `.js`, differing only in the module system the file declares.
//! They were missing from every list at once because every consumer that is
//! keyed by extension carried its own copy -- the language packs, the route
//! language, the test-file heuristics, the security, bridge and reflection
//! rules, the `agent` language census. Each of those reads this module, so a
//! new member is declared here and nowhere else. The one list that cannot is
//! the shell hook `infigraph-enforce.sh`, whose glob a test checks against
//! these (`crates/infigraph-cli/tests/hook_extensions.rs`).

/// One language pack's extensions, in the two spellings consumers use.
#[derive(Debug, Clone, Copy)]
pub struct Extensions {
    /// Without the dot: `ts`. What a rule list or a census names.
    pub bare: &'static [&'static str],
    /// With the dot: `.ts`. What a language pack registers and a path ends with.
    pub dotted: &'static [&'static str],
}

/// Both spellings from one literal list, so they cannot disagree.
macro_rules! extensions {
    ($($ext:literal),+ $(,)?) => {
        Extensions {
            bare: &[$($ext),+],
            dotted: &[$(concat!(".", $ext)),+],
        }
    };
}

/// The `typescript` pack. `.tsx` is its own pack, with its own grammar.
pub const TYPESCRIPT: Extensions = extensions!("ts", "mts", "cts");
/// The `tsx` pack.
pub const TSX: Extensions = extensions!("tsx");
/// The `javascript` pack.
pub const JAVASCRIPT: Extensions = extensions!("js", "jsx", "mjs", "cjs");

/// The packs above that belong to the same ecosystem; a rule that names one
/// extension of a pack applies to the whole pack (see [`extension_selected`]).
const PACKS: &[&Extensions] = &[&TYPESCRIPT, &TSX, &JAVASCRIPT];

/// The marks that make a JavaScript or TypeScript file a test: `x.test.ts`,
/// `x.spec.cjs`.
const TEST_MARKS: &[&str] = &[".test", ".spec"];

impl Extensions {
    /// `ext` (no dot) is one of these.
    pub fn contains(&self, ext: &str) -> bool {
        self.bare.contains(&ext)
    }

    /// `path` ends in one of these (case-sensitive: lower the path first).
    pub fn matches_path(&self, path: &str) -> bool {
        self.dotted.iter().any(|ext| path.ends_with(ext))
    }

    /// `path` ends in `mark` followed by one of these: `x.test.mts` for
    /// `.test`.
    pub fn marked_path(&self, path: &str, mark: &str) -> bool {
        self.dotted
            .iter()
            .filter_map(|ext| path.strip_suffix(ext))
            .any(|stem| stem.ends_with(mark))
    }
}

/// `path` is named like a JavaScript or TypeScript test, any variant.
pub fn is_script_test_name(path: &str) -> bool {
    PACKS
        .iter()
        .any(|pack| TEST_MARKS.iter().any(|mark| pack.marked_path(path, mark)))
}

/// Whether a rule whose extension list is `list` applies to a file with
/// extension `ext` (no dot). A list that names one extension of a pack names
/// the pack: `["js", "ts"]` covers `.cjs` and `.mts`, which a rule written
/// for JavaScript and TypeScript is about. Every other extension is compared
/// as written.
pub fn extension_selected(list: &[&str], ext: &str) -> bool {
    list.contains(&ext)
        || PACKS
            .iter()
            .any(|pack| pack.contains(ext) && list.iter().any(|named| pack.contains(named)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_spellings_come_from_one_list() {
        for pack in PACKS {
            assert_eq!(pack.bare.len(), pack.dotted.len());
            for (bare, dotted) in pack.bare.iter().zip(pack.dotted) {
                assert_eq!(*dotted, format!(".{bare}"));
            }
        }
    }

    #[test]
    fn module_system_variants_are_members_of_their_language() {
        assert!(TYPESCRIPT.contains("mts") && TYPESCRIPT.contains("cts"));
        assert!(JAVASCRIPT.contains("cjs") && JAVASCRIPT.contains("mjs"));
        assert!(!TYPESCRIPT.contains("tsx") && TSX.contains("tsx"));
    }

    #[test]
    fn test_names_are_recognised_in_every_variant() {
        for name in [
            "a.test.ts",
            "a.test.mts",
            "a.spec.cts",
            "a.test.tsx",
            "a.spec.js",
            "a.test.jsx",
            "a.spec.mjs",
            "a.test.cjs",
        ] {
            assert!(is_script_test_name(name), "{name}");
        }
        for name in ["a.ts", "a.mts", "test.cjs", "a.testing.ts", "a.test.py"] {
            assert!(!is_script_test_name(name), "{name}");
        }
    }

    #[test]
    fn a_rule_naming_one_member_covers_its_pack() {
        let rule = ["py", "js", "ts"];
        for ext in ["py", "js", "ts", "mts", "cts", "cjs", "mjs", "jsx"] {
            assert!(extension_selected(&rule, ext), "{ext}");
        }
        // `.tsx` is its own pack and the rule does not name it.
        for ext in ["tsx", "go", "rs", ""] {
            assert!(!extension_selected(&rule, ext), "{ext}");
        }
        assert!(extension_selected(&["tsx"], "tsx"));
        assert!(!extension_selected(&["ts"], "tsx"));
    }
}
