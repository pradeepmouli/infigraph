use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Severity of a security finding.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Critical,
    High,
    Medium,
    Low,
    Info,
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Severity::Critical => write!(f, "CRITICAL"),
            Severity::High => write!(f, "HIGH"),
            Severity::Medium => write!(f, "MEDIUM"),
            Severity::Low => write!(f, "LOW"),
            Severity::Info => write!(f, "INFO"),
        }
    }
}

/// Category of security issue.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum Category {
    SqlInjection,
    HardcodedSecret,
    DangerousEval,
    InsecureDeserialization,
    PathTraversal,
    Ssrf,
    Xxe,
    WeakCrypto,
    CommandInjection,
    InsecureRandom,
    XssRisk,
    OpenRedirect,
    Other(String),
}

impl std::fmt::Display for Category {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Category::SqlInjection => write!(f, "SQL Injection"),
            Category::HardcodedSecret => write!(f, "Hardcoded Secret"),
            Category::DangerousEval => write!(f, "Dangerous Eval"),
            Category::InsecureDeserialization => write!(f, "Insecure Deserialization"),
            Category::PathTraversal => write!(f, "Path Traversal"),
            Category::Ssrf => write!(f, "SSRF"),
            Category::Xxe => write!(f, "XXE"),
            Category::WeakCrypto => write!(f, "Weak Crypto"),
            Category::CommandInjection => write!(f, "Command Injection"),
            Category::InsecureRandom => write!(f, "Insecure Random"),
            Category::XssRisk => write!(f, "XSS Risk"),
            Category::OpenRedirect => write!(f, "Open Redirect"),
            Category::Other(s) => write!(f, "{}", s),
        }
    }
}

/// A single security finding in the codebase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    pub file: String,
    pub line: u32,
    pub col: u32,
    pub severity: Severity,
    pub category: Category,
    pub rule_id: String,
    pub message: String,
    pub snippet: String,
}

/// Summary stats from a security scan.
#[derive(Debug, Default)]
pub struct ScanStats {
    pub files_scanned: usize,
    pub findings: Vec<Finding>,
}

impl ScanStats {
    pub fn critical_count(&self) -> usize {
        self.findings.iter().filter(|f| f.severity == Severity::Critical).count()
    }
    pub fn high_count(&self) -> usize {
        self.findings.iter().filter(|f| f.severity == Severity::High).count()
    }
    pub fn medium_count(&self) -> usize {
        self.findings.iter().filter(|f| f.severity == Severity::Medium).count()
    }
    pub fn low_count(&self) -> usize {
        self.findings.iter().filter(|f| f.severity == Severity::Low).count()
    }
}

// ---------------------------------------------------------------------------
// Rule definitions
// ---------------------------------------------------------------------------

struct Rule {
    id: &'static str,
    category: fn() -> Category,
    severity: Severity,
    /// Extension filter: None = all files, Some = only these extensions
    extensions: Option<&'static [&'static str]>,
    /// Pattern to search for (substring match, case-insensitive)
    pattern: &'static str,
    /// If Some, the line must NOT contain this to avoid false positives
    exclude_if: Option<&'static str>,
    message: &'static str,
}

