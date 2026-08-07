use infigraph_core::bridges::BridgeScanResult;
use infigraph_core::model::{Bridge, BridgeKind};
use infigraph_core::routes::{detect_routes_from_rows, format_routes, Route};
use infigraph_core::security::{
    format_scan_results, scan_project, Category, Finding, ScanStats, Severity,
};

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
        bridges: vec![Bridge {
            file: "a.java".to_string(),
            line: 1,
            kind: BridgeKind::Jni,
            foreign_symbol: "native_func".to_string(),
            source_language: "java".to_string(),
            target_language: Some("c".to_string()),
            detail: "JNI".to_string(),
        }],
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
    std::fs::write(
        tmp.path().join("clean.py"),
        "def hello():\n    print('hi')\n",
    )
    .unwrap();
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
    )
    .unwrap();
    let stats = scan_project(tmp.path()).unwrap();
    assert!(
        !stats.findings.is_empty(),
        "should detect hardcoded password"
    );
}

#[test]
fn test_scan_project_sql_injection() {
    let tmp = tempfile::tempdir().unwrap();
    // Inline injection on the execute() line itself -- AIF3X-331 #17 made
    // SEC001 require a same-line SQL keyword (see
    // test_scan_project_http_client_execute_is_not_sql_injection for why),
    // so the fixture must carry real SQL evidence on the execute() line
    // rather than relying on a bare variable built on a previous line.
    std::fs::write(
        tmp.path().join("query.py"),
        "def run(user_input):\n    cursor.execute(\"SELECT * FROM users WHERE id = \" + user_input)\n",
    ).unwrap();
    let stats = scan_project(tmp.path()).unwrap();
    assert!(
        !stats.findings.is_empty(),
        "should detect SQL injection pattern"
    );
    assert!(
        stats.findings.iter().any(|f| f.rule_id == "SEC001"),
        "should be SEC001, got: {:?}",
        stats.findings
    );
}

/// AIF3X-331 #17: async HTTP client .execute() calls were being flagged as
/// SQL injection (SEC001) purely because of the bare `execute(` substring
/// match -- report cited this across AWS/Azure/GCP/SRF/ORCA clients.
#[test]
fn test_scan_project_http_client_execute_is_not_sql_injection() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("client.py"),
        "async def call_service(self, request):\n    return await self.http_client.execute(request)\n",
    )
    .unwrap();
    let stats = scan_project(tmp.path()).unwrap();
    assert!(
        !stats.findings.iter().any(|f| f.rule_id == "SEC001"),
        "http_client.execute() should not be flagged as SQL injection, got: {:?}",
        stats.findings
    );
}

/// AIF3X-331 #17: same fix must not create a false negative for the common
/// SQLAlchemy pattern (`session.execute(text("SELECT ..."))`), which was the
/// reason a receiver-name denylist was rejected in favor of requiring
/// same-line SQL evidence instead.
#[test]
fn test_scan_project_sqlalchemy_execute_still_flagged() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("query.py"),
        "def run(user_input):\n    session.execute(text(\"SELECT * FROM users WHERE id = \" + user_input))\n",
    )
    .unwrap();
    let stats = scan_project(tmp.path()).unwrap();
    assert!(
        stats.findings.iter().any(|f| f.rule_id == "SEC001"),
        "session.execute(text(\"SELECT ...\")) should still be flagged, got: {:?}",
        stats.findings
    );
}

/// AIF3X-331 #17 word-boundary regression: a bare substring match on the
/// SQL-keyword requirement would treat "delete" inside "delete_resource" (or
/// "from" inside "platform"/"transform") as SQL evidence, refiring the exact
/// false positive this fix removes. Word-boundary matching (not
/// `.contains()`) is required to actually distinguish these.
#[test]
fn test_scan_project_http_client_execute_same_line_crud_name_not_flagged() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("client.py"),
        "async def call(self, request):\n    return await self.http_client.execute(self.delete_resource_url(request))\n",
    )
    .unwrap();
    let stats = scan_project(tmp.path()).unwrap();
    assert!(
        !stats.findings.iter().any(|f| f.rule_id == "SEC001"),
        "execute() with a CRUD-named helper on the same line should not be flagged \
         (word-boundary match must reject \"delete\" inside \"delete_resource_url\"), got: {:?}",
        stats.findings
    );
}

