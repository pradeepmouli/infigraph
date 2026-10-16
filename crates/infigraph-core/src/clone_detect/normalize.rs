//! A function body as a sequence of normalized tokens.
//!
//! Two bodies that differ only in what their identifiers and literals are
//! called, in their comments or in their layout are the same code copied. The
//! lexer here is deliberately language-agnostic: it cannot tell a type from a
//! variable, but it does not need to, because every identifier that is not a
//! keyword becomes one placeholder on both sides of a comparison. Keywords,
//! operators and punctuation keep their text, so the shape of the code stays.

use crate::embed::fnv1a64;

/// One normalized token: a hash of its class (`ident`, `num`, `str`) or, for
/// a keyword, operator or punctuation mark, of its text.
pub type Token = u64;

/// Words that keep their text. A keyword missing from this list is treated as
/// an identifier on both sides of every comparison, so the omission costs
/// precision on that word and nothing else.
const KEYWORDS: &[&str] = &[
    "if",
    "else",
    "elif",
    "for",
    "foreach",
    "while",
    "do",
    "loop",
    "match",
    "switch",
    "case",
    "default",
    "break",
    "continue",
    "return",
    "yield",
    "await",
    "async",
    "try",
    "catch",
    "except",
    "finally",
    "throw",
    "raise",
    "fn",
    "func",
    "function",
    "def",
    "lambda",
    "let",
    "var",
    "const",
    "mut",
    "val",
    "class",
    "struct",
    "enum",
    "trait",
    "interface",
    "impl",
    "type",
    "new",
    "delete",
    "in",
    "is",
    "as",
    "not",
    "and",
    "or",
    "self",
    "this",
    "super",
    "null",
    "nil",
    "none",
    "true",
    "false",
    "pub",
    "public",
    "private",
    "protected",
    "static",
    "use",
    "import",
    "from",
    "with",
    "where",
    "unsafe",
    "go",
    "defer",
    "select",
    "range",
    "end",
    "then",
    "begin",
];

/// How a language writes its comments.
struct Comments {
    line: &'static [&'static str],
    block: Option<(&'static str, &'static str)>,
}

const C_STYLE: Comments = Comments {
    line: &["//"],
    block: Some(("/*", "*/")),
};
const HASH: Comments = Comments {
    line: &["#"],
    block: None,
};
const DASHES: Comments = Comments {
    line: &["--"],
    block: Some(("/*", "*/")),
};

fn comments_for(language: &str) -> &'static Comments {
    match language {
        "python" | "ruby" | "bash" | "shell" | "sh" | "perl" | "r" | "yaml" | "toml" | "make"
        | "elixir" | "nim" => &HASH,
        "sql" | "lua" | "haskell" | "ada" | "vhdl" | "elm" => &DASHES,
        _ => &C_STYLE,
    }
}

fn class(name: &str) -> Token {
    fnv1a64(name.as_bytes())
}

/// The placeholder text a class of token is rendered as.
const IDENT: &str = "_";
const NUM: &str = "0";
const STR: &str = "\"\"";

/// The normalized tokens of `body`, a function or method's source text.
pub fn normalize(body: &str, language: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    lex(body, language, |text| tokens.push(class(text)));
    tokens
}

/// `body` as the text its normalized tokens spell, space-separated: what a
/// text embedder is given, so that a rename or a re-layout does not move the
/// vector.
pub fn normalized_text(body: &str, language: &str) -> String {
    let mut text = String::new();
    lex(body, language, |token| {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(token);
    });
    text
}

/// Calls `emit` with the text each normalized token is spelled as.
fn lex(body: &str, language: &str, mut emit: impl FnMut(&str)) {
    let comments = comments_for(language);
    let mut rest = body;
    while let Some(c) = rest.chars().next() {
        if c.is_whitespace() {
            rest = &rest[c.len_utf8()..];
        } else if let Some(after) = skip_comment(rest, comments) {
            rest = after;
        } else if matches!(c, '"' | '`') || (c == '\'' && closes_on_line(rest)) {
            emit(STR);
            rest = after_string(rest, c);
        } else if c.is_ascii_digit() {
            emit(NUM);
            rest = skip_while(rest, |c| c.is_alphanumeric() || matches!(c, '.' | '_'));
        } else if c.is_alphabetic() || matches!(c, '_' | '$') {
            let word_end = word_len(rest);
            let lower = rest[..word_end].to_ascii_lowercase();
            emit(if KEYWORDS.contains(&lower.as_str()) {
                &lower
            } else {
                IDENT
            });
            rest = &rest[word_end..];
        } else {
            emit(&rest[..c.len_utf8()]);
            rest = &rest[c.len_utf8()..];
        }
    }
}

