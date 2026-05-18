use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::model::{Bridge, BridgeKind};

/// Aggregated result of a bridge scan.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct BridgeScanResult {
    pub bridges: Vec<Bridge>,
}

impl BridgeScanResult {
    pub fn by_kind(&self, kind: &BridgeKind) -> Vec<&Bridge> {
        self.bridges.iter().filter(|b| &b.kind == kind).collect()
    }

    pub fn ffi_count(&self) -> usize {
        self.bridges.iter().filter(|b| b.kind == BridgeKind::Ffi).count()
    }
    pub fn jni_count(&self) -> usize {
        self.bridges.iter().filter(|b| b.kind == BridgeKind::Jni).count()
    }
    pub fn grpc_count(&self) -> usize {
        self.bridges.iter().filter(|b| b.kind == BridgeKind::Grpc).count()
    }
    pub fn pinvoke_count(&self) -> usize {
        self.bridges.iter().filter(|b| b.kind == BridgeKind::PInvoke).count()
    }
    pub fn com_count(&self) -> usize {
        self.bridges.iter().filter(|b| b.kind == BridgeKind::Com).count()
    }
}

// ---------------------------------------------------------------------------
// Pattern rules
// ---------------------------------------------------------------------------

struct BridgeRule {
    kind: fn() -> BridgeKind,
    /// File extensions this rule applies to (empty = all)
    extensions: &'static [&'static str],
    /// Substring that must appear on the line (case-sensitive)
    pattern: &'static str,
    /// Source language label
    source_language: &'static str,
    /// Target language (None = unknown)
    target_language: Option<&'static str>,
    /// How to extract the foreign symbol name from the matching line
    extract: fn(&str) -> String,
}

fn extract_after_last_space(line: &str) -> String {
    line.trim()
        .split_whitespace()
        .last()
        .unwrap_or("")
        .trim_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != ':')
        .to_string()
}

fn extract_quoted(line: &str) -> String {
    // Grab first double-quoted string
    if let Some(start) = line.find('"') {
        if let Some(end) = line[start + 1..].find('"') {
            return line[start + 1..start + 1 + end].to_string();
        }
    }
    line.trim().to_string()
}

fn extract_vb6_lib(line: &str) -> String {
    let lower = line.to_ascii_lowercase();
    if let Some(idx) = lower.find(" lib ") {
        let after = line[idx + 5..].trim_start();
        return extract_quoted(after);
    }
    extract_after_last_space(line)
}

fn extract_paren_arg(line: &str) -> String {
    // Grab first argument inside parentheses (before first comma or close paren)
    if let Some(open) = line.find('(') {
        let after = line[open + 1..].trim_start();
        let end = after
            .find(|c| c == ',' || c == ')')
            .unwrap_or(after.len());
        return after[..end]
            .trim()
            .trim_matches('"')
            .to_string();
    }
    line.trim().to_string()
}

