# `Symbol.parameters`/`return_type` Field Coverage — Design

> **Scope note:** this work is independent of the SCIP local-symbol-pollution fix (`docs/superpowers/specs/2026-07-28-scip-local-symbol-pollution-design.md`), discovered as a side-finding while investigating it. Per explicit direction, this is intended as a **separate PR to the upstream `intuit/infigraph` repository**, not bundled with the SCIP fix.

## Background

`Symbol.parameters: Option<String>` and `Symbol.return_type: Option<String>` are populated at tree-sitter extraction time — `crates/infigraph-core/src/extract/entities.rs:213-215`:

```rust
let parameters = extract_child_text(node, "parameters", source);
let return_type = extract_child_text(node, "return_type", source)
    .or_else(|| extract_child_text(node, "result", source));
```

`extract_child_text` (`entities.rs:386-394`) calls tree-sitter's `node.child_by_field_name(field_name)` and returns the exact source text of that child, trimmed. This is independent of SCIP entirely — it runs during the primary AST-extraction pass, for every indexed file, in every language.

This was initially assumed (incorrectly, by me) to be entirely unpopulated — corrected mid-investigation of the SCIP bug when it was pointed out this data is also available from the AST directly, and confirmed live via a real `index_project` run that `parameters` *was* already richly populated for TypeScript, with types.

## Verified coverage (2026-07-29)

Checked empirically against all languages `crates/infigraph-cli/src/scip_download.rs`'s `CATALOG` configures a SCIP indexer for (the 10 indexer entries, 14 individual tree-sitter grammars) — for each, indexed a small real fixture via `mcp__infigraph__index_project` and queried the resulting `Symbol.parameters`/`return_type` via Cypher, mirroring the method that found this originally for TypeScript.

| Language | `parameters` | `return_type` | Notes |
|---|---|---|---|
| TypeScript | ✅ | ✅ | Found first, motivated this investigation |
| Python | ✅ | ✅ | |
| Rust | ✅ | ✅ | |
| Go | ✅ | ✅ | |
| Java | ✅ | ❌ empty | `return_type` gap not investigated further — out of this directive's scope, flag for follow-up |
| Scala | ✅ | ✅ | |
| Ruby | ✅ | n/a (dynamically typed) | |
| C# | ✅ | ✅ | |
| C | ✅ | ✅ | |
| C++ | ✅ | ✅ | Verified via a class method specifically, not just a free function |
| PHP | ✅ | ✅ | Most complete of the four in that batch |
| **Kotlin** | ❌ **broken** | — | See below |
| **Dart** | ❌ **broken** | — | See below |
| **F#** | ❌ **broken** | — | See below (deeper: no `Symbol` node at all) |

11 of 14 grammars work correctly today. 3 have real, root-caused gaps.

## Root causes

### Kotlin

`tree-sitter-kotlin-ng`'s `function_declaration` node has **no `parameters` field at all** — confirmed against the crate's actual `node-types.json`, not guessed. The parameter list (`function_value_parameters`) is present as an **unnamed, positional child**, not a named field. `child_by_field_name("parameters")` is structurally incapable of finding it, regardless of what field-name string is tried.

The entity query itself is fine — `crates/infigraph-languages/languages/kotlin/entities.scm:4-5` already captures the full `function_declaration` node as `@func.def`. The gap is entirely in `extract_child_text`'s field-name-only lookup strategy, which this grammar doesn't support for parameters.

**Fix shape — REVISED, see "Corrected fix shape" below.**

### Dart

Two distinct issues, both confirmed against `tree-sitter-dart`'s actual `node-types.json`:

1. **Plain functions**: `function_declaration`'s only fields are `body` and `signature` — `name`/`parameters`/`return_type` all live one level down, on the nested `function_signature` node reached via the `signature` field. The `.scm` query already knows this for `name` (`signature: (function_signature name: (identifier) @func.name)`), but `extract_child_text(node, "parameters", source)` operates on the outer `@func.def` node directly, where `child_by_field_name("parameters")` correctly returns `None` — it's simply the wrong node.
2. **Methods**: worse — `method_signature` has **zero** named fields (`fields: []`). Its `function_signature` child is itself unnamed/positional. `parameters` is structurally unreachable via `child_by_field_name` from the currently-captured node for *any* Dart method, not just a wrong-node problem like plain functions.
3. **Constructors already work** — `constructor_signature` has direct `name`+`parameters` fields, no nesting, no fix needed.