/// AIF3X-331 #17 residual: "DELETE"/"UPDATE" are HTTP verbs too, not just SQL
/// keywords -- a request built with `method="DELETE"` on the same line as
/// `.execute(` would still satisfy a naive require_any check even with
/// word-boundary matching (the verb is quoted, so both flanks are boundaries).
/// Real SQL DELETE/UPDATE injections carry `from`/`where` on the same line,
/// so dropping the two verbs from the keyword list removes this FP without
/// losing SQL detection.
#[test]
fn test_scan_project_http_client_execute_with_delete_verb_not_flagged() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("client.py"),
        "async def call(self, request):\n    return await self.http_client.execute(Request(method=\"DELETE\", url=self.build_url()))\n",
    )
    .unwrap();
    let stats = scan_project(tmp.path()).unwrap();
    assert!(
        !stats.findings.iter().any(|f| f.rule_id == "SEC001"),
        "execute() with method=\"DELETE\" (an HTTP verb, not SQL) should not be flagged, got: {:?}",
        stats.findings
    );
}

/// AIF3X-331 #17: dropping "delete"/"update" from SEC001's require_any list
/// must not create a false negative for real SQL DELETE/UPDATE injections,
/// which carry `from`/`where` on the same line.
#[test]
fn test_scan_project_sql_delete_injection_still_flagged_via_where() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("query.py"),
        "def run(user_input):\n    cursor.execute(\"DELETE FROM users WHERE id = \" + user_input)\n",
    )
    .unwrap();
    let stats = scan_project(tmp.path()).unwrap();
    assert!(
        stats.findings.iter().any(|f| f.rule_id == "SEC001"),
        "DELETE FROM ... WHERE ... via execute() should still be flagged, got: {:?}",
        stats.findings
    );
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
                file: "a.py".to_string(),
                line: 1,
                col: 0,
                severity: Severity::Critical,
                category: Category::HardcodedSecret,
                rule_id: "S1".to_string(),
                message: "".to_string(),
                snippet: "".to_string(),
                suppressed: false,
                sanitizer_hint: None,
            },
            Finding {
                file: "b.py".to_string(),
                line: 2,
                col: 0,
                severity: Severity::High,
                category: Category::HardcodedSecret,
                rule_id: "S2".to_string(),
                message: "".to_string(),
                snippet: "".to_string(),
                suppressed: false,
                sanitizer_hint: None,
            },
            Finding {
                file: "c.py".to_string(),
                line: 3,
                col: 0,
                severity: Severity::Medium,
                category: Category::SqlInjection,
                rule_id: "S3".to_string(),
                message: "".to_string(),
                snippet: "".to_string(),
                suppressed: false,
                sanitizer_hint: None,
            },
            Finding {
                file: "d.py".to_string(),
                line: 4,
                col: 0,
                severity: Severity::Low,
                category: Category::SqlInjection,
                rule_id: "S4".to_string(),
                message: "".to_string(),
                snippet: "".to_string(),
                suppressed: false,
                sanitizer_hint: None,
            },
        ],
    };
    assert_eq!(stats.critical_count(), 1);
    assert_eq!(stats.high_count(), 1);
    assert_eq!(stats.medium_count(), 1);
    assert_eq!(stats.low_count(), 1);
}

// ---------- DES false positive tests ----------

#[test]
fn test_des_false_positive_excluded() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("app.py"),
        "description = 'A long description'\ndescribe('test case')\ndes_key = 'not_crypto'\n",
    )
    .unwrap();

    let stats = infigraph_core::security::scan_project(dir.path()).unwrap();
    let des_findings: Vec<_> = stats
        .findings
        .iter()
        .filter(|f| f.rule_id.starts_with("SEC072"))
        .collect();
    assert!(
        des_findings.is_empty(),
        "description/describe should NOT trigger DES rule: {:?}",
        des_findings
    );
}