static BRIDGE_RULES: &[BridgeRule] = &[
    // ── Rust FFI ──────────────────────────────────────────────────────────────
    BridgeRule {
        kind: || BridgeKind::Ffi,
        extensions: &["rs"],
        pattern: "extern \"C\"",
        source_language: "rust",
        target_language: Some("c"),
        extract: extract_after_last_space,
    },
    BridgeRule {
        kind: || BridgeKind::Ffi,
        extensions: &["rs"],
        pattern: "#[no_mangle]",
        source_language: "rust",
        target_language: Some("c"),
        extract: |_| "exported_symbol".to_string(),
    },
    BridgeRule {
        kind: || BridgeKind::Ffi,
        extensions: &["rs"],
        pattern: "extern \"system\"",
        source_language: "rust",
        target_language: Some("c"),
        extract: extract_after_last_space,
    },
    // ── C/C++ FFI ─────────────────────────────────────────────────────────────
    BridgeRule {
        kind: || BridgeKind::Ffi,
        extensions: &["c", "h", "cpp", "hpp", "cc"],
        pattern: "JNIEXPORT",
        source_language: "c",
        target_language: Some("java"),
        extract: extract_after_last_space,
    },
    BridgeRule {
        kind: || BridgeKind::Jni,
        extensions: &["c", "h", "cpp", "hpp", "cc"],
        pattern: "JNIEnv",
        source_language: "c",
        target_language: Some("java"),
        extract: extract_after_last_space,
    },
    // ── Java JNI ──────────────────────────────────────────────────────────────
    BridgeRule {
        kind: || BridgeKind::Jni,
        extensions: &["java"],
        pattern: "native ",
        source_language: "java",
        target_language: Some("c"),
        extract: extract_after_last_space,
    },
    BridgeRule {
        kind: || BridgeKind::Jni,
        extensions: &["java"],
        pattern: "System.loadLibrary(",
        source_language: "java",
        target_language: Some("c"),
        extract: extract_paren_arg,
    },
    // ── Go cgo ───────────────────────────────────────────────────────────────
    BridgeRule {
        kind: || BridgeKind::Cgo,
        extensions: &["go"],
        pattern: "import \"C\"",
        source_language: "go",
        target_language: Some("c"),
        extract: |_| "C".to_string(),
    },
    BridgeRule {
        kind: || BridgeKind::Cgo,
        extensions: &["go"],
        pattern: "C.",
        source_language: "go",
        target_language: Some("c"),
        extract: |line| {
            // Extract C.FunctionName
            if let Some(idx) = line.find("C.") {
                let after = &line[idx + 2..];
                let end = after
                    .find(|c: char| !c.is_alphanumeric() && c != '_')
                    .unwrap_or(after.len());
                return format!("C.{}", &after[..end]);
            }
            "C.unknown".to_string()
        },
    },
    // ── .NET P/Invoke ─────────────────────────────────────────────────────────
    BridgeRule {
        kind: || BridgeKind::PInvoke,
        extensions: &["cs"],
        pattern: "[DllImport(",
        source_language: "csharp",
        target_language: Some("c"),
        extract: extract_paren_arg,
    },
    BridgeRule {
        kind: || BridgeKind::PInvoke,
        extensions: &["cs"],
        pattern: "DllImport(",
        source_language: "csharp",
        target_language: Some("c"),
        extract: extract_paren_arg,
    },
    // ── Python ctypes / cffi ──────────────────────────────────────────────────
    BridgeRule {
        kind: || BridgeKind::Ctypes,
        extensions: &["py"],
        pattern: "ctypes.CDLL(",
        source_language: "python",
        target_language: Some("c"),
        extract: extract_paren_arg,
    },
    BridgeRule {
        kind: || BridgeKind::Ctypes,
        extensions: &["py"],
        pattern: "ctypes.cdll.LoadLibrary(",
        source_language: "python",
        target_language: Some("c"),
        extract: extract_paren_arg,
    },
    BridgeRule {
        kind: || BridgeKind::Ctypes,
        extensions: &["py"],
        pattern: "ffi.cdef(",
        source_language: "python",
        target_language: Some("c"),
        extract: |_| "cffi_binding".to_string(),
    },
    // ── gRPC ─────────────────────────────────────────────────────────────────
    BridgeRule {
        kind: || BridgeKind::Grpc,
        extensions: &["proto"],
        pattern: "service ",
        source_language: "protobuf",
        target_language: None,
        extract: |line| {
            line.trim()
                .strip_prefix("service ")
                .map(|s| s.split_whitespace().next().unwrap_or("").to_string())
                .unwrap_or_default()
        },
    },
    BridgeRule {
        kind: || BridgeKind::Grpc,
        extensions: &["py", "js", "ts", "go", "java", "rb", "cs", "rs"],
        pattern: "_pb2_grpc",
        source_language: "python",
        target_language: None,
        extract: extract_after_last_space,
    },
    BridgeRule {
        kind: || BridgeKind::Grpc,
        extensions: &["go"],
        pattern: "grpc.Dial(",
        source_language: "go",
        target_language: None,
        extract: extract_paren_arg,
    },
    BridgeRule {
        kind: || BridgeKind::Grpc,
        extensions: &["java"],
        pattern: "ManagedChannelBuilder",
        source_language: "java",
        target_language: None,
        extract: extract_after_last_space,
    },
    BridgeRule {
        kind: || BridgeKind::Grpc,
        extensions: &["rs"],
        pattern: "tonic::transport::Channel",
        source_language: "rust",
        target_language: None,
        extract: extract_after_last_space,
    },
    // ── WASM ─────────────────────────────────────────────────────────────────
    BridgeRule {
        kind: || BridgeKind::Wasm,
        extensions: &["rs"],
        pattern: "#[wasm_bindgen]",
        source_language: "rust",
        target_language: Some("javascript"),
        extract: |_| "wasm_export".to_string(),
    },
    BridgeRule {
        kind: || BridgeKind::Wasm,
        extensions: &["rs"],
        pattern: "wasm_bindgen::JsValue",
        source_language: "rust",
        target_language: Some("javascript"),
        extract: extract_after_last_space,
    },
    BridgeRule {
        kind: || BridgeKind::Wasm,
        extensions: &["js", "ts"],
        pattern: "WebAssembly.instantiate(",
        source_language: "javascript",
        target_language: Some("wasm"),
        extract: extract_paren_arg,
    },
    BridgeRule {
        kind: || BridgeKind::Wasm,
        extensions: &["js", "ts"],
        pattern: "WebAssembly.instantiateStreaming(",
        source_language: "javascript",
        target_language: Some("wasm"),
        extract: extract_paren_arg,
    },
    // ── VB6 DLL / COM ───────────────────────────────────────────────────────
    BridgeRule {
        kind: || BridgeKind::Ffi,
        extensions: &["bas", "cls", "frm", "ctl"],
        pattern: "Declare Function",
        source_language: "vb6",
        target_language: Some("c"),
        extract: extract_vb6_lib,
    },
    BridgeRule {
        kind: || BridgeKind::Ffi,
        extensions: &["bas", "cls", "frm", "ctl"],
        pattern: "Declare Sub",
        source_language: "vb6",
        target_language: Some("c"),
        extract: extract_vb6_lib,
    },
    BridgeRule {
        kind: || BridgeKind::Com,
        extensions: &["bas", "cls", "frm", "ctl"],
        pattern: "CreateObject(",
        source_language: "vb6",
        target_language: Some("com"),
        extract: extract_paren_arg,
    },
];