**Fix shape — REVISED, see "Corrected fix shape" below.** (Note: capturing the inner `function_signature`/positional child directly *as* `@func.def` would be wrong regardless of mechanism — it would truncate the symbol's line range to just the signature, losing the full-body span the rest of the pipeline depends on. The corrected approach below captures the params as a *separate*, additional capture alongside the existing full-body `@func.def`/`@method.def`, not a replacement for it.)

### F# — different class of bug, not really "parameters coverage"

**No `Symbol` node is created for F# functions at all**, in either curried-parameter style (`let f (a: T) (b: U) : R = ...`) or tuple-parameter style (`let f (a: T, b: U) : R = ...`), with or without a wrapping `module`. Only the enclosing `module` itself gets extracted (as kind `Section`). `crates/infigraph-languages/languages/fsharp/entities.scm` has a real capture pattern (`(function_or_value_defn (function_declaration_left . (_) @func.name)) @func.def`) that isn't matching either real parse tree.

This needs its own root-cause pass (checking `tree-sitter-fsharp`'s actual `function_or_value_defn`/`function_declaration_left` node shape against real parse output, the same way Kotlin/Dart were diagnosed here) before it can even be scoped as a plan task — there's no `parameters` field question to answer if no symbol exists to carry it. **Recommend splitting this into its own follow-up rather than folding it into this plan** — fixing it is a precondition for F# ever benefiting from this work, but the fix itself is a different kind of bug (query/grammar mismatch causing total extraction failure, not a field-lookup gap).

## Corrected fix shape — capture-based, not code-side traversal

Original draft of this section proposed a generic code-side fallback in `extract_child_text` (search children by node kind when field-name lookup fails). **Revised after discussion**: that would hardcode field-name/structure assumptions into Rust (`entities.rs`) that only the language-specific query author actually knows are correct for a given grammar — the wrong place for that knowledge to live, and it doesn't generalize to arbitrarily-nested cases like Dart's methods without writing increasingly special-cased traversal code.

`extract_entities` already has an established, working idiom for exactly this kind of language-specific data: capture-name-keyed extraction. The function's own doc comment (`entities.rs:9-16`) documents the supported capture names (`@func.def`/`@func.name`/`@func.docstring`/`@func.decorator`, same for `@method.*`/`@class.*`, plus `@route.method`/`@route.path`/`@route.handler`), and the match loop (`entities.rs:47` onward) already populates `Option<String>` locals (`docstring`, `decorator`, `route_method`, etc.) from whichever captures a given language's query happens to provide — nothing is hardcoded about *how* a language's grammar exposes a docstring or a decorator; each language's `.scm` file decides what to capture and how, and the Rust code just consumes it by capture name.

**Fix**: extend that same contract with two new optional capture names, `@func.params` / `@method.params` (and if Java's `return_type` gap turns out to need the same treatment, `@func.return_type` / `@method.return_type`):

- Add `params_capture: Option<String>` (and `return_type_capture` if needed) alongside the existing `docstring`/`decorator` locals in the match loop (`entities.rs:32-40`).
- Add match arms: `"func.params" | "method.params" => { params_capture = Some(node_text(node, source)); }`.
- At the point where `parameters`/`return_type` are currently computed (`entities.rs:213-215`), prefer the explicit capture when present, falling back to today's `extract_child_text(node, "parameters"/"return_type"/"result", source)` when it isn't:
  ```rust
  let parameters = params_capture.or_else(|| extract_child_text(node, "parameters", source));
  ```
- **Kotlin fix, entirely in `.scm`, zero Rust code specific to Kotlin**: add a capture for the positional `function_value_parameters` child in `crates/infigraph-languages/languages/kotlin/entities.scm`, e.g. `(function_declaration (function_value_parameters) @func.params)`.
- **Dart fix, entirely in `.scm`**: add captures matching the real nesting depth found for each case in `crates/infigraph-languages/languages/dart/entities.scm` — for plain functions, through the `signature` field into `function_signature`'s params; for methods, through `method_signature`'s positional `function_signature` child into its params. Tree-sitter query patterns can match arbitrary nesting depth and positional (unnamed) children directly, unlike `child_by_field_name`, so both cases are expressible as query patterns without needing separate Rust-side handling for "field vs. positional" or "one level vs. two levels deep."

**Why this is better than the code-side fallback**: zero changes to any of the 11 already-working grammars (they simply never provide a `func.params` capture, so the `.or_else` fallback preserves today's behavior exactly); the exact-structure knowledge for Kotlin/Dart lives in their own query files, written by/reviewable against that grammar's actual `node-types.json`, not encoded as generic traversal heuristics in shared Rust code that has to stay correct for every grammar's quirks at once; and it reuses an idiom this file already has three examples of (docstring, decorator, route fields) rather than introducing a new one.

## Testing

For each of Kotlin/Dart (and F#, once its extraction bug is independently fixed), add a targeted test in `crates/infigraph-core/src/extract/entities.rs`'s existing test module (it already has per-language fixture tests, e.g. the ones referenced in the diagnostic forks' methodology) asserting `parameters`/`return_type` are populated with the expected typed text — mirroring the existing TypeScript-style assertions already in that file. Use real fixture snippets like the ones the research forks used (left at `scratchpad/params-check-{kotlin,dart,fsharp}` in this session — not committed, reproduce fresh for the actual test).

## Scope

In scope: `crates/infigraph-core/src/extract/entities.rs` (`extract_child_text` and/or its call sites), possibly `crates/infigraph-languages/languages/{kotlin,dart}/entities.scm` if a query-level change ends up being part of the chosen fix shape. Java's empty `return_type` is a real but separate, smaller gap — worth folding in if trivial once here, otherwise its own follow-up.

Explicitly out of scope: F#'s total-extraction-failure bug (recommend as its own item, see above), and the SCIP local-symbol-pollution fix (separate spec, separate destination).

## Confidence and recommendation

**Confidence: High** for Kotlin and Dart — both root-caused against the actual bundled grammar's `node-types.json`, not inferred from behavior alone, and the corrected capture-based fix shape reuses an idiom already proven in this exact file (docstring/decorator/route captures). **Medium** for F# — confirmed broken and confirmed the query exists, but the actual grammar-mismatch root cause wasn't run to ground in the time available.

**Recommendation**: write an implementation plan covering Kotlin + Dart via the capture-based fix (add `@func.params`/`@method.params` to each grammar's own `.scm` file, plus the small shared `entities.rs` change to prefer that capture when present), fold in Java's `return_type` gap if a quick look shows it's the same shape of problem, and open F#'s total-extraction bug as a separate, prerequisite investigation rather than blocking this plan on it.
