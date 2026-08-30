# Symbol Identity and Scoping Hardening

## Context and Motivation

This spec grew out of a brainstorm on a four-part enhancement request:

> a. Update the code graph with SCIP symbol names/ids — maybe just the descriptor path will be
>    sufficient b/c our indices are repo local anyway
> b. Enhance our shipped entity.scm to build SCIP-compatible ids
> c. Consider migrating from our proprietary symbol ids to those
> d. Add generic support for doc ↔ code links in (the shape of SCIP symbols or our current id
>    format…) and/or in-code comments

Before designing a new id *shape*, the investigation surfaced real, confirmed correctness bugs in
the *current* id scheme — independent of SCIP, rooted in how symbols get scoped and deduplicated,
not in what format their ids use. Redesigning the id format on top of a scoping mechanism that
already silently drops and collides symbols would just give the new format the same failures. This
spec scopes and sequences fixing that foundation. It supersedes items (a)/(b)/(c) above in
priority — they're revisited at the end, not dropped.

Item (d) (doc ↔ code links) is untouched by this spec and can proceed independently at any time.

## Root-Cause Findings

Each finding below was verified against source and, where noted, an empirical reproduction — not
assumed from documentation or memory.

### Finding 1 — SCIP enrichment's `(file, name)` lookup key collapses overloads

`crates/infigraph-core/src/scip/mod.rs:121` keys `file_name_to_ids` as
`HashMap<(String, String), Vec<String>>` — file and bare name, no disambiguator. This causes:
- `docstring` last-write-wins across colliding occurrences (the `Vec<String>` value plus a
  `for sid in ids` enrich-all loop enrich *every* id sharing that name, not just the right one).
- `CALLS` and `INHERITS` edge resolution pick an arbitrary (`.first()`) match among the colliding
  candidates.

SCIP's own disambiguator, present in `occ.symbol` and parsed by `scip_sym_to_name`, is discarded
before the lookup — this was a known, accepted simplification (the `Vec` return type and
enrich-all loop are the evidence), not an oversight.

### Finding 2 — tree-sitter's own extraction already collapses same-id symbols (deeper root cause)

`crates/infigraph-core/src/graph/store_parquet.rs` (~lines 270–320) guards every symbol push with
`if sym_seen.insert(sym.id.clone())` — the *second* symbol sharing an id string is silently
dropped before it ever reaches the graph. This happens entirely independent of SCIP. Fixing SCIP's
lookup key alone is therefore necessary but **not sufficient**: even with a perfect SCIP-side key,
there is fundamentally only one graph node to enrich when two source-level symbols share an id.

### Finding 3 — Rust's `impl_item` has no `name` field (confirmed empirically)

`find_parent_class` (`crates/infigraph-core/src/extract/entities.rs:333-389`) walks up the AST
looking for `CLASS_KINDS` (including `impl_item` for Rust) and extracts the enclosing node's
`name` field to build `Symbol.id`/`Symbol.parent`. Checked against
`tree-sitter-rust-0.24.2`'s real `node-types.json`: `impl_item`'s only fields are `body`, `trait`,
`type`, `type_parameters` — **no `name` field exists**. `child_by_field_name("name")` silently
returns `None`, so every Rust impl method (inherent or trait) falls back to a flat `file::method`
id with no type-scoping at all.

Empirically reproduced: indexed a fixture with `struct Alpha` (`impl Alpha { fn hello(...) }`)
and `struct Beta` (`impl Greet for Beta { fn hello(...) }`, same signature). Result:

```
Class Alpha    id=src/main.rs::Alpha
Class Beta     id=src/main.rs::Beta
Class Greet    id=src/main.rs::Greet
Method hello   L9-11  id=src/main.rs::hello     <- only Alpha's survives
```

`Beta::hello` is entirely absent from the graph, not merely mis-tagged. This also disproves
"just add a signature-string disambiguator" as a fix for this case: `Alpha::hello` and
`Beta::hello` have *identical* signatures (`fn hello(&self) -> String`) — the missing ingredient
is parent scoping, not overload disambiguation.

### Finding 4 — `find_parent_class` and `find_enclosing_class` are a duplicated, independently-buggy pair