static RULES: &[Rule] = &[
    // ── SQL Injection ────────────────────────────────────────────────────────
    Rule {
        id: "SEC001",
        category: || Category::SqlInjection,
        severity: Severity::High,
        extensions: Some(&["py", "js", "ts", "java", "go", "rb", "php", "cs", "rs"]),
        pattern: "execute(",
        exclude_if: Some("# nosec"),
        message: "Possible SQL injection: raw string passed to execute(). Use parameterized queries.",
    },
    Rule {
        id: "SEC002",
        category: || Category::SqlInjection,
        severity: Severity::High,
        extensions: Some(&["py", "js", "ts", "java", "go", "rb", "php", "cs", "rs"]),
        pattern: "raw_query(",
        exclude_if: None,
        message: "raw_query() call — ensure parameters are not interpolated from user input.",
    },
    Rule {
        id: "SEC003",
        category: || Category::SqlInjection,
        severity: Severity::Critical,
        extensions: Some(&["py", "js", "ts", "java", "go", "rb", "php", "cs"]),
        pattern: "format!(\"select",
        exclude_if: None,
        message: "String-interpolated SQL SELECT — classic SQL injection risk.",
    },
    Rule {
        id: "SEC004",
        category: || Category::SqlInjection,
        severity: Severity::Critical,
        extensions: Some(&["py", "js", "ts", "java", "go", "rb", "php"]),
        pattern: "f\"select",
        exclude_if: None,
        message: "f-string SQL SELECT — SQL injection risk.",
    },
    Rule {
        id: "SEC005",
        category: || Category::SqlInjection,
        severity: Severity::Critical,
        extensions: Some(&["py", "js", "ts", "java", "go", "rb", "php"]),
        pattern: "f'select",
        exclude_if: None,
        message: "f-string SQL SELECT — SQL injection risk.",
    },
    // ── Hardcoded Secrets ────────────────────────────────────────────────────
    Rule {
        id: "SEC010",
        category: || Category::HardcodedSecret,
        severity: Severity::Critical,
        extensions: None,
        pattern: "password = \"",
        exclude_if: Some("example"),
        message: "Hardcoded password literal.",
    },
    Rule {
        id: "SEC011",
        category: || Category::HardcodedSecret,
        severity: Severity::Critical,
        extensions: None,
        pattern: "password = '",
        exclude_if: Some("example"),
        message: "Hardcoded password literal.",
    },
    Rule {
        id: "SEC012",
        category: || Category::HardcodedSecret,
        severity: Severity::Critical,
        extensions: None,
        pattern: "secret_key = \"",
        exclude_if: None,
        message: "Hardcoded secret key.",
    },
    Rule {
        id: "SEC013",
        category: || Category::HardcodedSecret,
        severity: Severity::Critical,
        extensions: None,
        pattern: "api_key = \"",
        exclude_if: Some("os."),
        message: "Hardcoded API key.",
    },
    Rule {
        id: "SEC014",
        category: || Category::HardcodedSecret,
        severity: Severity::High,
        extensions: None,
        pattern: "aws_secret_access_key",
        exclude_if: Some("os.environ"),
        message: "AWS secret access key reference — ensure not hardcoded.",
    },
    Rule {
        id: "SEC015",
        category: || Category::HardcodedSecret,
        severity: Severity::High,
        extensions: Some(&["py", "js", "ts", "go", "java", "rb", "cs", "rs"]),
        pattern: "private_key = \"",
        exclude_if: None,
        message: "Hardcoded private key.",
    },
    Rule {
        id: "SEC016",
        category: || Category::HardcodedSecret,
        severity: Severity::High,
        extensions: None,
        pattern: "-----begin rsa private key-----",
        exclude_if: None,
        message: "RSA private key literal in source code.",
    },
    Rule {
        id: "SEC017",
        category: || Category::HardcodedSecret,
        severity: Severity::High,
        extensions: None,
        pattern: "-----begin ec private key-----",
        exclude_if: None,
        message: "EC private key literal in source code.",
    },
    // ── Dangerous Eval ───────────────────────────────────────────────────────
    Rule {
        id: "SEC020",
        category: || Category::DangerousEval,
        severity: Severity::High,
        extensions: Some(&["py"]),
        pattern: "eval(",
        exclude_if: Some("#"),
        message: "eval() with dynamic input is dangerous — possible code injection.",
    },
    Rule {
        id: "SEC021",
        category: || Category::DangerousEval,
        severity: Severity::High,
        extensions: Some(&["js", "ts"]),
        pattern: "eval(",
        exclude_if: None,
        message: "JavaScript eval() — code injection risk.",
    },
    Rule {
        id: "SEC022",
        category: || Category::DangerousEval,
        severity: Severity::Medium,
        extensions: Some(&["py"]),
        pattern: "exec(",
        exclude_if: Some("#"),
        message: "Python exec() — code injection risk if input is not sanitized.",
    },
    // ── Insecure Deserialization ──────────────────────────────────────────────
    Rule {
        id: "SEC030",
        category: || Category::InsecureDeserialization,
        severity: Severity::Critical,
        extensions: Some(&["py"]),
        pattern: "pickle.loads(",
        exclude_if: None,
        message: "pickle.loads() on untrusted data allows arbitrary code execution.",
    },
    Rule {
        id: "SEC031",
        category: || Category::InsecureDeserialization,
        severity: Severity::Critical,
        extensions: Some(&["py"]),
        pattern: "pickle.load(",
        exclude_if: None,
        message: "pickle.load() on untrusted data allows arbitrary code execution.",
    },
    Rule {
        id: "SEC032",
        category: || Category::InsecureDeserialization,
        severity: Severity::High,
        extensions: Some(&["py"]),
        pattern: "yaml.load(",
        exclude_if: Some("loader=yaml.SafeLoader"),
        message: "yaml.load() without SafeLoader — use yaml.safe_load() instead.",
    },
    Rule {
        id: "SEC033",
        category: || Category::InsecureDeserialization,
        severity: Severity::High,
        extensions: Some(&["java"]),
        pattern: "objectinputstream",
        exclude_if: None,
        message: "Java ObjectInputStream deserialization — gadget chain risk.",
    },
    Rule {
        id: "SEC034",
        category: || Category::InsecureDeserialization,
        severity: Severity::High,
        extensions: Some(&["rb"]),
        pattern: "marshal.load(",
        exclude_if: None,
        message: "Ruby Marshal.load on untrusted data — code execution risk.",
    },
    // ── Path Traversal ───────────────────────────────────────────────────────
    Rule {
        id: "SEC040",
        category: || Category::PathTraversal,
        severity: Severity::High,
        extensions: Some(&["py", "js", "ts", "go", "java", "rb", "php", "cs", "rs"]),
        pattern: "../",
        exclude_if: Some("test"),
        message: "Literal '../' in path construction — possible path traversal.",
    },
    Rule {
        id: "SEC041",
        category: || Category::PathTraversal,
        severity: Severity::Medium,
        extensions: Some(&["py"]),
        pattern: "open(request.",
        exclude_if: None,
        message: "File open with request parameter — path traversal risk.",
    },
    // ── SSRF ─────────────────────────────────────────────────────────────────
    Rule {
        id: "SEC050",
        category: || Category::Ssrf,
        severity: Severity::High,
        extensions: Some(&["py"]),
        pattern: "requests.get(request.",
        exclude_if: None,
        message: "HTTP GET with user-controlled URL — SSRF risk.",
    },
    Rule {
        id: "SEC051",
        category: || Category::Ssrf,
        severity: Severity::High,
        extensions: Some(&["py"]),
        pattern: "requests.post(request.",
        exclude_if: None,
        message: "HTTP POST with user-controlled URL — SSRF risk.",
    },
    Rule {
        id: "SEC052",
        category: || Category::Ssrf,
        severity: Severity::Medium,
        extensions: Some(&["js", "ts"]),
        pattern: "fetch(req.",
        exclude_if: None,
        message: "fetch() with request-derived URL — SSRF risk.",
    },
    // ── XXE ──────────────────────────────────────────────────────────────────
    Rule {
        id: "SEC060",
        category: || Category::Xxe,
        severity: Severity::High,
        extensions: Some(&["py"]),
        pattern: "etree.parse(",
        exclude_if: Some("defusedxml"),
        message: "xml.etree.parse() — XXE risk. Use defusedxml.",
    },
    Rule {
        id: "SEC061",
        category: || Category::Xxe,
        severity: Severity::High,
        extensions: Some(&["java"]),
        pattern: "documentbuilderfactory.newinstance()",
        exclude_if: Some("setfeature"),
        message: "DocumentBuilderFactory without XXE protection.",
    },
    // ── Weak Crypto ───────────────────────────────────────────────────────────
    Rule {
        id: "SEC070",
        category: || Category::WeakCrypto,
        severity: Severity::Medium,
        extensions: None,
        pattern: "md5(",
        exclude_if: Some("test"),
        message: "MD5 is cryptographically broken. Use SHA-256 or better.",
    },
    Rule {
        id: "SEC071",
        category: || Category::WeakCrypto,
        severity: Severity::Medium,
        extensions: None,
        pattern: "sha1(",
        exclude_if: Some("test"),
        message: "SHA-1 is cryptographically weak. Use SHA-256 or better.",
    },
    Rule {
        id: "SEC072",
        category: || Category::WeakCrypto,
        severity: Severity::High,
        extensions: None,
        pattern: "des(",
        exclude_if: None,
        message: "DES is broken. Use AES-256.",
    },
    Rule {
        id: "SEC073",
        category: || Category::WeakCrypto,
        severity: Severity::Medium,
        extensions: Some(&["py", "js", "ts", "go", "java", "rb", "rs"]),
        pattern: "hashlib.md5(",
        exclude_if: None,
        message: "hashlib.md5 — not suitable for security-sensitive hashing.",
    },
    // ── Command Injection ─────────────────────────────────────────────────────
    Rule {
        id: "SEC080",
        category: || Category::CommandInjection,
        severity: Severity::Critical,
        extensions: Some(&["py"]),
        pattern: "os.system(",
        exclude_if: None,
        message: "os.system() with dynamic input — command injection risk.",
    },
    Rule {
        id: "SEC081",
        category: || Category::CommandInjection,
        severity: Severity::High,
        extensions: Some(&["py"]),
        pattern: "subprocess.call(",
        exclude_if: Some("shell=False"),
        message: "subprocess.call() — use shell=False and list arguments.",
    },
    Rule {
        id: "SEC082",
        category: || Category::CommandInjection,
        severity: Severity::High,
        extensions: Some(&["py"]),
        pattern: "subprocess.popen(",
        exclude_if: Some("shell=false"),
        message: "subprocess.Popen() — use shell=False and list arguments.",
    },
    Rule {
        id: "SEC083",
        category: || Category::CommandInjection,
        severity: Severity::High,
        extensions: Some(&["js", "ts"]),
        pattern: "exec(",
        exclude_if: Some("test"),
        message: "child_process.exec() with dynamic input — command injection risk.",
    },
    Rule {
        id: "SEC084",
        category: || Category::CommandInjection,
        severity: Severity::High,
        extensions: Some(&["go"]),
        pattern: "exec.command(",
        exclude_if: None,
        message: "exec.Command with user-controlled args — verify input is sanitized.",
    },
    // ── Insecure Random ───────────────────────────────────────────────────────
    Rule {
        id: "SEC090",
        category: || Category::InsecureRandom,
        severity: Severity::Medium,
        extensions: Some(&["py"]),
        pattern: "random.random(",
        exclude_if: None,
        message: "random.random() is not cryptographically secure. Use secrets module.",
    },
    Rule {
        id: "SEC091",
        category: || Category::InsecureRandom,
        severity: Severity::Medium,
        extensions: Some(&["py"]),
        pattern: "random.randint(",
        exclude_if: None,
        message: "random.randint() is not cryptographically secure. Use secrets.randbelow().",
    },
    Rule {
        id: "SEC092",
        category: || Category::InsecureRandom,
        severity: Severity::Medium,
        extensions: Some(&["js", "ts"]),
        pattern: "math.random()",
        exclude_if: None,
        message: "Math.random() is not cryptographically secure. Use crypto.getRandomValues().",
    },
    // ── XSS Risk ─────────────────────────────────────────────────────────────
    Rule {
        id: "SEC100",
        category: || Category::XssRisk,
        severity: Severity::High,
        extensions: Some(&["js", "ts"]),
        pattern: "innerhtml",
        exclude_if: None,
        message: "innerHTML assignment — XSS risk if content is user-controlled.",
    },
    Rule {
        id: "SEC101",
        category: || Category::XssRisk,
        severity: Severity::High,
        extensions: Some(&["js", "ts"]),
        pattern: "dangerouslysetinnerhtml",
        exclude_if: None,
        message: "React dangerouslySetInnerHTML — XSS risk.",
    },
    Rule {
        id: "SEC102",
        category: || Category::XssRisk,
        severity: Severity::Medium,
        extensions: Some(&["py"]),
        pattern: "mark_safe(",
        exclude_if: None,
        message: "Django mark_safe() — ensure content is sanitized before marking safe.",
    },
    // ── Open Redirect ─────────────────────────────────────────────────────────
    Rule {
        id: "SEC110",
        category: || Category::OpenRedirect,
        severity: Severity::Medium,
        extensions: Some(&["py", "js", "ts", "go", "java", "rb"]),
        pattern: "redirect(request.",
        exclude_if: None,
        message: "redirect() with user-supplied URL — open redirect risk.",
    },
];

