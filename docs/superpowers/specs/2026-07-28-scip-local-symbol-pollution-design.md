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

Four call sites (lines ~99, ~133, ~318, ~417) skip an occurrence if `occ.symbol.starts_with("local ") || occ.symbol.starts_with('<')`. This correctly catches SCIP-spec `local <id>` symbols and synthetic (`<...>`) symbols. It does **not** catch `scip-typescript`'s actual encoding for these local variables: a full global-style moniker with a nested term/parameter descriptor immediately following the enclosing method's own `()` — `<method-descriptor>().(<localName>)`. No `local ` prefix, so it sails straight past the filter.

**2. `scip_sym_to_name`'s fallback silently returns the raw moniker for exactly this shape.**

`scip_sym_to_name` (line ~516) is supposed to extract a clean short name from a moniker. Tracing it against the string above: it correctly strips the trailing `.(rulesBag)` group, leaving `...mintStructuredChoiceArm()`. The next step (trim trailing `.`) does nothing since it now ends in `)`. The final "bare identifier" fallback requires the string to end in an identifier character — it ends in `)` instead — so the function falls through to `else { scip_sym.to_string() }`, returning the **entire original raw moniker**. This name becomes the graph node's `name` field verbatim (`Pass 1`, line ~137, `let name = scip_sym_to_name(scip_sym)`), matching exactly what's in the screenshot.

**Combined effect**: every local variable inside every SCIP-typescript-indexed function becomes its own real `Symbol` graph node (via the `else` branch at line ~151, since its ugly name never coincidentally matches an existing tree-sitter symbol), each with a tiny line-span matching its declaration. Since `Pass 2`'s reference-to-container resolution (`file_symbols`, sorted smallest-span-first for containment) can pick these bogus tiny-span nodes as `container_id` for any reference that happens to fall on their declaration line, this isn't just cosmetic — it can misattribute real `CALLS` edges away from the true enclosing function.

## Design principle (per discussion)

Two complementary fixes, not one:

1. **Match against existing entries first; only trust SCIP to introduce a genuinely new symbol when the *kind* says it should.** The import already does identity-matching-first for enrichment and for `CALLS`/`INHERITS` edge targets (`file_name_to_ids` lookup by `(file, name)` before falling back to creating anything new) — that part of the architecture is sound. The bug is specifically in the "no match found" fallback for **definitions** (Pass 1, line ~146 `else` branch): it unconditionally creates a new `Symbol` for *any* unmatched definition occurrence, regardless of whether that occurrence is even the kind of thing that should ever be a standalone graph node. SCIP's `SymbolInformation.kind` (`symbol_information::Kind`, already available via `sym_info_map` and already partially consumed by `scip_kind_to_prism`) directly distinguishes `Variable`/`StaticVariable`/`Field`/`SelfParameter`/`Parameter` from real declaration kinds (`Function`, `Method`, `Class`, `Struct`, `Interface`, `Trait`, `Enum`, `Module`, ...). Gating "may this become a new Symbol" on kind is structured, indexer-version-independent data — far more robust than inferring "is this a local" from the shape of the moniker string, which is exactly the kind of parsing convention that varies across SCIP producers and already broke once.

2. **When a name is genuinely needed (matching key or new-symbol name), derive it from source text via `occ.range`, not by parsing the moniker.** Every occurrence already carries its exact line/col span (`parse_range(&occ.range, file)`, used elsewhere). The literal source text at that span is unambiguous ground truth for the identifier's name, immune to moniker-format quirks across SCIP indexers and language versions. This replaces `scip_sym_to_name`'s string-surgery, which is inherently fragile (this bug is evidence: a plausible-looking heuristic missed a real, common shape) and improves matching accuracy broadly — not just for locals, since the same function is used for every definition and reference's name resolution, not only the buggy local case.

Fix #1 is the one that actually stops the pollution (no bogus node, no bogus edge). Fix #2 is complementary hardening that makes name resolution correct in general, including for the (rarer) cases where a local/parameter-shaped symbol legitimately needs a display name (e.g. logging, or the `--detail` view of an enrichment source) and for any other moniker shapes `scip_sym_to_name` might mishandle that we haven't hit yet.

**Downstream effect, not a separate fix**: `Pass 2` (`CALLS` edges) and `Pass 3` (`INHERITS` edges) both build exclusively off structures populated during Pass 1 (`file_name_to_ids`, `file_symbols`, `scip_sym_to_file_name`). Once Pass 1 stops inserting bogus local-variable nodes into those structures, Pass 2/3 automatically stop being polluted by them — no change needed at those call sites beyond keeping the existing `starts_with("local ")`/`starts_with('<')` checks as a cheap fast-path (still valid, still catches real cases before any kind lookup is needed).

## Proposed implementation shape

