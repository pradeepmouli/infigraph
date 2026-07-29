# Symbol Parameters/Return-Type Field Coverage Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix `Symbol.parameters`/`Symbol.return_type` being empty for Kotlin (all functions) and Dart (all functions and methods) despite the data being present and reachable in both grammars' real ASTs.

**Architecture:** `crates/infigraph-core/src/extract/entities.rs`'s `extract_entities` already has an established capture-name-keyed extraction idiom (used today for `docstring`, `decorator`, and the `route.*` fields) — the query file for a language decides what to capture and how; the Rust code just consumes captures by name, with zero per-language special-casing. Extend that same contract with two new *optional* capture names, `@func.params`/`@method.params` and `@func.return_type`/`@method.return_type`, preferred over the current `child_by_field_name`-based fallback (`extract_child_text`) when a language's query provides them. Wire the new captures into Kotlin's and Dart's own `.scm` query files — zero Rust code specific to either language, zero changes to any of the 11 grammars that already work (they simply never provide these captures, so the fallback preserves today's behavior exactly).

**Tech Stack:** Rust, `tree-sitter` (query language, `tree-sitter-kotlin-ng` and `tree-sitter-dart` grammars).

## Global Constraints

- Spec: `docs/superpowers/specs/2026-07-29-symbol-parameters-field-coverage-design.md` (read for full background/verification detail — this plan implements its "Corrected fix shape" section).
- This work is independent of the SCIP local-symbol-pollution fix (separate spec, separate plan) and is intended for a **separate upstream PR to `intuit/infigraph`** — keep commits self-contained to this plan's scope, no incidental changes to unrelated files.
- Out of scope, deliberately not covered by this plan (per the spec's own scoping):
  - **F#**: no `Symbol` node is created for F# functions at all — a different, deeper class of bug (query/grammar mismatch causing total extraction failure) than a field-lookup gap. Needs its own root-cause investigation before it can even be scoped as a task.
  - **Java's empty `return_type`**: noted as a real gap but not root-caused in the research that produced the spec. Not included here — a quick look during/after this plan is a reasonable follow-up, not part of it.
- `extract_entities`'s exact supported-capture-names contract is documented in its own doc comment (`crates/infigraph-core/src/extract/entities.rs:9-16`) — update it as part of this plan, it's part of the file's public contract for anyone writing a new language's `.scm` file.
- Verified directly against the real bundled grammars' `node-types.json` (not guessed) before writing this plan:
  - `tree-sitter-kotlin-ng-1.1.0`: `function_declaration` has only a `name` named field; `function_value_parameters` (the parameter list) and `type` (the return type) are both present as *positional* (unnamed) children — `child_by_field_name` can never find either regardless of the field-name string tried.
  - `tree-sitter-dart-0.2.0`: `function_signature` (reached via `function_declaration`'s `signature:` field for plain functions, or as a positional child of `method_signature` for methods) has real named fields `name`/`parameters`/`return_type` — but `entities.rs`'s `extract_child_text(node, "parameters", source)` is called on the *outer* captured node (`function_declaration`/`method_signature`), not on `function_signature` itself, so the field-name lookup misses even though the field genuinely exists one level down. For methods specifically, `method_signature` itself has zero named fields at all (`fields: {}`), so there's no field-name path to the answer regardless of nesting depth.
- Environment gotcha: this machine's `~/.zshrc` exports `INFIGRAPH_WATCH_DAEMON=1` globally. Prefix `cargo build`/`cargo test`/`git commit` with `env -u INFIGRAPH_WATCH_DAEMON`.
- Pre-authorized flake for this branch's `--no-verify` justification (only if genuinely hit and confirmed via `git stash` against baseline): `write_lock_perf::test_contended_lock_throughput`.

---

### Task 1: Capture-based parameters/return_type mechanism, wired up for Kotlin

**Files:**
- Modify: `crates/infigraph-core/Cargo.toml` (add `tree-sitter-kotlin-ng` dev-dependency)
- Modify: `crates/infigraph-core/src/extract/entities.rs` (capture-name contract + match loop + `parameters`/`return_type` computation)
- Modify: `crates/infigraph-languages/languages/kotlin/entities.scm` (add `@func.params`/`@func.return_type` captures)
- Test: `crates/infigraph-core/src/extract/entities.rs`'s existing `#[cfg(test)] mod tests` block

**Interfaces:**
- Produces: two new optional query capture names, `@func.params`/`@method.params` and `@func.return_type`/`@method.return_type`, documented in `extract_entities`'s doc comment. Any language's `.scm` file may provide them; none is required to.
- Consumes: nothing from another task (this is the first task; Task 2 consumes the mechanism this task produces, not a new interface — it just adds a second `.scm` file using the same two capture names).

- [ ] **Step 1: Add the Kotlin grammar as a dev-dependency**

In `crates/infigraph-core/Cargo.toml`, find the `[dev-dependencies]` section (it already has `tree-sitter-python = "0.25"`, used by `entities.rs`'s existing per-language tests) and add:

```toml
tree-sitter-kotlin-ng = "1.1"
```

(Exact version matches what `crates/infigraph-languages/Cargo.toml` already pins, confirmed via that file.)

- [ ] **Step 2: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block in `crates/infigraph-core/src/extract/entities.rs` (after the existing `test_python_function_no_return_type` test):

```rust
    #[test]
    fn test_kotlin_function_extracts_parameters_and_return_type() {
        // tree-sitter-kotlin-ng's function_declaration has no named
        // "parameters"/"return_type" fields at all -- function_value_parameters
        // and the return type are both positional children. Verified against
        // the crate's real node-types.json before writing this test.
        let grammar = tree_sitter_kotlin_ng::LANGUAGE.into();
        let src = b"fun greet(name: String, age: Int): String {\n    return name\n}\n";
        let mut parser = Parser::new();
        parser.set_language(&grammar).unwrap();
        let tree = parser.parse(src, None).unwrap();
        let root = tree.root_node();

        let query = tree_sitter::Query::new(
            &grammar,
            "(function_declaration\n  \
               name: (identifier) @func.name\n  \
               (function_value_parameters) @func.params\n  \
               (type)? @func.return_type) @func.def",
        )
        .unwrap();

        let symbols = extract_entities("greet.kt", src, root, &query, "kotlin");
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "greet");
        assert!(
            symbols[0].parameters.is_some(),
            "parameters should be extracted via the @func.params capture"
        );
        assert!(
            symbols[0].parameters.as_deref().unwrap().contains("name"),
            "parameters should contain param names: {:?}",
            symbols[0].parameters
        );
        assert_eq!(
            symbols[0].return_type.as_deref(),
            Some("String"),
            "return_type should be extracted via the @func.return_type capture"
        );
    }

    #[test]
    fn test_kotlin_function_without_return_type_annotation() {
        // @func.return_type is an optional capture (`(type)?`) -- must not
        // panic or produce a bogus value when a function has no explicit
        // return type (Kotlin infers Unit).
        let grammar = tree_sitter_kotlin_ng::LANGUAGE.into();
        let src = b"fun sayHi(name: String) {\n    println(name)\n}\n";
        let mut parser = Parser::new();
        parser.set_language(&grammar).unwrap();
        let tree = parser.parse(src, None).unwrap();
        let root = tree.root_node();

        let query = tree_sitter::Query::new(
            &grammar,
            "(function_declaration\n  \
               name: (identifier) @func.name\n  \
               (function_value_parameters) @func.params\n  \
               (type)? @func.return_type) @func.def",
        )
        .unwrap();

        let symbols = extract_entities("sayHi.kt", src, root, &query, "kotlin");
        assert_eq!(symbols.len(), 1);
        assert!(symbols[0].parameters.is_some());
        assert_eq!(symbols[0].return_type, None);
    }
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `env -u INFIGRAPH_WATCH_DAEMON cargo test -p infigraph-core --lib extract::entities -- --test-threads=1`

Expected: FAIL to compile at first (`tree_sitter_kotlin_ng` not yet a usable crate reference — wait, Step 1 already added it as a dependency, so it should compile) — actually expected: compiles fine, but `test_kotlin_function_extracts_parameters_and_return_type` FAILS on the `symbols[0].parameters.is_some()` assertion (currently `None`, since nothing populates `params_capture` yet and `extract_child_text(node, "parameters", ...)` returns `None` for this grammar as established in the spec).

- [ ] **Step 4: Implement the capture-based mechanism**

In `crates/infigraph-core/src/extract/entities.rs`, update the doc comment (lines 9-16) listing supported capture names:

```rust
/// The query must use these capture names:
///   @func.def / @func.name / @func.docstring / @func.decorator
///   @func.params / @func.return_type (both optional -- only needed when a
///     grammar doesn't expose "parameters"/"return_type"/"result" as named
///     fields on the captured @func.def node; see Kotlin/Dart for examples)
///   @method.def / @method.name / @method.docstring / @method.decorator
///   @method.params / @method.return_type (same as @func.params/@func.return_type)
///   @class.def / @class.name / @class.docstring / @class.decorator
///   @module.def / @module.name
///   @test.def / @test.name / @test.docstring
///   @var.def / @var.name
///   @route.def / @route.method / @route.path / @route.handler
```

Add two new locals alongside the existing `docstring`/`decorator` declarations (in the `while let Some(m) = matches.next()` loop, near line 35-36):

```rust
        let mut params_capture: Option<String> = None;
        let mut return_type_capture: Option<String> = None;
```

Add two new match arms in the capture-processing loop, immediately after the existing `"method.decorator"` arm (around line 79-81):

```rust
                "func.params" | "method.params" => {
                    params_capture = Some(node_text(node, source));
                }
                "func.return_type" | "method.return_type" => {
                    return_type_capture = Some(node_text(node, source));
                }
```

Finally, change how `parameters`/`return_type` are computed (currently around line 213-215):

```rust
            let parameters = extract_child_text(node, "parameters", source);
            let return_type = extract_child_text(node, "return_type", source)
                .or_else(|| extract_child_text(node, "result", source));
```

to:

```rust
            let parameters =
                params_capture.or_else(|| extract_child_text(node, "parameters", source));
            let return_type = return_type_capture.or_else(|| {
                extract_child_text(node, "return_type", source)
                    .or_else(|| extract_child_text(node, "result", source))
            });
```

- [ ] **Step 5: Add the Kotlin `.scm` captures**

In `crates/infigraph-languages/languages/kotlin/entities.scm`, replace:

```scheme
; Function declarations
(function_declaration
  name: (identifier) @func.name) @func.def
```

with:

```scheme
; Function declarations
; function_value_parameters and the return type are positional children
; on this grammar, not named fields -- extract_child_text can't reach
; either via child_by_field_name, so capture them explicitly here.
(function_declaration
  name: (identifier) @func.name
  (function_value_parameters) @func.params
  (type)? @func.return_type) @func.def
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `env -u INFIGRAPH_WATCH_DAEMON cargo test -p infigraph-core --lib extract::entities -- --test-threads=1`

Expected: all tests pass, including both new Kotlin tests and every pre-existing test in `extract::entities::tests` (in particular `test_python_function_extracts_parameters_and_return_type` and `test_python_function_no_return_type` — these prove the `.or_else` fallback still gives Python its exact prior behavior, since Python's query never provides `@func.params`/`@func.return_type`).

- [ ] **Step 7: Confirm end-to-end via a real indexed project (not just the unit test)**

```bash
mkdir -p /tmp/infigraph-kotlin-check/src
cat > /tmp/infigraph-kotlin-check/src/Greeter.kt << 'EOF'
fun greet(name: String, age: Int): String {
    return "hello $name"
}
EOF
```

Use `mcp__infigraph__index_project` (path `/tmp/infigraph-kotlin-check`), then `mcp__infigraph__query_graph` with Cypher `MATCH (s:Symbol {name: 'greet'}) RETURN s.parameters, s.return_type`. Expected: `s.parameters` contains `name`/`age` with types, `s.return_type` is `String`. Clean up `/tmp/infigraph-kotlin-check` afterward.

- [ ] **Step 8: fmt + clippy**

```bash
cargo fmt --all
env -u INFIGRAPH_WATCH_DAEMON cargo clippy -p infigraph-core --all-targets -- -D warnings
```

- [ ] **Step 9: Commit**

```bash
git add crates/infigraph-core/Cargo.toml crates/infigraph-core/src/extract/entities.rs crates/infigraph-languages/languages/kotlin/entities.scm
env -u INFIGRAPH_WATCH_DAEMON git commit -m "$(cat <<'EOF'
feat: capture-based parameters/return_type extraction, fix Kotlin

Symbol.parameters/return_type were empty for all Kotlin functions.
Root cause, verified against tree-sitter-kotlin-ng's real
node-types.json: function_declaration has no "parameters" or
"return_type"/"result" named fields at all -- the parameter list
(function_value_parameters) and the return type (a bare `type` node)
are both positional children, structurally unreachable via
child_by_field_name regardless of what field-name string is tried.

Extended extract_entities's existing capture-name-keyed extraction
idiom (already used for docstring/decorator/route.* fields) with two
new optional captures, @func.params/@method.params and
@func.return_type/@method.return_type, preferred over the existing
field-name fallback when a language's query provides them. Wired up
for Kotlin only in this commit -- zero changes to any other grammar,
whose queries simply never provide these captures, so the fallback
preserves their exact current behavior (regression-covered by the
pre-existing Python parameters/return_type tests still passing
unchanged).
EOF
)"
```

---

### Task 2: Wire up Dart (functions and methods)

**Files:**
- Modify: `crates/infigraph-core/Cargo.toml` (add `tree-sitter-dart` dev-dependency)
- Modify: `crates/infigraph-languages/languages/dart/entities.scm` (add `@func.params`/`@func.return_type` to the function pattern, `@method.params`/`@method.return_type` to the method pattern)
- Test: `crates/infigraph-core/src/extract/entities.rs`'s existing `#[cfg(test)] mod tests` block

**Interfaces:**
- Consumes: the `@func.params`/`@method.params`/`@func.return_type`/`@method.return_type` capture mechanism produced by Task 1 (`crates/infigraph-core/src/extract/entities.rs`). No further changes to `entities.rs` needed in this task — it's `.scm`-only, following the same pattern Task 1 established for Kotlin.

- [ ] **Step 1: Add the Dart grammar as a dev-dependency**

In `crates/infigraph-core/Cargo.toml`, in `[dev-dependencies]`, add:

```toml
tree-sitter-dart = "0.2"
```

- [ ] **Step 2: Write the failing tests**

Add to `crates/infigraph-core/src/extract/entities.rs`'s `#[cfg(test)] mod tests` block, after the Kotlin tests added in Task 1:

```rust
    #[test]
    fn test_dart_function_extracts_parameters_and_return_type() {
        // tree-sitter-dart's function_signature DOES have real "parameters"/
        // "return_type" named fields -- but they're one level below the
        // captured function_declaration node (reached via its "signature"
        // field), so extract_child_text's direct child_by_field_name lookup
        // on the outer node misses them. Verified against the crate's real
        // node-types.json before writing this test.
        let grammar = tree_sitter_dart::LANGUAGE.into();
        let src = b"String greet(String name, int age) {\n  return 'hello';\n}\n";
        let mut parser = Parser::new();
        parser.set_language(&grammar).unwrap();
        let tree = parser.parse(src, None).unwrap();
        let root = tree.root_node();

        let query = tree_sitter::Query::new(
            &grammar,
            "(function_declaration\n  \
               signature: (function_signature\n    \
                 name: (identifier) @func.name\n    \
                 parameters: (formal_parameter_list) @func.params\n    \
                 return_type: (type)? @func.return_type)) @func.def",
        )
        .unwrap();

        let symbols = extract_entities("greet.dart", src, root, &query, "dart");
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "greet");
        assert!(
            symbols[0].parameters.is_some(),
            "parameters should be extracted via the @func.params capture"
        );
        assert!(
            symbols[0].parameters.as_deref().unwrap().contains("name"),
            "parameters should contain param names: {:?}",
            symbols[0].parameters
        );
        assert_eq!(symbols[0].return_type.as_deref(), Some("String"));
    }

    #[test]
    fn test_dart_method_extracts_parameters_and_return_type() {
        // method_signature has ZERO named fields at all (verified against
        // node-types.json) -- structurally unreachable via child_by_field_name
        // from the captured node regardless of nesting depth, unlike the
        // top-level function case which is merely "one level too shallow".
        let grammar = tree_sitter_dart::LANGUAGE.into();
        let src =
            b"class Greeter {\n  String greet(String name, int age) {\n    return 'hi';\n  }\n}\n";
        let mut parser = Parser::new();
        parser.set_language(&grammar).unwrap();
        let tree = parser.parse(src, None).unwrap();
        let root = tree.root_node();

        let query = tree_sitter::Query::new(
            &grammar,
            "(method_signature\n  \
               (function_signature\n    \
                 name: (identifier) @method.name\n    \
                 parameters: (formal_parameter_list) @method.params\n    \
                 return_type: (type)? @method.return_type)) @method.def",
        )
        .unwrap();

        let symbols = extract_entities("greeter.dart", src, root, &query, "dart");
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "greet");
        assert!(
            symbols[0].parameters.is_some(),
            "parameters should be extracted for Dart methods too, not just top-level functions"
        );
        assert!(
            symbols[0].parameters.as_deref().unwrap().contains("name"),
            "parameters should contain param names: {:?}",
            symbols[0].parameters
        );
        assert_eq!(symbols[0].return_type.as_deref(), Some("String"));
    }

    #[test]
    fn test_dart_constructor_still_works_unchanged() {
        // constructor_signature has direct name+parameters fields, no
        // nesting -- already worked before this plan and must keep working,
        // using the existing child_by_field_name fallback (no @method.params
        // capture needed or added for constructors).
        let grammar = tree_sitter_dart::LANGUAGE.into();
        let src = b"class Greeter {\n  Greeter(String name, int age);\n}\n";
        let mut parser = Parser::new();
        parser.set_language(&grammar).unwrap();
        let tree = parser.parse(src, None).unwrap();
        let root = tree.root_node();

        let query = tree_sitter::Query::new(
            &grammar,
            "(constructor_signature name: (identifier) @method.name) @method.def",
        )
        .unwrap();

        let symbols = extract_entities("greeter.dart", src, root, &query, "dart");
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "Greeter");
        assert!(
            symbols[0].parameters.is_some(),
            "constructor parameters must still work via the pre-existing fallback"
        );
    }
```

- [ ] **Step 3: Run tests to verify the new ones fail**

Run: `env -u INFIGRAPH_WATCH_DAEMON cargo test -p infigraph-core --lib extract::entities -- --test-threads=1`

Expected: `test_dart_function_extracts_parameters_and_return_type` and `test_dart_method_extracts_parameters_and_return_type` FAIL on the `parameters.is_some()` assertion. `test_dart_constructor_still_works_unchanged` PASSES already (constructors were never broken — this test proves that and guards against a regression in the next step).

- [ ] **Step 4: Add the Dart `.scm` captures**

In `crates/infigraph-languages/languages/dart/entities.scm`, replace:

```scheme
; Top-level function declarations
(function_declaration
  signature: (function_signature
    name: (identifier) @func.name)) @func.def

; Method signatures
(method_signature
  (function_signature
    name: (identifier) @method.name)) @method.def
```

with:

```scheme
; Top-level function declarations
; function_signature genuinely has "parameters"/"return_type" named
; fields, but they're one level below @func.def (reached via
; function_declaration's own "signature" field) -- capture them
; directly here rather than relying on child_by_field_name on the
; outer node, which can't see through that extra level.
(function_declaration
  signature: (function_signature
    name: (identifier) @func.name
    parameters: (formal_parameter_list) @func.params
    return_type: (type)? @func.return_type)) @func.def

; Method signatures
; method_signature has ZERO named fields of its own -- there is no
; child_by_field_name path to "parameters" here at any nesting depth,
; so this capture is required, not just an optimization.
(method_signature
  (function_signature
    name: (identifier) @method.name
    parameters: (formal_parameter_list) @method.params
    return_type: (type)? @method.return_type)) @method.def
```

(Constructor signatures are unchanged — `constructor_signature` already has direct `name`/`parameters` fields, no fix needed, covered by `test_dart_constructor_still_works_unchanged`.)

- [ ] **Step 5: Run tests to verify they pass**

Run: `env -u INFIGRAPH_WATCH_DAEMON cargo test -p infigraph-core --lib extract::entities -- --test-threads=1`

Expected: all tests pass, including all three new Dart tests and every pre-existing test.

- [ ] **Step 6: Confirm end-to-end via a real indexed project**

```bash
mkdir -p /tmp/infigraph-dart-check/lib
cat > /tmp/infigraph-dart-check/lib/greeter.dart << 'EOF'
String greetFn(String name, int age) {
  return 'hello $name';
}

class Greeter {
  String greetMethod(String name, int age) {
    return 'hi $name';
  }
}
EOF
```

Use `mcp__infigraph__index_project` (path `/tmp/infigraph-dart-check`), then `mcp__infigraph__query_graph` with Cypher `MATCH (s:Symbol) WHERE s.name IN ['greetFn', 'greetMethod'] RETURN s.name, s.parameters, s.return_type`. Expected: both rows show populated `parameters` (containing `name`/`age`) and `return_type` (`String`). Clean up `/tmp/infigraph-dart-check` afterward.

- [ ] **Step 7: Run the full workspace test suite**

Run: `env -u INFIGRAPH_WATCH_DAEMON cargo test --all 2>&1 | tail -100`

Expected: green, modulo any already-known pre-existing flakes (`watcher_concurrency::test_graph_tools_with_group_watchers` is the most recently confirmed pre-existing one). This task's changes are narrowly scoped (`entities.rs` from Task 1, two `.scm` files, `Cargo.toml` dev-deps) but `infigraph-core` is shared/foundational — run the full suite before considering this plan done, not just the targeted tests.

- [ ] **Step 8: fmt + clippy**

```bash
cargo fmt --all
env -u INFIGRAPH_WATCH_DAEMON cargo clippy -p infigraph-core --all-targets -- -D warnings
```

- [ ] **Step 9: Commit**

```bash
git add crates/infigraph-core/Cargo.toml crates/infigraph-languages/languages/dart/entities.scm
env -u INFIGRAPH_WATCH_DAEMON git commit -m "$(cat <<'EOF'
feat: capture-based parameters/return_type extraction for Dart

Symbol.parameters/return_type were empty for all Dart functions and
methods. Root cause, verified against tree-sitter-dart's real
node-types.json: function_signature genuinely has "parameters"/
"return_type" named fields, but they live one level below the
captured @func.def/@method.def node (through function_declaration's
"signature" field for plain functions), so extract_child_text's
direct child_by_field_name lookup on the outer node never sees them.
For methods specifically it's worse -- method_signature has zero
named fields at all, so there's no field-name path to the answer
regardless of nesting depth, not just a wrong-node problem.

Uses the @func.params/@method.params/@func.return_type/@method.return_type
capture mechanism added for Kotlin in the previous commit -- this
commit is .scm-only, no further Rust changes. Constructor signatures
already worked via their own direct name+parameters fields and are
unchanged, covered by a dedicated regression test.
EOF
)"
```

---

## Self-Review Notes

- **Spec coverage**: the spec's "Corrected fix shape" section specifies the capture-based mechanism plus Kotlin and Dart as the two confirmed, root-caused fixes. Both are covered (Task 1, Task 2). F# (different bug class) and Java's `return_type` (not root-caused in the spec's research) are explicitly excluded per the spec's own scoping — not silently dropped, noted in Global Constraints.
- **Placeholder scan**: no TBD/TODO; every `.scm` addition and every Rust diff shown in full; every test has real assertions against real expected values (not just "assert something happened").
- **Type consistency**: `params_capture`/`return_type_capture` are both `Option<String>`, matching `Symbol.parameters`/`Symbol.return_type`'s type exactly (`crates/infigraph-core/src/model/mod.rs:75` and neighboring field). Capture name convention (`func.params`/`method.params`, `func.return_type`/`method.return_type`) is consistent between the doc comment, the match arms, and both `.scm` files across both tasks.
- **Cross-task interface**: Task 2 explicitly consumes Task 1's capture mechanism without needing to know its internals — just the two capture-name pairs, which are documented in `entities.rs`'s own doc comment (updated in Task 1, Step 4) as the stable contract Task 2's `.scm` work relies on.
- **Regression coverage**: every existing test in `extract::entities::tests` must still pass after both tasks (verified via full-module test runs at each task's end), proving the 11 already-working grammars are genuinely untouched by this plan, not just assumed untouched.