// ---------------------------------------------------------------------------
// Scanner
// ---------------------------------------------------------------------------

/// Scan the project rooted at `root` for security issues.
///
/// Walks all non-vendor files and applies pattern-based rules.
pub fn scan_project(root: &Path) -> Result<ScanStats> {
    let mut stats = ScanStats::default();

    walk_and_scan(root, root, &mut stats)?;
    // Sort findings: Critical first, then High, etc.
    stats.findings.sort_by(|a, b| a.severity.cmp(&b.severity).then(a.file.cmp(&b.file)).then(a.line.cmp(&b.line)));

    Ok(stats)
}

static IGNORE_DIRS: &[&str] = &[
    ".git", "node_modules", ".venv", "venv", "target", "build", "dist",
    "__pycache__", ".tox", ".infigraph", "vendor", ".idea", ".mypy_cache",
    "coverage", ".pytest_cache",
];

fn walk_and_scan(root: &Path, dir: &Path, stats: &mut ScanStats) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if path.is_dir() {
            if !IGNORE_DIRS.contains(&name_str.as_ref()) && !name_str.starts_with('.') {
                walk_and_scan(root, &path, stats)?;
            }
        } else if path.is_file() {
            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().replace('\\', "/");
                scan_file(&path, &rel, ext, stats)?;
            }
        }
    }
    Ok(())
}

