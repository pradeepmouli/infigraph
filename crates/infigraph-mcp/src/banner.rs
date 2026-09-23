//! Warnings a tool puts above its output: stale results, a degraded read.
//!
//! One format, owned here, so the compression pipeline can set banners
//! aside without any compressor knowing they exist (#173). A compressor
//! recognises its tool's output by the first line -- `compress_search`
//! looks for `Search:` -- and a banner in that position made it pass the
//! whole output through untouched: an index with pending edits, the normal
//! state during active editing, turned search compression off for exactly
//! the longest sessions.

const MARK: &str = "\u{26a0} ";
const END: &str = "\n\n";

/// Put `message` above `out` as a visible warning. The most severe banner
/// is prepended last, so it ends up first.
pub fn prepend(out: &mut String, message: &str) {
    out.insert_str(0, &format!("{MARK}{message}{END}"));
}

/// Split `raw` into its leading banners and the tool output beneath them.
pub fn split(raw: &str) -> (&str, &str) {
    let mut body = raw;
    while body.starts_with(MARK) {
        match body.find(END) {
            Some(i) => body = &body[i + END.len()..],
            None => break,
        }
    }
    raw.split_at(raw.len() - body.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banners_prepend_in_severity_order_and_split_back_off() {
        let mut out = "Search: 'x'\n\nresults".to_string();
        prepend(&mut out, "results may be stale");
        prepend(&mut out, "serving a snapshot");
        assert!(out.starts_with("\u{26a0} serving a snapshot\n\n\u{26a0} results may be stale"));

        let (banners, body) = split(&out);
        assert_eq!(body, "Search: 'x'\n\nresults");
        assert_eq!(banners.matches(MARK).count(), 2);
    }

    #[test]
    fn output_without_a_banner_is_all_body() {
        assert_eq!(split("Search: 'x'"), ("", "Search: 'x'"));
        // A warning sign later in the output is content, not a banner.
        assert_eq!(split("a\n\u{26a0} b\n\n"), ("", "a\n\u{26a0} b\n\n"));
    }
}