/// `rest` without the comment it starts with, or `None` if it starts with none.
fn skip_comment<'a>(rest: &'a str, comments: &Comments) -> Option<&'a str> {
    if comments.line.iter().any(|marker| rest.starts_with(marker)) {
        return Some(rest.find('\n').map_or("", |at| &rest[at..]));
    }
    let (open, close) = comments.block?;
    let inner = rest.strip_prefix(open)?;
    Some(
        inner
            .find(close)
            .map_or("", |at| &inner[at + close.len()..]),
    )
}

/// A `'` opens a string only if it closes on the same line: otherwise it is a
/// lifetime or a stray quote and stays punctuation.
fn closes_on_line(rest: &str) -> bool {
    let line = rest[1..].split('\n').next().unwrap_or("");
    let mut escaped = false;
    line.chars().any(|c| match (escaped, c) {
        (true, _) => {
            escaped = false;
            false
        }
        (false, '\\') => {
            escaped = true;
            false
        }
        (false, '\'') => true,
        _ => false,
    })
}

/// `rest` (which starts with the opening `quote`) after the string it opens.
fn after_string(rest: &str, quote: char) -> &str {
    let mut escaped = false;
    for (at, c) in rest.char_indices().skip(1) {
        match (escaped, c) {
            (true, _) => escaped = false,
            (false, '\\') => escaped = true,
            (false, c) if c == quote => return &rest[at + c.len_utf8()..],
            _ => {}
        }
    }
    ""
}

fn skip_while(rest: &str, keep: impl Fn(char) -> bool) -> &str {
    let end = rest.find(|c| !keep(c)).unwrap_or(rest.len());
    &rest[end..]
}

fn word_len(rest: &str) -> usize {
    rest.find(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '$')))
        .unwrap_or(rest.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(src: &str) -> Vec<Token> {
        normalize(src, "rust")
    }

    #[test]
    fn renamed_identifiers_and_changed_literals_normalize_alike() {
        let a = "fn total(items: &[u32]) -> u32 { let mut sum = 0; for x in items { sum += x * 2; } sum }";
        let b = "fn grand(rows: &[u32]) -> u32 { let mut acc = 7; for r in rows { acc += r * 9; } acc }";
        assert!(!norm(a).is_empty());
        assert_eq!(norm(a), norm(b));
    }

    #[test]
    fn comments_and_layout_do_not_change_the_tokens() {
        let tight = "fn f(a: u32) -> u32 { a + 1 }";
        let loose =
            "fn f(a: u32) -> u32 {\n    // add one\n    /* really\n       one */\n    a  +  1\n}\n";
        assert_eq!(norm(tight), norm(loose));
    }

    #[test]
    fn a_different_shape_is_a_different_sequence() {
        let plus = "fn f(a: u32) -> u32 { a + 1 }";
        let minus = "fn f(a: u32) -> u32 { a - 1 }";
        let branch = "fn f(a: u32) -> u32 { if a > 1 { a } else { 1 } }";
        assert_ne!(norm(plus), norm(minus));
        assert_ne!(norm(plus), norm(branch));
    }

    #[test]
    fn strings_are_one_token_whatever_they_hold() {
        let a = "call(\"hello // not a comment\", 'x')";
        let b = "call(\"bye\", 'y')";
        assert_eq!(norm(a), norm(b));
    }

    #[test]
    fn a_hash_comments_language_drops_hash_comments_only_there() {
        let py = "def f(a):\n    # note\n    return a\n";
        let py_clean = "def f(a):\n    return a\n";
        assert_eq!(normalize(py, "python"), normalize(py_clean, "python"));
        // In a C-style language `#` is not a comment: it is a token.
        assert_ne!(normalize(py, "rust"), normalize(py_clean, "rust"));
    }

    #[test]
    fn the_text_spells_the_tokens_and_hides_the_names() {
        let a = normalized_text("fn total(items: u32) -> u32 { items + 41 }", "rust");
        let b = normalized_text("fn grand(rows: u32) -> u32 { rows + 7 }", "rust");
        assert_eq!(a, "fn _ ( _ : _ ) - > _ { _ + 0 }");
        assert_eq!(a, b);
    }

    #[test]
    fn an_empty_body_has_no_tokens() {
        assert!(norm("").is_empty());
        assert!(norm("  // only a comment\n").is_empty());
    }
}
