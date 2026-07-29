# SCIP Local-Symbol Pollution — Design

## Problem

Live `get_doc_context` output on `sittir` (a TypeScript project, SCIP-enriched via `scip-typescript`) shows callers/callees entries that are raw, unparsed SCIP monikers instead of clean symbol names, e.g.:

```
packages/codegen/src/dsl/enrich.ts::scip-typescript npm @sittir/codegen 0.1.0 src/dsl/`enrich.ts`/mintStructuredChoiceArm().(rulesBag)
```

One such entry exists for every local variable declared inside `mintStructuredChoiceArm()` (`rulesBag`, `clauseGroupRules`, `counter`, `groupDedupeMap`, `visibleGroupHiddenNames`, `clauseGroupOwners`, `collidingLeadingNames`, `ambientPrec`, ...). These are local variables, not functions — they should never appear as independent, cross-referenceable graph nodes at all.

## Root cause (two compounding bugs)

Both live in `crates/infigraph-core/src/scip/mod.rs`, `import_scip_index`.

**1. Local-descriptor detection is text-shape-only, and the shape assumption is wrong for this case.**

Four call sites (lines ~99, ~133, ~318, ~417) skip an occurrence if `occ.symbol.starts_with("local ") || occ.symbol.starts_with('<')`. This correctly catches SCIP-spec `local <id>` symbols and synthetic (`<...>`) symbols. It does **not** catch `scip-typescript`'s actual encoding for these — a full global-style moniker with a nested Parameter descriptor immediately following the enclosing method's own `()` — `<method-descriptor>().(<paramName>)`. No `local ` prefix, so it sails straight past the filter.

**Verified against real `scip-typescript` output (2026-07-28, generated locally with `scip-typescript 0.4.0`, see "Verification" below): these entries are function *parameters*, not block-scoped local `const`/`let` variables.** A plain `const` inside a function body gets a genuine `local N` symbol (correctly filtered already). A *directly-declared, non-destructured parameter* of an analyzed top-level function gets exactly the `<method>().(<paramName>)` moniker shape with no `local ` prefix — reproduced byte-for-byte against the real screenshot by indexing a function with several simple positional parameters (`rulesBag`, `clauseGroupRules`, `counter`, `groupDedupeMap`, ...). The real `mintStructuredChoiceArm` in `sittir` almost certainly takes a wide simple-parameter list, not a destructured object (typed destructuring produces a different, already-filtered shape — see Verification).

**2. `scip_sym_to_name`'s fallback silently returns the raw moniker for exactly this shape.**

`scip_sym_to_name` (line ~516) is supposed to extract a clean short name from a moniker. Tracing it against the string above: it correctly strips the trailing `.(rulesBag)` group, leaving `...mintStructuredChoiceArm()`. The next step (trim trailing `.`) does nothing since it now ends in `)`. The final "bare identifier" fallback requires the string to end in an identifier character — it ends in `)` instead — so the function falls through to `else { scip_sym.to_string() }`, returning the **entire original raw moniker**. This name becomes the graph node's `name` field verbatim (`Pass 1`, line ~137, `let name = scip_sym_to_name(scip_sym)`), matching exactly what's in the screenshot.

**Combined effect**: every local variable inside every SCIP-typescript-indexed function becomes its own real `Symbol` graph node (via the `else` branch at line ~151, since its ugly name never coincidentally matches an existing tree-sitter symbol), each with a tiny line-span matching its declaration. Since `Pass 2`'s reference-to-container resolution (`file_symbols`, sorted smallest-span-first for containment) can pick these bogus tiny-span nodes as `container_id` for any reference that happens to fall on their declaration line, this isn't just cosmetic — it can misattribute real `CALLS` edges away from the true enclosing function.

## Design principle (per discussion) — REVISED after verification

Original draft of this section proposed gating on SCIP's `SymbolInformation.kind`. **That approach is invalid — verified empirically, not just risky.** Every symbol in real `scip-typescript 0.4.0` output has `kind == UnspecifiedKind`: the module, the function, the parameter, and even the genuine `local N` symbols. `kind` is never populated by this indexer at all, so it cannot be used as a signal for anything. (This was flagged as unverified in the original draft's "confirm before implementing" section — checking it first, as asked, caught a design that would not have worked before any code was written.)

The real, verified fix follows the same "match against existing entries" principle, but structurally rather than via `kind`:

**Match a candidate definition occurrence against a symbol *already known from this same import pass*, by exact string containment, not by inferring identity for the nested descriptor at all.**

Verified structural fact (byte-for-byte, from real output): a parameter's moniker is *exactly* its enclosing method's own moniker (which always ends in a trailing `.`) with `(<paramName>)` appended — no other transformation:

```
method:    ...mintStructuredChoiceArm().
parameter: ...mintStructuredChoiceArm().(rulesBag)
```