fn scan_file(path: &Path, rel_path: &str, ext: &str, stats: &mut ScanStats) -> Result<()> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Ok(()), // skip binary files
    };

    stats.files_scanned += 1;
    let ext_lower = ext.to_lowercase();

    for (line_no, line) in content.lines().enumerate() {
        let line_lower = line.to_lowercase();
        let line_no = (line_no + 1) as u32;

        for rule in RULES {
            // Extension filter
            if let Some(exts) = rule.extensions {
                if !exts.contains(&ext_lower.as_str()) {
                    continue;
                }
            }

            // Pattern match (case-insensitive)
            if !line_lower.contains(rule.pattern) {
                continue;
            }

            // Exclusion check (case-insensitive)
            if let Some(excl) = rule.exclude_if {
                if line_lower.contains(&excl.to_lowercase() as &str) {
                    continue;
                }
            }

            // Find column of match
            let col = line_lower.find(rule.pattern).unwrap_or(0) as u32 + 1;

            stats.findings.push(Finding {
                file: rel_path.to_string(),
                line: line_no,
                col,
                severity: rule.severity.clone(),
                category: (rule.category)(),
                rule_id: rule.id.to_string(),
                message: rule.message.to_string(),
                snippet: line.trim().chars().take(120).collect(),
            });
        }
    }

    Ok(())
}