#[test]
fn test_des_real_crypto_detected() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("crypto.py"),
        "from Crypto.Cipher import DES\ncipher = DES.new(key, DES.MODE_ECB)\n",
    )
    .unwrap();

    let stats = infigraph_core::security::scan_project(dir.path()).unwrap();
    let des_findings: Vec<_> = stats
        .findings
        .iter()
        .filter(|f| f.rule_id.starts_with("SEC072"))
        .collect();
    assert!(
        !des_findings.is_empty(),
        "DES.new() should be flagged: findings = {:?}",
        stats
            .findings
            .iter()
            .map(|f| &f.rule_id)
            .collect::<Vec<_>>()
    );
}

#[test]
fn test_sanitizer_window_respects_boundary() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut code = String::new();
    code.push_str("# sanitize: escaped\n");
    for _ in 0..10 {
        code.push_str("x = 1\n");
    }
    code.push_str("query = f\"SELECT * FROM users WHERE id = {user_input}\"\n");
    std::fs::write(dir.path().join("far.py"), &code).unwrap();

    let stats = infigraph_core::security::scan_project(dir.path()).unwrap();
    let sql_findings: Vec<_> = stats
        .findings
        .iter()
        .filter(|f| f.category == Category::SqlInjection && !f.suppressed)
        .collect();
    assert!(
        !sql_findings.is_empty(),
        "sanitizer 10+ lines away should NOT suppress finding"
    );
}

// ==================== detect_routes_from_rows: HTTP method from decorator ====================
//
// Regression tests for AIF3X-331: `detect_routes` mislabeled FastAPI
// `@router.post(...)` / `@router.get(...)` decorators. The decorator text is
// captured into the symbol's docstring column (col 4) at extraction time, and
// `detect_from_docstring` parses the method+path from it. The bug: the method
// keyword was matched with a trailing space ("post ") which never matches the
// real `.post(` syntax, so everything silently defaulted to GET.

/// Build a synthetic symbol row in the layout `detect_routes_from_rows` reads:
/// `[id, name, kind, file, docstring]`. The decorator lands in the docstring
/// column exactly as the tree-sitter extractor stores it.
fn route_row(name: &str, decorator: &str) -> Vec<String> {
    vec![
        format!("f.py::{name}"),
        name.to_string(),
        "Function".to_string(),
        "f.py".to_string(),
        decorator.to_string(),
    ]
}

#[test]
fn test_detect_routes_post_decorator_is_post_not_get() {
    let rows = vec![route_row(
        "generate_chat_response",
        r#"@router.post("/{model_id}/chat/completions")"#,
    )];
    let routes = detect_routes_from_rows(&rows);
    assert_eq!(routes.len(), 1, "expected one route, got: {routes:?}");
    assert_eq!(routes[0].method, "POST", "@router.post must be POST");
    assert_eq!(routes[0].path, "/{model_id}/chat/completions");
}

#[test]
fn test_detect_routes_get_decorator_is_get() {
    let rows = vec![route_row(
        "get_status",
        r#"@router.get("/{model_id}/status")"#,
    )];
    let routes = detect_routes_from_rows(&rows);
    assert_eq!(routes.len(), 1, "expected one route, got: {routes:?}");
    assert_eq!(routes[0].method, "GET");
}

#[test]
fn test_detect_routes_method_name_in_path_is_not_the_method() {
    // "/widgets" contains "get" as a substring: must stay POST, not become GET.
    let rows = vec![route_row("create_widget", r#"@router.post("/widgets")"#)];
    let routes = detect_routes_from_rows(&rows);
    assert_eq!(routes.len(), 1, "expected one route, got: {routes:?}");
    assert_eq!(
        routes[0].method, "POST",
        "'get' inside '/widgets' must not flip method to GET"
    );
}

#[test]
fn test_detect_routes_method_name_embedded_in_words_is_ignored() {
    // "budget" contains "get", "deleted" contains "delete": neither is the verb.
    let rows = vec![route_row(
        "mark_deleted",
        r#"@router.post("/budget/deleted")"#,
    )];
    let routes = detect_routes_from_rows(&rows);
    assert_eq!(routes.len(), 1, "expected one route, got: {routes:?}");
    assert_eq!(routes[0].method, "POST");
}
