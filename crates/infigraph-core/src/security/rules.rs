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
    pub suppressed: bool,
    pub sanitizer_hint: Option<String>,
}

/// Summary stats from a security scan.
#[derive(Debug, Default)]
pub struct ScanStats {
    pub files_scanned: usize,
    pub findings: Vec<Finding>,
}

impl ScanStats {
    pub fn critical_count(&self) -> usize {
        self.findings
            .iter()
            .filter(|f| f.severity == Severity::Critical)
            .count()
    }
    pub fn high_count(&self) -> usize {
        self.findings
            .iter()
            .filter(|f| f.severity == Severity::High)
            .count()
    }
    pub fn medium_count(&self) -> usize {
        self.findings
            .iter()
            .filter(|f| f.severity == Severity::Medium)
            .count()
    }
    pub fn low_count(&self) -> usize {
        self.findings
            .iter()
            .filter(|f| f.severity == Severity::Low)
            .count()
    }
}

// ---------------------------------------------------------------------------
// Rule definitions
// ---------------------------------------------------------------------------

pub(crate) struct Rule {
    pub(crate) id: &'static str,
    pub(crate) category: fn() -> Category,
    pub(crate) severity: Severity,
    /// Extension filter: None = all files, Some = only these extensions
    pub(crate) extensions: Option<&'static [&'static str]>,
    /// Pattern to search for (substring match, case-insensitive)
    pub(crate) pattern: &'static str,
    /// If Some, the line must NOT contain this to avoid false positives
    pub(crate) exclude_if: Option<&'static str>,
    pub(crate) message: &'static str,
}

pub(crate) static RULES: &[Rule] = &[
    // ── SQL Injection ────────────────────────────────────────────────────────
    Rule {
        id: "SEC001",
        category: || Category::SqlInjection,
        severity: Severity::High,
        extensions: Some(&["py", "js", "ts", "java", "go", "rb", "php", "cs", "rs"]),
        pattern: "execute(",
        exclude_if: Some("# nosec"),
        message:
            "Possible SQL injection: raw string passed to execute(). Use parameterized queries.",
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
        pattern: "des.new(",
        exclude_if: Some("test"),
        message: "DES is broken. Use AES-256.",
    },
    Rule {
        id: "SEC072b",
        category: || Category::WeakCrypto,
        severity: Severity::High,
        extensions: None,
        pattern: "des_cbc",
        exclude_if: Some("test"),
        message: "DES/3DES is broken. Use AES-256.",
    },
    Rule {
        id: "SEC072c",
        category: || Category::WeakCrypto,
        severity: Severity::High,
        extensions: None,
        pattern: "des_ede",
        exclude_if: Some("test"),
        message: "Triple-DES (DES-EDE) is deprecated. Use AES-256.",
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
// Sanitizer definitions — suppress findings when nearby code sanitizes input
// ---------------------------------------------------------------------------

pub(crate) struct Sanitizer {
    pub(crate) category: fn() -> Category,
    pub(crate) patterns: &'static [&'static str],
}

pub(crate) static SANITIZERS: &[Sanitizer] = &[
    Sanitizer {
        category: || Category::SqlInjection,
        patterns: &[
            "parameterize",
            "prepare(",
            "bind_param",
            "sanitize_sql",
            "sqlalchemy.text(",
            "prepared_statement",
            "placeholders",
            "cursor.execute(%s",
            "cursor.execute(?,",
            "?)",
        ],
    },
    Sanitizer {
        category: || Category::XssRisk,
        patterns: &[
            "escape_html",
            "sanitize(",
            "dompurify",
            "bleach.clean(",
            "html.escape(",
            "encodeuricomponent(",
            "cgi.escape(",
            "markupsafe.escape(",
            "xss_clean(",
        ],
    },
    Sanitizer {
        category: || Category::CommandInjection,
        patterns: &[
            "shlex.quote(",
            "shell_escape",
            "escapeshellarg(",
            "escapeshellcmd(",
            "shell=false",
            "shlex.split(",
        ],
    },
    Sanitizer {
        category: || Category::PathTraversal,
        patterns: &[
            "realpath(",
            "abspath(",
            "normalize(",
            "canonicalize(",
            "path.resolve(",
            "secure_filename(",
            "os.path.basename(",
        ],
    },
    Sanitizer {
        category: || Category::Ssrf,
        patterns: &[
            "validate_url(",
            "is_allowed_host(",
            "urlparse(",
            "allowed_hosts",
            "url_validator(",
            "safelist",
        ],
    },
    Sanitizer {
        category: || Category::OpenRedirect,
        patterns: &[
            "url_has_allowed_host(",
            "is_safe_url(",
            "validate_redirect(",
            "allowed_hosts",
            "safe_redirect(",
        ],
    },
    Sanitizer {
        category: || Category::InsecureDeserialization,
        patterns: &[
            "safe_load(",
            "yaml.safe_load(",
            "json.loads(",
            "allowlist",
            "whitelist_classes",
        ],
    },
];

pub(crate) const SANITIZER_WINDOW: usize = 5;

pub(crate) fn find_sanitizer_for(
    category: &Category,
    lines: &[&str],
    finding_line: usize,
) -> Option<String> {
    let start = finding_line.saturating_sub(SANITIZER_WINDOW);
    let end = (finding_line + SANITIZER_WINDOW + 1).min(lines.len());

    for sanitizer in SANITIZERS {
        if (sanitizer.category)() != *category {
            continue;
        }
        for &line in &lines[start..end] {
            let lower = line.to_lowercase();
            for &pat in sanitizer.patterns {
                if lower.contains(pat) {
                    return Some(pat.to_string());
                }
            }
        }
    }
    None
}