- Add `fn is_local_scoped_kind(kind: &symbol_information::Kind) -> bool` (or fold into a small classifier) returning true for `Variable | StaticVariable | Field | SelfParameter | Parameter` — mirroring `scip_kind_to_prism`'s existing match arms so the two stay in sync (consider deriving one from the other, or a shared match, to avoid drift as SCIP's kind enum evolves).
- In Pass 1 (line ~151 `else` branch): before creating a new symbol, look up `si.kind` from `sym_info_map`; if `is_local_scoped_kind(...)` is true (or `si` is absent — no SymbolInformation at all is itself a signal this isn't a real declaration worth tracking), skip the occurrence entirely — do not add it to `new_symbols`, `file_name_to_ids`, or `file_symbols`.
- Replace `scip_sym_to_name`'s body with source-text extraction: given `occ.range` and the file's source, slice the exact span and return that substring (trimmed). Keep the function name/signature so all call sites (matching-key construction at lines ~102, ~137, and any others) get the fix for free. Fall back to the current string-parsing logic only if the source file can't be read or the span is degenerate (defense in depth, not the primary path).
- Leave the 4 existing `starts_with("local ")`/`starts_with('<')` checks in place — cheap, still correct, still worth short-circuiting on before any kind lookup or file I/O.

## Testing

Needs a targeted unit test in `crates/infigraph-core/src/scip/mod.rs`'s existing test module reproducing this exact shape:
- Construct a synthetic SCIP `Index` (in-memory, matching this file's existing test patterns for `import_scip_index`) with one real function definition (`Method` kind) and one nested local variable occurrence whose symbol has the `<method-descriptor>().(<name>)` shape but **no** `local ` prefix, with `SymbolInformation.kind` set to `Variable` (or `Parameter`).
- Assert: no new `Symbol` node is created for the local variable (query the graph after import, confirm no node with that ugly name or any node at that tiny span exists).
- Assert: a legitimate call from within that function to some other real function still resolves `container_id` correctly to the enclosing function, not to the (would-be) bogus local node — this is the regression test for the containment-hijacking risk, and is the test that would have caught this class of bug before it shipped.
- A second, simpler test for the `scip_sym_to_name` replacement: given a known `(occ.range, source)` pair, assert the extracted name matches the literal source text, for both a normal top-level symbol and a local/parameter-shaped one (even though the latter should now be filtered before name extraction ever matters for graph purposes, the function should still behave correctly if called directly).

## Confirm before implementing

- Whether `scip-typescript`'s actual `.scip` output reliably sets a meaningful `SymbolInformation.kind` (not `UnspecifiedKind`) for this local-descriptor shape. Design assumes yes (SCIP protocol has explicit `Variable`/`Parameter` kind values scip-typescript is expected to emit), but this should be checked against a real `.scip` file from `sittir` (or scip-typescript's own test fixtures) before relying on it as the primary signal — if it turns out kind is unreliable in practice for this indexer, the shape-based detection from the earlier discussion (`.trim_end_matches(')')`, find matching `(`, check what remains ends in `)`) should be added as a second, independent gate rather than the sole one.
- `scip_kind_to_prism`'s current fallback (`_ => SymbolKind::Function`) is separately worth revisiting once this lands — if `kind` is genuinely absent/unspecified for some real function definitions too, an over-eager local-filter keyed only on kind could wrongly drop them. The "no `SymbolInformation` found at all" case in the new Pass 1 gate should probably NOT auto-skip (only explicit local-scoped kinds should), to avoid this failure mode — worth a dedicated test case either way.

## Scope

In scope: `crates/infigraph-core/src/scip/mod.rs` only — `import_scip_index`, `scip_sym_to_name`, and the new kind-classifier. `infigraph-core` is a shared/foundational path (per this repo's `CLAUDE.md`) — run targeted `scip::` tests first, then full `cargo test --all` before considering this done, and check blast radius (callers of `scip_sym_to_name` / `import_scip_index`) before editing.

Explicitly out of scope: `DocIndex::init()`'s unrelated wipe-on-any-open-failure behavior (a different subsystem — `infigraph-docs`, not SCIP import — flagged separately as a real but distinct hardening gap during DocWatch Task 2's debugging).

## Confidence and recommendation

**Confidence: High** — root cause traced against the literal string from the live screenshot, both bugs independently confirmed by manually stepping through the actual function logic, and the fix targets the single point of leverage (Pass 1's new-symbol decision) that also explains and fixes the downstream `CALLS`-edge contamination risk without a separate change.

**Recommendation**: proceed to a full implementation plan (TDD, subagent-driven) once the "confirm before implementing" item above (kind-field reliability against a real `.scip` fixture) is checked — that's the one assumption in this design that isn't yet verified against real data, and it determines whether kind-gating alone suffices or needs the shape-based check as a second signal from day one.
