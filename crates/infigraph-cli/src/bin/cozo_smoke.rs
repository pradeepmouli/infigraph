use std::collections::BTreeMap;

fn main() {
    let db = cozo::DbInstance::new("sqlite", "/tmp/infigraph_cozo_smoke.db", Default::default())
        .expect("open cozo db");

    // Create relations matching infigraph schema
    db.run_script(
        ":create symbol {id: String => name: String, kind: String, file: String, start_line: Int, end_line: Int, visibility: String, complexity: Int, parameters: String, return_type: String}",
        Default::default(),
        cozo::ScriptMutability::Mutable,
    ).expect("create symbol");

    db.run_script(
        ":create file {id: String => name: String, path: String, language: String, symbol_count: Int}",
        Default::default(),
        cozo::ScriptMutability::Mutable,
    ).expect("create file");

    db.run_script(
        ":create calls {caller: String, callee: String}",
        Default::default(),
        cozo::ScriptMutability::Mutable,
    )
    .expect("create calls");

    db.run_script(
        ":create defines {file_id: String, symbol_id: String}",
        Default::default(),
        cozo::ScriptMutability::Mutable,
    )
    .expect("create defines");

    // Insert test data
    let params: BTreeMap<String, cozo::DataValue> = BTreeMap::new();

    db.run_script(
        r#"?[id, name, kind, file, start_line, end_line, visibility, complexity, parameters, return_type] <- [
            ["sym_a", "func_a", "Function", "src/main.rs", 1, 10, "public", 3, "(x: i32)", "i32"],
            ["sym_b", "func_b", "Method", "src/main.rs", 12, 20, "", 1, "()", "()"],
            ["sym_c", "func_c", "Function", "src/lib.rs", 1, 5, "public", 2, "(s: &str)", "String"]
        ]
        :put symbol {id => name, kind, file, start_line, end_line, visibility, complexity, parameters, return_type}"#,
        params.clone(),
        cozo::ScriptMutability::Mutable,
    ).expect("insert symbols");

    db.run_script(
        r#"?[id, name, path, language, symbol_count] <- [
            ["src/main.rs", "main.rs", "src/main.rs", "rust", 2],
            ["src/lib.rs", "lib.rs", "src/lib.rs", "rust", 1]
        ]
        :put file {id => name, path, language, symbol_count}"#,
        params.clone(),
        cozo::ScriptMutability::Mutable,
    )
    .expect("insert files");

    db.run_script(
        r#"?[file_id, symbol_id] <- [
            ["src/main.rs", "sym_a"],
            ["src/main.rs", "sym_b"],
            ["src/lib.rs", "sym_c"]
        ]
        :put defines {file_id, symbol_id}"#,
        params.clone(),
        cozo::ScriptMutability::Mutable,
    )
    .expect("insert defines");

    db.run_script(
        r#"?[caller, callee] <- [
            ["sym_a", "sym_b"],
            ["sym_a", "sym_c"],
            ["sym_c", "sym_b"]
        ]
        :put calls {caller, callee}"#,
        params.clone(),
        cozo::ScriptMutability::Mutable,
    )
    .expect("insert calls");

    // Query 1: symbols_in_file
    let r = db
        .run_script(
            r#"?[id, name, kind, start_line, end_line] :=
            *defines{file_id: "src/main.rs", symbol_id: id},
            *symbol{id, name, kind, start_line, end_line}
        :order start_line"#,
            params.clone(),
            cozo::ScriptMutability::Immutable,
        )
        .expect("symbols_in_file");
    println!("=== symbols_in_file(src/main.rs) ===");
    println!("headers: {:?}", r.headers);
    for row in &r.rows {
        println!("  {:?}", row);
    }

    // Query 2: callers_of(sym_b)
    let r = db
        .run_script(
            r#"?[caller_id] := *calls{caller: caller_id, callee: "sym_b"}"#,
            params.clone(),
            cozo::ScriptMutability::Immutable,
        )
        .expect("callers_of");
    println!("\n=== callers_of(sym_b) ===");
    for row in &r.rows {
        println!("  {:?}", row);
    }

    // Query 3: callees_of(sym_a)
    let r = db
        .run_script(
            r#"?[callee_id] := *calls{caller: "sym_a", callee: callee_id}"#,
            params.clone(),
            cozo::ScriptMutability::Immutable,
        )
        .expect("callees_of");
    println!("\n=== callees_of(sym_a) ===");
    for row in &r.rows {
        println!("  {:?}", row);
    }

    // Query 4: find_symbol_by_id(sym_c)
    let r = db
        .run_script(
            r#"?[id, name, kind, file, start_line, end_line] :=
            id = "sym_c",
            *symbol{id, name, kind, file, start_line, end_line}"#,
            params.clone(),
            cozo::ScriptMutability::Immutable,
        )
        .expect("find_symbol_by_id");
    println!("\n=== find_symbol_by_id(sym_c) ===");
    for row in &r.rows {
        println!("  {:?}", row);
    }

    // Query 5: transitive callers (recursive) — who transitively calls sym_b?
    let r = db
        .run_script(
            r#"reaches[caller] := *calls{caller, callee: "sym_b"}
        reaches[caller] := *calls{caller, callee}, reaches[callee]
        ?[caller] := reaches[caller]"#,
            params.clone(),
            cozo::ScriptMutability::Immutable,
        )
        .expect("transitive callers");
    println!("\n=== transitive_impact(sym_b) ===");
    for row in &r.rows {
        println!("  {:?}", row);
    }

    // Cleanup
    let _ = std::fs::remove_file("/tmp/infigraph_cozo_smoke.db");

    println!("\nSmoke test passed!");
}