/// Format scan results as a human-readable report.
pub fn format_scan_results(stats: &ScanStats) -> String {
    if stats.findings.is_empty() {
        return format!(
            "Security scan complete: {} files scanned, no issues found.",
            stats.files_scanned
        );
    }

    let mut out = format!(
        "Security scan: {} files, {} findings  [CRITICAL:{} HIGH:{} MEDIUM:{} LOW:{}]\n\n",
        stats.files_scanned,
        stats.findings.len(),
        stats.critical_count(),
        stats.high_count(),
        stats.medium_count(),
        stats.low_count(),
    );

    let mut cur_file = String::new();
    for f in &stats.findings {
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

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn scan_str(content: &str, ext: &str) -> Vec<Finding> {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(format!("test.{}", ext));
        let mut f = std::fs::File::create(&file).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        let mut stats = ScanStats::default();
        scan_file(&file, &format!("test.{}", ext), ext, &mut stats).unwrap();
        stats.findings
    }

    #[test]
    fn detects_pickle_loads() {
        let findings = scan_str("data = pickle.loads(user_input)", "py");
        assert!(findings.iter().any(|f| f.rule_id == "SEC030"));
    }

    #[test]
    fn detects_hardcoded_password() {
        let findings = scan_str("password = \"s3cr3t\"", "py");
        assert!(findings.iter().any(|f| f.rule_id == "SEC010"));
    }

    #[test]
    fn detects_eval_js() {
        let findings = scan_str("eval(userInput)", "js");
        assert!(findings.iter().any(|f| f.rule_id == "SEC021"));
    }

    #[test]
    fn detects_md5() {
        let findings = scan_str("digest = md5(password)", "py");
        assert!(findings.iter().any(|f| f.category == Category::WeakCrypto));
    }

    #[test]
    fn detects_innerhtml() {
        let findings = scan_str("el.innerHTML = userInput", "js");
        assert!(findings.iter().any(|f| f.rule_id == "SEC100"));
    }

    #[test]
    fn no_false_positive_yaml_safe() {
        let findings = scan_str("data = yaml.load(f, loader=yaml.SafeLoader)", "py");
        assert!(!findings.iter().any(|f| f.rule_id == "SEC032"));
    }
}
