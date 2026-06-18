use infigraph_core::routes::{format_routes, Route};
use infigraph_core::bridges::BridgeScanResult;
use infigraph_core::model::{Bridge, BridgeKind};
use infigraph_core::security::{scan_project, format_scan_results, Category, ScanStats, Finding, Severity};

// ==================== format_routes ====================

#[test]
fn test_format_routes_empty() {
    let out = format_routes(&[]);
    assert_eq!(out, "No HTTP routes detected.");
}

#[test]
fn test_format_routes_single() {
    let routes = vec![Route {
        method: "GET".to_string(),
        path: "/api/users".to_string(),
        handler_id: "app.py::get_users".to_string(),
        file: "app.py".to_string(),
        framework: "flask".to_string(),
    }];
    let out = format_routes(&routes);
    assert!(out.contains("1 HTTP route"), "got: {out}");
    assert!(out.contains("GET"));
    assert!(out.contains("/api/users"));
    assert!(out.contains("flask"));
    assert!(out.contains("app.py::get_users"));
}

#[test]
fn test_format_routes_multiple_files() {
    let routes = vec![
        Route {
            method: "GET".to_string(),
            path: "/users".to_string(),
            handler_id: "users.py::list".to_string(),
            file: "users.py".to_string(),
            framework: "flask".to_string(),
        },
        Route {
            method: "POST".to_string(),
            path: "/users".to_string(),
            handler_id: "users.py::create".to_string(),
            file: "users.py".to_string(),
            framework: "flask".to_string(),
        },
        Route {
            method: "GET".to_string(),
            path: "/orders".to_string(),
            handler_id: "orders.py::list".to_string(),
            file: "orders.py".to_string(),
            framework: "flask".to_string(),
        },
    ];
    let out = format_routes(&routes);
    assert!(out.contains("3 HTTP route"), "got: {out}");
    assert!(out.contains("users.py:"));
    assert!(out.contains("orders.py:"));
}

// ==================== BridgeScanResult::com_count ====================

#[test]
fn test_bridge_scan_com_count_empty() {
    let result = BridgeScanResult { bridges: vec![] };
    assert_eq!(result.com_count(), 0);
}

#[test]
fn test_bridge_scan_com_count_with_com() {
    let result = BridgeScanResult {
        bridges: vec![
            Bridge {
                file: "a.vb".to_string(),
                line: 10,
                kind: BridgeKind::Com,
                foreign_symbol: "Excel.Application".to_string(),
                source_language: "vb6".to_string(),
                target_language: None,
                detail: "COM interop".to_string(),
            },
            Bridge {
                file: "b.vb".to_string(),
                line: 20,
                kind: BridgeKind::Com,
                foreign_symbol: "Word.Document".to_string(),
                source_language: "vb6".to_string(),
                target_language: None,
                detail: "COM interop".to_string(),
            },
            Bridge {
                file: "c.rs".to_string(),
                line: 5,
                kind: BridgeKind::Ffi,
                foreign_symbol: "libc::malloc".to_string(),
                source_language: "rust".to_string(),
                target_language: Some("c".to_string()),
                detail: "extern C".to_string(),
            },
        ],
    };
    assert_eq!(result.com_count(), 2);
    assert_eq!(result.ffi_count(), 1);
}

#[test]
fn test_bridge_scan_by_kind() {
    let result = BridgeScanResult {
        bridges: vec![
            Bridge {
                file: "a.java".to_string(),
                line: 1,
                kind: BridgeKind::Jni,
                foreign_symbol: "native_func".to_string(),
                source_language: "java".to_string(),
                target_language: Some("c".to_string()),
                detail: "JNI".to_string(),
            },
        ],
    };
    let jni = result.by_kind(&BridgeKind::Jni);
    assert_eq!(jni.len(), 1);
    let ffi = result.by_kind(&BridgeKind::Ffi);
    assert_eq!(ffi.len(), 0);
}

// ==================== security: scan_project ====================

#[test]
fn test_scan_project_clean_dir() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("clean.py"), "def hello():\n    print('hi')\n").unwrap();
    let stats = scan_project(tmp.path()).unwrap();
    assert_eq!(stats.findings.len(), 0);
    assert!(stats.files_scanned >= 1);
}

#[test]
fn test_scan_project_hardcoded_password() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("bad.py"),
        "password = \"s3cret123\"\ndb_password = \"hunter2\"\n",
    ).unwrap();
    let stats = scan_project(tmp.path()).unwrap();
    assert!(!stats.findings.is_empty(), "should detect hardcoded password");
}

#[test]
fn test_scan_project_sql_injection() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("query.py"),
        "def run(user_input):\n    query = \"SELECT * FROM users WHERE id = \" + user_input\n    cursor.execute(query)\n",
    ).unwrap();
    let stats = scan_project(tmp.path()).unwrap();
    assert!(!stats.findings.is_empty(), "should detect SQL injection pattern");
}

// ==================== security: format_scan_results ====================

#[test]
fn test_format_scan_results_empty() {
    let stats = ScanStats {
        files_scanned: 5,
        findings: vec![],
    };
    let out = format_scan_results(&stats);
    assert!(out.contains("no issues found"), "got: {out}");
    assert!(out.contains("5 files"));
}

#[test]
fn test_format_scan_results_with_findings() {
    let stats = ScanStats {
        files_scanned: 3,
        findings: vec![Finding {
            file: "bad.py".to_string(),
            line: 1,
            col: 0,
            severity: Severity::High,
            category: Category::HardcodedSecret,
            rule_id: "SEC001".to_string(),
            message: "Hardcoded password".to_string(),
            snippet: "password = \"s3cret\"".to_string(),
            suppressed: false,
            sanitizer_hint: None,
        }],
    };
    let out = format_scan_results(&stats);
    assert!(out.contains("HIGH"), "got: {out}");
    assert!(out.contains("bad.py"));
    assert!(out.contains("SEC001"));
    assert!(out.contains("Hardcoded password"));
}

// ==================== security: count methods ====================

#[test]
fn test_scan_stats_count_methods() {
    let stats = ScanStats {
        files_scanned: 10,
        findings: vec![
            Finding {
                file: "a.py".to_string(), line: 1, col: 0,
                severity: Severity::Critical,
                category: Category::HardcodedSecret,
                rule_id: "S1".to_string(), message: "".to_string(), snippet: "".to_string(),
                suppressed: false, sanitizer_hint: None,
            },
            Finding {
                file: "b.py".to_string(), line: 2, col: 0,
                severity: Severity::High,
                category: Category::HardcodedSecret,
                rule_id: "S2".to_string(), message: "".to_string(), snippet: "".to_string(),
                suppressed: false, sanitizer_hint: None,
            },
            Finding {
                file: "c.py".to_string(), line: 3, col: 0,
                severity: Severity::Medium,
                category: Category::SqlInjection,
                rule_id: "S3".to_string(), message: "".to_string(), snippet: "".to_string(),
                suppressed: false, sanitizer_hint: None,
            },
            Finding {
                file: "d.py".to_string(), line: 4, col: 0,
                severity: Severity::Low,
                category: Category::SqlInjection,
                rule_id: "S4".to_string(), message: "".to_string(), snippet: "".to_string(),
                suppressed: false, sanitizer_hint: None,
            },
        ],
    };
    assert_eq!(stats.critical_count(), 1);
    assert_eq!(stats.high_count(), 1);
    assert_eq!(stats.medium_count(), 1);
    assert_eq!(stats.low_count(), 1);
}