`find_enclosing_class` (`crates/infigraph-core/src/extract/relations.rs:354-384`) implements
nearly identical `CLASS_KINDS`-walk logic to `find_parent_class`, copy-pasted into a separate
file. The two have already drifted (this copy has an extra Pascal `declClass`/`declIntf` branch
the other doesn't) — a live DRY violation regardless of its exact call sites. It shares the
identical `impl_item` gap from Finding 3.

**Correction (found while scoping Phase 1's implementation):** `find_enclosing_class` is used
*only* to resolve `self`/`this`/`@` receivers to a class name for method-call resolution
(`relations.rs:174`) — a narrower case than originally assumed here, not a general relation-source
qualifier. It does **not** build a `CALLS` edge's caller-side id. That's a separate mechanism —
see Finding 5's correction below.

### Finding 5 — The pattern generalizes: `find_enclosing_function` and C++ type-resolution walks

`find_enclosing_function` (`relations.rs:252-351`) attributes an unqualified call to its enclosing
function (the caller side of `CALLS` edges). It has grown far more organically than the
class-scoping walks: a C# `accessor_declaration → property_declaration` indirection (with a
comment documenting a specific past incident — a silently-dropped `CALLS` edge from inside a C#
property getter), a C/C++ branch through `find_cpp_function_declarator`/`rightmost_identifier`, a
Pascal `defProc`/`genericDot` branch, and SQL `create_table`/`insert`/`cte` container handling.
`find_field_type_in_enclosing_class` (`relations.rs:400-454`) and
`find_param_type_in_enclosing_function` (`relations.rs:467-502`) are narrower, C++-only
declared-type inference walks in the same imperative style.

**Correction (found while scoping Phase 1's implementation):** a `CALLS` edge's caller-side id is
built via `find_enclosing_function` returning the **bare** function/method name — never
class-qualified — plus `format!("{}::{}", file, src)` (`relations.rs:165,208`), later reconciled
in `resolve_calls.rs`'s `resolve_calls` against a `symbol_map` keyed purely by bare name across
the whole file. This means Rust's `impl_item` gap has a real consequence here too, but a
*different* one than Finding 4 originally claimed: before Phase 1, `Alpha::hello`/`Beta::hello`
collapse into one bogus node, so `symbol_map["hello"]` has exactly one candidate — no visible
ambiguity, just a wrong node. After Phase 1 correctly makes them distinct symbols,
`symbol_map["hello"]` has *two* legitimate same-file candidates, and `find_enclosing_function`
still never returns anything class-qualified — a call made from inside either impl's `hello` body
can be attributed to the wrong one. This is scoped to Phase 3+4 (issue tracking below), not
Phase 1.

Structurally, **none of `infigraph-core/src/extract/{entities,relations}.rs` is organized per
language** — confirmed via directory listing: `crates/infigraph-core/src/extract/` contains only
`entities.rs`, `relations.rs`, `mod.rs`. Every language's node-kind knowledge for these walks is
inline string literals and `if`-branches inside a handful of shared functions. This is the
opposite of `crates/infigraph-languages/languages/` (59 per-language directories, each with its
own `entities.scm`/`relations.scm`, and for at least Rust and Python, `inherit_decompose.scm`) —
the pattern the rest of the extraction pipeline already uses successfully.

### Finding 6 — An `IMPLEMENTS` edge already exists in the type system but is unused

`RelationKind::Implements`/`ImplementedBy` are defined (`crates/infigraph-core/src/model/mod.rs:82-98`)
but nothing ever produces them. Rust's `impl Trait for Type` is captured by `relations.scm`'s
`@inherit.parent`/`@inherit.child` captures and classified as `Inherits`, not `Implements` — a
capture-naming choice, not a schema limitation. The current graph schema materializes only
`INHERITS` (confirmed via `get_graph_schema`). This edge is also **type-level only**
(`Beta → Greet`) — there is no method-level edge (`Beta::hello → Greet::hello`), so "find all
implementations of this trait method" isn't answerable even at the granularity that exists today.
A method-level edge is blocked on Finding 3: `Beta::hello` must exist as a correctly-scoped,
distinct symbol before it can be an edge endpoint.

### Finding 7 — The pattern to generalize toward already exists and works

`ParserBackend::TreeSitter`'s `inherit_decompose_query: Option<Box<Query>>` field
(`crates/infigraph-core/src/lang/mod.rs:41`) plus `resolve_inherit_text`
(`relations.rs:621-644`) is a fully generic `(node, source, decompose_query) -> String` helper:
it iteratively re-applies a declarative query to descend through compound wrapper nodes
(`generic_type`, `scoped_type_identifier`, …) until it bottoms out at a plain identifier — no
hardcoded per-node-kind Rust branching. Rust's `inherit_decompose.scm`
(`[(_ name: (_) @candidate) (_ type: (_) @candidate)]`) already handles exactly the shape an
`impl_item`'s `type:` field can take when compound (e.g. `impl<T> Foo<T> for Vec<Bar>`). This
machinery is directly reusable — not just analogous — for the entities-side fix in Phase 1.

Also confirmed as a non-issue: query-compilation caching. `Query::new()` runs once per language
inside `LanguagePack::new()` (`lang/mod.rs:57-77`), itself called once at `bundled_registry()`
startup; every file of that language during a run reuses the same compiled `Query`. No per-file
recompilation exists today, so there is nothing to add here.

## Design Principles

1. **Capture-with-fallback migration idiom.** Established precedent:
   `@func.params`/`@method.params` and `@func.return_type`/`@method.return_type` are optional
   captures, consumed preferentially, falling back to the old field-lookup mechanism when absent.
   The same shape applies here: add `@method.parent`/`@class.parent`/etc. as optional captures;
   consume them ahead of the imperative walk; delete the walk for a given language only once its
   `.scm` file supplies the capture.
2. **Reuse decompose queries, don't duplicate them.** `inherit_decompose_query` is already
   generic; entities-side consumption should call the same helper/query rather than growing a
   second copy.
3. **Per-language knowledge belongs in per-language files.** The target is not "queries instead
   of Rust" in the abstract — it's relocating each language's scoping/attribution logic into that
   language's own directory under `infigraph-languages/languages/<lang>/`, matching the pattern
   already proven across 59 languages. `infigraph-core`'s shared code should shrink to
   capture-consuming machinery plus a generic fallback for languages that haven't migrated yet.

## Phased Decomposition

### Phase 1 — Consolidate + fix the `impl_item` gap (bounded, do first)

- Merge `find_parent_class` (`entities.rs`) and `find_enclosing_class` (`relations.rs`) into one
  shared function, removing the Finding 4 duplication.
- Add `type: (_) @method.parent` to Rust's `entities.scm` impl-method pattern; resolve it via
  `resolve_inherit_text` + the pack's existing `inherit_decompose_query` (Finding 7's mechanism,
  reused not duplicated).
- Fixes the entity-id collapse (Finding 3). **Correction:** an earlier version of this spec
  claimed this phase also fixes a parallel `CALLS`-edge caller-attribution bug — that was wrong.
  `find_enclosing_class` (the function this phase consolidates) is used only for `self`/`this`
  receiver resolution, not caller-attribution; the real caller-attribution gap (Finding 5's
  correction) lives in `find_enclosing_function`, out of scope for Phase 1, tracked under
  Phase 3+4 instead.
- No schema change, purely additive to `entities.scm`/`entities.rs`; does not touch
  `relations.scm`. Clean for both fork and upstream.
- **Known residual gap, explicitly not fixed by this phase:** a single type with two same-named
  methods from different sources — e.g. `impl Bar { fn x() }` and
  `impl SomeTrait for Bar { fn x() }` on the *same* `Bar` — both resolve `@method.parent` to
  `Bar`, producing the identical id `file::Bar::x`. This is a genuine overload collision within
  one parent, not a scoping bug, and needs Phase 2's disambiguation work, not Phase 1's. Tracked
  as a separate issue so it isn't quietly assumed fixed.
- Regression tests: the Alpha/Beta/Greet fixture that reproduced Finding 3, plus the residual-gap
  case above (asserted as a known-failing/tracked case, not silently passing).

### Phase 2 — SCIP `(file, name)` key + tree-sitter `sym_seen` fix (Findings 1 & 2)

**Phase 2 complete.** `Symbol.scip_id` persisted across all three graph backends (Kuzu, Neo4j,
Cozo) in commit `8e5d13c`, reviving originally-deferred item (a). **Part A** (Finding 2,
tree-sitter's own collapse) implemented in commit `11ff3f0` — resolved `#125` as a side effect.
**Part B** (Finding 1, SCIP's lookup key) implemented in commit `d14c982` — Pass 1 now builds
`scip_sym_to_ts_id` by containment-matching each definition occurrence to its specific
same-named candidate, and Pass 2/3 resolve `CALLS`/`INHERITS` targets via a direct, exact lookup
on that map instead of the lossy `(file, name)` chain. `#126` closed.

Signature-string disambiguation is ruled out: it's indexer/language-specific (mirrors the earlier
finding that SCIP's own disambiguator can't be generically reconstructed), and demonstrably
insufficient on its own (Finding 3's `Alpha::hello`/`Beta::hello` have identical signatures).

**Disambiguation mechanism — decided.** A per-`(file, parent, name)` occurrence ordinal, not a raw
`start_line` embed. Sort same-named symbols within the same `(file, parent)` group by start
position (`Span.start_line`, tie-broken by `start_col` for the rare same-line case), assign each
its rank, and append the ordinal to the id only when there's more than one in the group —
everything else keeps its current, unsuffixed id. Ordinal beats a raw line number for stability:
an edit *above* the colliding pair shifts both their line numbers equally (ordinal unaffected); an
edit *between* them shifts only the second one's line number (ordinal still unaffected, since
relative order didn't change). Ordinal only changes if the declarations are actually reordered
relative to each other — a real identity change, not incidental edit noise.

`file`, `parent`, and `name` are already three independent fields on every `Symbol` — this groups
on values that already exist, nothing to reconstruct. It's also orthogonal to `parent`-correctness:
it groups on whatever `(file, parent, name)` the id-construction code already computes at that
point (Phase 1's fix included), not on some independently-verified ground truth, so it does not
need to wait on Phase 3+4's per-language parent-scoping migration. The same span data (SCIP's
`occ.range`, already parsed via `parse_range` but currently unused for correlation) also fixes the
separate SCIP-to-tree-sitter correlation problem — match by span containment instead of
`(file, name)` lookup.

### Phases 3+4 (merged) — Per-language migration off all remaining hardcoded walks

Originally split into "class-scoping" (Phase 3) and "`find_enclosing_function`/C++
type-resolution" (Phase 4), merged into a single per-language effort: the two walks don't cover
identical language sets, but wherever they overlap (C#, C++, Pascal), verifying that language's
grammar and touching its query files once is strictly better than two disconnected global sweeps.
Organized by language/case, not by walk-type — see the tracking issue for the full checklist.
Each case is capture-with-fallback (optional `@*.parent`/`@*.enclosing` capture, consumed
preferentially, old walk kept until every consuming case has migrated), verified against that
language's real bundled `node-types.json` before writing captures — same discipline as the
existing Kotlin/Dart params-coverage work. The consolidated Phase-1 walk function, and
`find_enclosing_function`/the C++ type-resolution walks, are deleted only once every case they
serve has migrated.

### Phase 5 — `IMPLEMENTS` edge (Finding 6)

- Type-level relabel: change Rust's `relations.scm` impl-trait capture classification from
  `Inherits` to `Implements` (or introduce a distinct capture name so both remain available where
  meaningful) — additive, no schema change.
- Method-level edge (`Beta::hello → Greet::hello`): blocked on Phase 1 landing first, since
  `Beta::hello` must exist as a correctly-scoped symbol before it can be an edge endpoint.

## Explicitly Deferred / Out of Scope for This Spec

- **Original item (c)** — migrating the primary symbol-id key to a SCIP-shaped format: remains a
  later *decision*, not a task, per the original decomposition. Revisit only after Phases 1–3 are
  proven; a primary-key change touches every edge type, embeddings, and every downstream tool.
- **Original item (d)** — generic doc↔code link support: independent of everything in this spec,
  can proceed on its own timeline.
- **Original items (a)/(b)** in their originally-proposed form (persist SCIP descriptor path;
  universal SCIP-shaped `entities.scm` ids): superseded in priority by the correctness fixes
  above. A stable, non-colliding id is a precondition for persisting or mirroring SCIP descriptors
  meaningfully — revisit after Phase 2.

## Confidence and Recommendation

**Confidence: High** on all seven findings (each verified against source; Finding 3 additionally
verified by empirical reproduction) and on Phase 1's design. **Medium** on Phase 2's exact
disambiguation mechanism (the open question above is real and unresolved — current lean is a
per-`(file, parent, name)` occurrence ordinal rather than a raw `start_line` embed, since ordinal
is stable against edits that shift line numbers without reordering the colliding declarations
themselves; only symbols that would otherwise collide get a suffix at all). **Recommendation:**
proceed with Phase 1 as a bounded implementation now; track Phase 2 and Phases 3+4 (merged) and
Phase 5 as separate GitHub issues referencing this spec, so each can be picked up and scoped
independently without blocking on the others.

## Issue Tracking

- Phase 1 — implemented directly (bounded), no tracking issue. **Done** (`f29a1ea`).
- Phase 1 residual gap (same-type overload collision) — `pradeepmouli/infigraph#125`.
  **Closed** — resolved by Phase 2 Part A (`11ff3f0`).
- Phase 2 (SCIP key + `sym_seen` fix) — `pradeepmouli/infigraph#126`. **Closed** — Part A
  (`11ff3f0`) + `scip_id` persistence (`8e5d13c`) + Part B (`d14c982`).
- Phases 3+4, merged (per-language migration of all remaining hardcoded walks) —
  `pradeepmouli/infigraph#127`. Open.
- Phase 5 (`IMPLEMENTS` edge) — `pradeepmouli/infigraph#129`. Open.