`import_scip_index` already builds `scip_sym_to_file_name: HashMap<String, (String, String)>` (line ~92) by iterating every document's definition occurrences and inserting every non-`local `/non-`<`-prefixed symbol string as a key — and this happens in a pass that runs *before* Pass 1 decides whether to create a new symbol. So by the time Pass 1 (line ~128) is deciding what to do with a `.(rulesBag)`-shaped occurrence, `scip-typescript npm ... mintStructuredChoiceArm().` is *already* a key in `scip_sym_to_file_name` — a symbol we already know is real.

The fix: before falling to the `else` (new-symbol) branch in Pass 1, if the candidate symbol ends in `)`, strip the trailing `(...)` group; if what remains is a key already present in `scip_sym_to_file_name`, this occurrence is a parameter (or similarly-shaped member) of an *already-known* real symbol — skip it entirely (no new `Symbol`, no enrichment, not added to `file_name_to_ids`/`file_symbols`). This is exact string matching against data the importer already collected, not a heuristic re-derived from scratch, and not dependent on `kind` at all.

This directly generalizes: it isn't specific to "parameters" as a concept — it's "is this symbol nested syntactically under one we already know is real," which is the right question regardless of which SCIP descriptor kind produces that nesting (parameters today; possibly other shapes from other indexers/versions later, all covered by the same check for free, unlike a kind-based or parameter-specific rule).

