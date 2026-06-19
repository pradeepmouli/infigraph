use super::rules::{ScanStats, Severity};

/// Format scan results as a human-readable report.
pub fn format_scan_results(stats: &ScanStats) -> String {
    if stats.findings.is_empty() {
        return format!(
            "Security scan complete: {} files scanned, no issues found.",
            stats.files_scanned
        );
    }

    let active: Vec<_> = stats.findings.iter().filter(|f| !f.suppressed).collect();
    let suppressed_count = stats.findings.len() - active.len();

    let mut out = format!(
        "Security scan: {} files, {} findings ({} suppressed by sanitizer)  [CRITICAL:{} HIGH:{} MEDIUM:{} LOW:{}]\n\n",
        stats.files_scanned,
        active.len(),
        suppressed_count,
        active.iter().filter(|f| f.severity == Severity::Critical).count(),
        active.iter().filter(|f| f.severity == Severity::High).count(),
        active.iter().filter(|f| f.severity == Severity::Medium).count(),
        active.iter().filter(|f| f.severity == Severity::Low).count(),
    );

    let mut cur_file = String::new();
    for f in &active {
        if f.file != cur_file {
            out.push_str(&format!("\n  {}\n", f.file));
            cur_file = f.file.clone();
        }
        out.push_str(&format!(
            "    [{sev:<8}] L{line:<5} [{rule}] {msg}\n",
            sev = f.severity.to_string(),
            line = f.line,
            rule = f.rule_id,
            msg = f.message,
        ));
        out.push_str(&format!("             {}\n", f.snippet));
    }

    if suppressed_count > 0 {
        out.push_str(&format!(
            "\n--- {} findings suppressed (sanitizer detected nearby) ---\n",
            suppressed_count
        ));
        for f in stats.findings.iter().filter(|f| f.suppressed) {
            out.push_str(&format!(
                "  {}:L{} [{}] suppressed by: {}\n",
                f.file,
                f.line,
                f.rule_id,
                f.sanitizer_hint.as_deref().unwrap_or("unknown"),
            ));
        }
    }

    out
}
