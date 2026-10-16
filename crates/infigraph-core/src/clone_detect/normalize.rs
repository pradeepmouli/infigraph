//! A function body as a sequence of normalized tokens.
//!
//! Two bodies that differ only in what their *locals* are called, in their
//! literals, their comments or their layout are the same code copied. Two
//! bodies that call different functions are not, however alike they look, so
//! the normalization removes what a rename cannot change and keeps what it
//! cannot hide:
//!
//! - normalized: local variables and parameters (one placeholder), numbers and
//!   strings (one placeholder each), comments and whitespace;
//! - kept, as their text: keywords, operators and punctuation, and every name
//!   that is not a local: a callee (a word followed by `(`), a member or path
//!   segment (a word after `.`, `->` or `::`, or before `::`) and a type name
//!   (a word starting with a capital).
//!
//! The lexer is language-agnostic and does not parse, so "local" is decided by
//! that heuristic and can be wrong in both directions. The one place it is
//! corrected is a function's own name: a word before `(` that follows a
//! defining keyword (`fn`, `def`, `function`, `class`...) or another
//! non-keyword word (a C-style `int total(`) is a definition, not a call, and
//! is normalized, so a copy under a new name is still a copy. A lowercase type
//! name (`u32`, `int`) is read as a local; it reads the same on both sides of
//! any comparison, which costs precision on that one word and nothing else.

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

/// Keywords after which a name is being defined, not called.
const DEFINERS: &[&str] = &[
    "fn",
    "func",
    "function",
    "def",
    "class",
    "struct",
    "enum",
    "trait",
    "interface",
    "type",
];

/// What the last token lexed was, for the next word's sake.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Prev {
    Start,
    Keyword(&'static str),
    /// A word that is not a keyword (a local, a type, a name).
    Word,
    Other(char),
}

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
    let (mut prev, mut before) = (Prev::Start, Prev::Start);
    while let Some(c) = rest.chars().next() {
        let token = if c.is_whitespace() {
            rest = &rest[c.len_utf8()..];
            continue;
        } else if let Some(after) = skip_comment(rest, comments) {
            rest = after;
            continue;
        } else if matches!(c, '"' | '`') || (c == '\'' && closes_on_line(rest)) {
            emit(STR);
            rest = after_string(rest, c);
            Prev::Other('"')
        } else if c.is_ascii_digit() {
            emit(NUM);
            rest = skip_while(rest, |c| c.is_alphanumeric() || matches!(c, '.' | '_'));
            Prev::Other('0')
        } else if c.is_alphabetic() || matches!(c, '_' | '$') {
            let word_end = word_len(rest);
            let word = &rest[..word_end];
            let lower = word.to_ascii_lowercase();
            rest = &rest[word_end..];
            if let Some(keyword) = KEYWORDS.iter().find(|k| **k == lower) {
                emit(keyword);
                Prev::Keyword(keyword)
            } else {
                let after = rest.trim_start();
                let member = matches!(
                    (before, prev),
                    (_, Prev::Other('.'))
                        | (Prev::Other(':'), Prev::Other(':'))
                        | (Prev::Other('-'), Prev::Other('>'))
                );
                let defined = match prev {
                    Prev::Keyword(k) => DEFINERS.contains(&k),
                    Prev::Word => true,
                    _ => false,
                };
                let kept = member
                    || after.starts_with("::")
                    || word.starts_with(char::is_uppercase)
                    || (after.starts_with('(') && !defined);
                emit(if kept { word } else { IDENT });
                Prev::Word
            }
        } else {
            let mut buf = [0u8; 4];
            emit(c.encode_utf8(&mut buf));
            rest = &rest[c.len_utf8()..];
            Prev::Other(c)
        };
        (before, prev) = (prev, token);
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
        assert_eq!(a, "fn _ ( _ : _ ) - > u32 { _ + 0 }");
        assert_eq!(a, b);
    }

    #[test]
    fn callee_names_are_kept_so_different_calls_are_different_code() {
        let validate = "fn f(x: u32) { validate(x); }";
        let purge = "fn f(x: u32) { purge(x); }";
        let renamed_local = "fn f(y: u32) { validate(y); }";
        assert_ne!(norm(validate), norm(purge));
        assert_eq!(norm(validate), norm(renamed_local));
        // A space before the parenthesis is still a call.
        assert_eq!(norm("validate (x)"), norm("validate(x)"));
        assert_ne!(norm("validate (x)"), norm("purge (x)"));
    }

    #[test]
    fn member_and_path_names_are_kept() {
        assert_ne!(norm("a.save(b)"), norm("a.drop(b)"));
        assert_ne!(norm("self.count + 1"), norm("self.total + 1"));
        assert_ne!(norm("p->next"), norm("p->prev"));
        assert_ne!(norm("io::read(x)"), norm("net::read(x)"));
        assert_ne!(norm("fs::read(x)"), norm("fs::write(x)"));
        // The receiver is a local and is not.
        assert_eq!(norm("alpha.save(b)"), norm("beta.save(b)"));
    }

    #[test]
    fn type_names_are_kept() {
        assert_ne!(
            norm("let a: Vec<String> = x;"),
            norm("let a: Vec<Count> = x;")
        );
        assert_eq!(
            norm("let a: Vec<String> = x;"),
            norm("let b: Vec<String> = y;")
        );
    }

    #[test]
    fn the_name_a_function_is_defined_under_is_not_kept() {
        assert_eq!(
            norm("fn total(x: u32) { x }"),
            norm("fn grand(y: u32) { y }")
        );
        assert_eq!(
            normalize("def total(x):\n    return x", "python"),
            normalize("def grand(y):\n    return y", "python")
        );
        // A C-style declaration: a word before the name is its type.
        assert_eq!(
            normalize("int total(int x) { return x; }", "c"),
            normalize("int grand(int y) { return y; }", "c")
        );
        // After a keyword that precedes expressions it is a call.
        assert_ne!(norm("return validate(x)"), norm("return purge(x)"));
        assert_ne!(norm("await validate(x)"), norm("await purge(x)"));
    }

    #[test]
    fn the_text_keeps_the_names_it_keeps() {
        assert_eq!(
            normalized_text(
                "fn f(x: u32) { Vec::new(); validate(x); x.save(); }",
                "rust"
            ),
            "fn _ ( _ : _ ) { Vec : : new ( ) ; validate ( _ ) ; _ . save ( ) ; }"
        );
    }

    #[test]
    fn an_empty_body_has_no_tokens() {
        assert!(norm("").is_empty());
        assert!(norm("  // only a comment\n").is_empty());
    }
}