**Complementary, secondary fix, still worth doing**: when a name is genuinely needed (the matching key for real symbols, or a new symbol's display name), derive it from source text via `occ.range` rather than parsing the moniker. This doesn't stop the pollution (the structural check above does that) but fixes `scip_sym_to_name`'s demonstrated fragility more generally — it's used for every definition and reference's name resolution, not just the buggy local/parameter case, and this bug is evidence the string-parsing approach misses real, common shapes.

**Downstream effect, not a separate fix**: `Pass 2` (`CALLS` edges) and `Pass 3` (`INHERITS` edges) both build exclusively off structures populated during Pass 1 (`file_name_to_ids`, `file_symbols`, `scip_sym_to_file_name`). Once Pass 1 stops inserting bogus local-variable nodes into those structures, Pass 2/3 automatically stop being polluted by them — no change needed at those call sites beyond keeping the existing `starts_with("local ")`/`starts_with('<')` checks as a cheap fast-path (still valid, still catches real cases before any kind lookup is needed).

## Proposed implementation shape

- Add `fn is_member_of_known_symbol(scip_sym: &str, known: &HashMap<String, (String, String)>) -> bool`: if `scip_sym` doesn't end in `)`, return false; otherwise find the matching `(` for the trailing group (`rfind('(')`), strip it, and check whether the remainder is a key in `known` (`scip_sym_to_file_name`, already built before Pass 1 runs).
- In Pass 1 (line ~151 `else` branch): before creating a new symbol, call `is_member_of_known_symbol(scip_sym, &scip_sym_to_file_name)`; if true, skip the occurrence entirely — do not add it to `new_symbols`, `file_name_to_ids`, or `file_symbols`. (Note `scip_sym_to_file_name` is keyed on the raw, unmodified `occ.symbol` string — line ~103 inserts `occ.symbol.clone()` — so the stripped candidate must be compared against raw moniker strings too, not post-`scip_sym_to_name` names.)
- Replace `scip_sym_to_name`'s body with source-text extraction: given `occ.range` and the file's source, slice the exact span and return that substring (trimmed). Keep the function name/signature so all call sites (matching-key construction at lines ~102, ~137, and any others) get the fix for free. Fall back to the current string-parsing logic only if the source file can't be read or the span is degenerate (defense in depth, not the primary path).
- Leave the 4 existing `starts_with("local ")`/`starts_with('<')` checks in place — cheap, still correct (confirmed: real `local N` symbols still exist and are still the right thing to filter that way), still worth short-circuiting on before the new structural check.

## Verification

Checked before writing the implementation plan, using a locally-built `scip-typescript 0.4.0` index against small reproduction fixtures (throwaway `#[test]` in `scip/mod.rs` dumping real parsed `Index` contents, discarded — not committed):

1. **`kind` is always `UnspecifiedKind`** — for the module, the function, every parameter, and even genuine `local N` symbols. Confirms the original kind-gating design was not viable; rejected before implementation, per the plan below.
2. **A plain `const`/`let` inside a function body** → genuine `local N` symbol (e.g. `local 2`), already correctly filtered by the existing `starts_with("local ")` check.
3. **A simple (non-destructured) function parameter** → exactly the `<method-descriptor>().(<paramName>)` shape with no `local ` prefix — reproduced byte-for-byte against the real screenshot's moniker structure using a function with several such parameters named to match (`rulesBag`, `clauseGroupRules`, `counter`, `groupDedupeMap`). This confirms the screenshot's entries are parameters of `mintStructuredChoiceArm`, not arbitrary body-local variables.
4. **A destructured, type-annotated parameter** (`{ rulesBag, ... }: Args`) produces a *different* shape — a `local N` binding symbol plus a separate reference occurrence to the interface's own property symbol (`Args#rulesBag.`) at the same range — already correctly excluded by the existing filter, not a source of this bug.
5. **Structural relationship confirmed exact**: parameter moniker = enclosing method's own moniker (always ends in `.`) with `(<paramName>)` appended, nothing else. This is the basis for the `is_member_of_known_symbol` check above — string containment against `scip_sym_to_file_name`'s existing keys, not a re-derived heuristic.

## Testing

Needs a targeted unit test in `crates/infigraph-core/src/scip/mod.rs`'s existing test module reproducing this exact shape:
- Construct a synthetic SCIP `Index` (in-memory, matching this file's existing test patterns for `import_scip_index`) with one real function definition and one parameter occurrence whose symbol is `<that function's own moniker>.(<paramName>)` — mirroring the verified real shape above, `kind` left `UnspecifiedKind` to match reality.
- Assert: no new `Symbol` node is created for the parameter (query the graph after import, confirm no node with that name or at that tiny span exists).
- Assert: a legitimate call from within that function to some other real function still resolves `container_id` correctly to the enclosing function, not to the (would-be) bogus parameter node — this is the regression test for the containment-hijacking risk, and is the test that would have caught this class of bug before it shipped.
- A second, simpler test for the `scip_sym_to_name` replacement: given a known `(occ.range, source)` pair, assert the extracted name matches the literal source text, for both a normal top-level symbol and a parameter-shaped one (even though the latter should now be filtered before name extraction ever matters for graph purposes, the function should still behave correctly if called directly).

## Confirmed / no longer open

- ~~Whether `SymbolInformation.kind` is reliable~~ — checked, it is not (always `UnspecifiedKind`); design no longer depends on it at all.
- ~~Whether a newer `scip-typescript` might behave differently~~ — checked: the bare `scip-typescript` npm name is a squatted/deprecated placeholder; the real, actively-maintained package is `@sourcegraph/scip-typescript` (also what `crates/infigraph-cli/src/scip_download.rs`'s `CATALOG` installs, unpinned). `0.4.0` is its current latest and what all verification above used — re-confirmed with the exact `index --infer-tsconfig --output ...` invocation infigraph itself runs (`scip_download.rs:97`), not just a bare `index` call. Same shape, `kind` still always `UnspecifiedKind`.
- ~~Whether the decompose/match-against-known check needs to apply anywhere beyond Pass 1's new-symbol decision~~ — checked against real data: indexed a `class Button implements Widget` and inspected `si.relationships` directly. Both `si.symbol` (source) and `rel.symbol` (target) are always clean, top-level Class/Interface/Method monikers, never a nested `.(paramName)`-shaped descriptor — structurally, only types/methods can implement/be implemented, never a parameter. Pass 3's existing exact-match lookup is already correct; no decompose fallback needed there. Pass 2 needs no separate change either — once Pass 1 stops creating the bogus node, Pass 2's `file_name_to_ids` lookup for any reference to it simply finds nothing and skips, exactly like today's handling of any other unresolvable reference. **The decompose check is scoped to exactly one call site: Pass 1's new-symbol decision.**

## Scope

In scope: `crates/infigraph-core/src/scip/mod.rs` only — `import_scip_index`, `scip_sym_to_name`, and the new kind-classifier. `infigraph-core` is a shared/foundational path (per this repo's `CLAUDE.md`) — run targeted `scip::` tests first, then full `cargo test --all` before considering this done, and check blast radius (callers of `scip_sym_to_name` / `import_scip_index`) before editing.

Explicitly out of scope: `DocIndex::init()`'s unrelated wipe-on-any-open-failure behavior (a different subsystem — `infigraph-docs`, not SCIP import — flagged separately as a real but distinct hardening gap during DocWatch Task 2's debugging).

## Confidence and recommendation

**Confidence: High** — root cause traced against the literal string from the live screenshot; both bugs independently confirmed by manually stepping through the actual function logic; the original kind-based design was checked against real `scip-typescript` output *before* being finalized and found non-viable, replaced with a structural match-against-already-known-symbols check verified byte-for-byte against a reproduction of the exact real screenshot shape; the fix targets the single point of leverage (Pass 1's new-symbol decision) that also explains and fixes the downstream `CALLS`-edge contamination risk without a separate change.

**Recommendation**: proceed to a full implementation plan (TDD, subagent-driven). No remaining open verification items — the one real assumption in the original draft (kind-field reliability) was checked against real data and the design updated accordingly rather than shipped on an unverified premise.