// ---------------------------------------------------------------------------
// Scanner
// ---------------------------------------------------------------------------

/// Scan a project directory for cross-language bridge points.
pub fn detect_bridges(root: &Path) -> Result<BridgeScanResult> {
    let mut result = BridgeScanResult::default();
    scan_dir(root, root, &mut result)?;
    Ok(result)
}

fn scan_dir(root: &Path, dir: &Path, result: &mut BridgeScanResult) -> Result<()> {
    const SKIP_DIRS: &[&str] = &[
        ".git", ".infigraph", "node_modules", "__pycache__", "target", "build", "dist",
    ];
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if path.is_dir() {
            if !SKIP_DIRS.contains(&name_str.as_ref()) && !name_str.starts_with('.') {
                scan_dir(root, &path, result)?;
            }
        } else if path.is_file() {
            scan_file(root, &path, result);
        }
    }
    Ok(())
}

fn scan_file(root: &Path, path: &Path, result: &mut BridgeScanResult) {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    let rel = path.strip_prefix(root).unwrap_or(path).to_string_lossy().replace('\\', "/");

    // Only scan rules whose extensions match
    let applicable: Vec<&BridgeRule> = BRIDGE_RULES
        .iter()
        .filter(|r| r.extensions.is_empty() || r.extensions.contains(&ext))
        .collect();

    if applicable.is_empty() {
        return;
    }

    let Ok(source) = std::fs::read_to_string(path) else { return };

    for (line_no, line) in source.lines().enumerate() {
        for rule in &applicable {
            if line.contains(rule.pattern) {
                let foreign_symbol = (rule.extract)(line);
                result.bridges.push(Bridge {
                    file: rel.clone(),
                    line: line_no as u32 + 1,
                    kind: (rule.kind)(),
                    foreign_symbol,
                    source_language: rule.source_language.to_string(),
                    target_language: rule.target_language.map(str::to_string),
                    detail: line.trim().to_string(),
                });
            }
        }
    }
}
