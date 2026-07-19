# Design: Pattern-Detection Precision + Pluggability

Status: draft
Author: (session 2026-07-18)
Scope: `crates/infigraph-core/src/patterns/mod.rs` — make the 5 existing GoF
pattern detectors pluggable via a real trait + registry, propagate errors
instead of silencing them, and reduce false positives in Singleton and
Observer by adding a structural check alongside their existing name-matching.
Numeric confidence scoring and precision work on Factory/Strategy/Decorator
are explicitly deferred to a later, data-informed pass.

## TL;DR

Today, `detect_all` is five hardcoded function calls, `detect_filtered` runs
all of them and throws away everything but the name the caller asked for
(silently returning an empty report for an unknown name), and two of five
detectors (Singleton, Observer) are pure name-matching with a hardcoded
`"high"` confidence — no structural signal at all, unlike the other three.

This design: (1) introduces a `PatternDetector` trait + `PatternRegistry`
(mirroring the existing `LanguageRegistry` register-many/lookup shape) so
detectors are looked up by name instead of hardcoded, (2) makes detection
`Result`-returning end-to-end with no silent failures — an unknown pattern
name is a hard error, a detector that errors during execution doesn't block
the others but its failure is surfaced in the report, not dropped, and
(3) gives Singleton and Observer a structural check that grades their
confidence (high/medium or high/low) instead of gating inclusion, bringing
them in line with how Factory/Strategy/Decorator already work.

## Problem / Motivation

- **No pluggability.** `detect_all` is five hardcoded calls; adding, removing,
  or listing detectors means editing this function directly.
- **Silent failure on unknown pattern name.** `detect_filtered` runs
  `detect_all` then filters by name; an unrecognized name silently returns an
  empty report instead of telling the caller they asked for something that
  doesn't exist.
- **Singleton and Observer are unusually weak.** Both are pure
  name-heuristic matches (Singleton: fixed accessor-method name list;
  Observer: `register*`/`subscribe*` paired with `notify*`/`emit*` on the same
  class) with confidence hardcoded to `"high"` regardless of any structural
  evidence. Factory, Strategy, and Decorator all already combine a structural
  graph check (`INHERITS`/`CALLS` edges) with name-based confidence tiering —
  Singleton and Observer are the outliers, not the norm.
- **No detection-logic test coverage.** The existing `mod tests` in
  `patterns/mod.rs` only test output formatting (`format_report`,
  `format_json`, `strip_quotes`) against hand-built `PatternReport` values.
  Nothing exercises `detect_factory`/`detect_singleton`/etc. against a real
  graph — a real gap for a phase whose entire point is precision.

## Approaches considered

- **A. Bare function-pointer table.** Simplest, but no shared trait means no
  common interface for cross-cutting concerns (naming, error handling) and no
  natural extension point. Rejected — too weak for "pluggable."
- **B. Real trait, static array, no registry type.** A `PatternDetector`
  trait with a `const DETECTORS: &[&dyn PatternDetector]`. Simpler than a
  registry struct, but doesn't match how the rest of the codebase does
  register-many/lookup (`LanguageRegistry`), and gives up an obvious home for
  `find()`-by-name logic.
- **C. Real trait + `PatternRegistry` struct (chosen).** Mirrors
  `LanguageRegistry`'s `new()` + `register()` + lookup/iterate shape, which is
  the established precedent in this codebase for "register many
  implementations, look one up or iterate all." `EmbedProvider`/
  `GraphBackend` were also examined as trait precedents, but they're
  pick-one-active-implementation patterns (`Arc<dyn Trait>` + `OnceLock`),
  not register-many-and-enumerate — the wrong shape for a detector set.

Phasing: ship the registry + error-propagation + Singleton/Observer precision
upgrade now. Defer graduated *numeric* confidence scoring (replacing the
`String` field) and further precision work on Factory/Strategy/Decorator to a
later, data-informed pass — this phase is about closing the gap between the
two weakest detectors and the other three, not re-designing confidence
scoring wholesale.

## Architecture

```rust
pub trait PatternDetector: Sync {
    fn name(&self) -> &'static str;
    fn detect(&self, backend: &dyn GraphBackend) -> Result<Vec<PatternMatch>>;
}

// ZST marker structs, each delegating to the existing detect_x function
// (updated to return Result instead of Vec directly)
struct FactoryDetector;
struct SingletonDetector;
struct ObserverDetector;
struct StrategyDetector;
struct DecoratorDetector;

impl PatternDetector for FactoryDetector {
    fn name(&self) -> &'static str { "Factory" }
    fn detect(&self, backend: &dyn GraphBackend) -> Result<Vec<PatternMatch>> {
        detect_factory(backend)
    }
}
// ... one impl per detector, same shape

pub struct PatternRegistry {
    detectors: Vec<&'static dyn PatternDetector>,
}

impl PatternRegistry {
    pub fn new() -> Self {
        let mut r = Self { detectors: Vec::new() };
        r.register(&FactoryDetector);
        r.register(&SingletonDetector);
        r.register(&ObserverDetector);
        r.register(&StrategyDetector);
        r.register(&DecoratorDetector);
        r
    }

    fn register(&mut self, d: &'static dyn PatternDetector) {
        self.detectors.push(d);
    }

    pub fn all(&self) -> &[&'static dyn PatternDetector] {
        &self.detectors
    }

    pub fn find(&self, name: &str) -> Option<&'static dyn PatternDetector> {
        self.detectors.iter().copied().find(|d| d.name().eq_ignore_ascii_case(name))
    }
}
```

This mirrors `lang/registry.rs::LanguageRegistry`'s access pattern
(`new()` + `register()` + lookup/iterate) rather than inventing a new registry
idiom, and uses a real trait (unlike `LanguagePack`, which is a plain data
struct) because detectors have behavior (`detect()`), not just data.

## Components

Each `detect_x` function changes signature from `Vec<PatternMatch>` to
`Result<Vec<PatternMatch>>` so errors propagate instead of being swallowed at
the call site. Behavior changes are scoped to two detectors:

- **Factory, Strategy, Decorator** — unchanged in this phase. Already
  structural (`INHERITS`/`CALLS` edges) with name-based confidence tiering.
  Deferred to the future data-informed pass per the phasing decision above.

- **Singleton** — inclusion criteria unchanged (same name-based accessor-name
  match). New: confidence becomes `"high"` if there is no external `CALLS`
  edge into the class's constructor (nothing else constructs it directly —
  consistent with the singleton contract), or `"low"` if something else does
  construct it directly (structural evidence against the pattern, surfaced
  rather than dropped).

- **Observer** — inclusion criteria unchanged (same `register*`/`subscribe*`
  + `notify*`/`emit*` name-pair match on the same class). New: confidence
  becomes `"high"` if the `notify*`/`emit*` method has at least one outbound
  `CALLS` edge, or `"medium"` if it has none. This is a real limitation, not
  the "ideal" check: the ideal would be verifying `notify*` iterates a stored
  collection of registered listeners, but `graph/schema.rs::CREATE_SCHEMA`
  (read in full) has no `Field`/collection-type modeling, so that check is
  not implementable against the current schema. The `CALLS`-edge check is the
  closest realistic substitute and is stated here as a known limitation, not
  oversold as equivalent to the ideal check.

Both upgrades **grade** confidence rather than gate inclusion, matching the
established convention from Factory/Strategy/Decorator rather than
introducing a new hard-filter behavior inconsistent with the rest of the
module.

`PatternMatch.confidence` stays a `String` (`"high"`/`"medium"`/`"low"`) —
not converted to a numeric score. That conversion is explicitly deferred to
a future, data-informed pass per the phasing decision.

## Error handling

- `detect_all(backend: &dyn GraphBackend) -> Result<PatternReport>`: builds
  `PatternRegistry::new()`, runs every registered detector even if one fails
  (no fail-fast — one detector's bug shouldn't discard four other detectors'
  working results). Successes accumulate into `patterns`; failures accumulate
  into a new `errors: Vec<(String, String)>` field on `PatternReport`
  (detector name, error message) instead of being dropped.
- `detect_filtered(backend, Some(name)) -> Result<PatternReport>`:
  `PatternRegistry::new().find(name)` → `None` is a **hard `Err`**
  (`"unknown pattern '{name}', available: {...}"`) — this is a caller
  mistake (asking for a detector that doesn't exist), not a runtime failure,
  so it fails immediately rather than silently returning an empty report
  (the current, wrong behavior). `Some(detector)` runs that single detector
  through the same collect-don't-abort path as `detect_all` (a runtime error
  from a *known* detector goes into `errors`, not a hard failure — the
  detector existing and running is not a caller mistake).
- `detect_filtered(backend, None) -> Result<PatternReport>`: delegates to
  `detect_all(backend)`.

```rust
pub struct PatternReport {
    pub patterns: Vec<PatternMatch>,
    pub errors: Vec<(String, String)>, // (detector_name, error_message)
}
```

## Data flow

The only current caller is the CLI — confirmed via `search_code` grep (not
just `trace_callers`, which missed the cross-crate edge): `analysis_commands
::cmd_detect_patterns` (`crates/infigraph-cli/src/analysis_commands.rs:349-
357`) calls `prism.backend().context(...)?` then
`patterns::detect_filtered(backend, pattern)`. No MCP tool wraps pattern
detection today, so no MCP-side changes are needed for this phase.

Note: as of the 2026-07-18 upstream sync (v3.0.0), `Infigraph::backend()`
returns `Some` for every backend kind including the default local Kuzu
backend (previously `None` — Kuzu callers went through a separate
`GraphStore`/`GraphQuery` path). `patterns::detect_all`/`detect_filtered`
were updated upstream as part of that same refactor to take
`backend: &dyn GraphBackend` directly instead of `&GraphStore`, so this
design's registry/detector plumbing targets that signature throughout.

```
cmd_detect_patterns(pattern: Option<&str>)
  -> prism.backend().context("graph not initialized...")?
  -> patterns::detect_filtered(backend, pattern)
       pattern = Some(name):
         PatternRegistry::new().find(name)
           None  -> Err("unknown pattern '{name}', available: {...}")
           Some(d) -> d.detect(backend) -> single-detector PatternReport
       pattern = None:
         -> detect_all(backend)
              PatternRegistry::new().all().iter()
                -> each d.detect(backend)  [Cypher via backend.raw_query() + Rust post-processing]
                -> Ok(matches)  => patterns.extend(matches)
                -> Err(e)       => errors.push((d.name(), e.to_string()))
              -> PatternReport { patterns, errors }
  -> format_report(&report) / format_json(&report)   [CLI output]
```

**Formatter obligation:** adding `errors` to `PatternReport` without updating
`format_report`/`format_json` would silently drop errors at the presentation
layer — the exact failure mode this design otherwise eliminates, just moved
one layer up. Both formatters must be updated in this phase to print a short
summary when `errors` is non-empty (e.g. "N detector(s) failed: Strategy —
<message>").

## Testing

Existing coverage gap: `patterns/mod.rs`'s `mod tests` (L525-590) only tests
`format_report`/`format_json`/`strip_quotes` against hand-built
`PatternReport` values — no test exercises actual detection logic against a
real graph.

Fixture convention already established in this codebase: integration tests
under `crates/infigraph-core/tests/` (`modules.rs`, `features.rs`) each
define their own local `setup_graph() -> TestGraph` helper that indexes real
fixture source files and exposes a `backend: KuzuBackend` field directly —
`KuzuBackend` implements `GraphBackend`, so `tg.backend` can be passed
straight into `detect_x(&tg.backend)` calls with no wrapper needed.
Confirmed via `search_code` that this helper is duplicated per file rather
than shared, so a new test file follows the same local-fixture convention.

Additions for this phase:

1. New `crates/infigraph-core/tests/patterns.rs` integration test file with
   its own `setup_graph()`-style fixture, covering true-positive and
   near-miss cases per detector (an `INHERITS` chain for Strategy/Decorator,
   a `CALLS` edge for the Observer notify-check, an externally- vs.
   internally-constructed class for Singleton).
2. **Required regression tests for Singleton and Observer specifically** —
   each needs a case that would have been a false positive (or wrongly
   `"high"` confidence) under the *old* pure-name-matching code, and is
   correctly downgraded under the new structural check. These must be
   verified to fail without the fix and pass with it.
3. Registry-level unit tests (no graph needed, in `patterns/mod.rs`'s
   existing `mod tests`): `PatternRegistry::new().all()` has the expected 5
   entries; `.find()` is case-insensitive and returns `None` for unknown
   names; `detect_filtered` with an unknown name returns `Err`.
4. `detect_all` error-aggregation unit test: a throwaway test-only
   `PatternDetector` impl that always returns `Err`, constructed directly
   (not via the production registry) alongside real detectors, proving one
   failing detector doesn't block the others' results.
5. Formatter tests: extend the existing hand-built-`PatternReport` tests to
   cover a non-empty `errors` case for both `format_report` and
   `format_json`.

## Out of scope (deferred)

- Numeric/graduated confidence scoring (replacing `PatternMatch.confidence:
  String` with a numeric score). Deferred to a future, data-informed pass —
  needs real usage data on false-positive rates to calibrate, which this
  phase doesn't have yet.
- Precision improvements to Factory, Strategy, or Decorator. These already
  have structural checks; revisiting them is scoped to the same future pass
  as numeric scoring, once real data indicates where their false positives
  actually are.
- A declarative graph-query DSL for pattern rules. Considered and explicitly
  rejected for this phase: `.scm` (tree-sitter query files, used today for
  per-file `entities.scm`/`relations.scm` symbol/relation extraction) cannot
  express cross-file, cross-symbol graph traversal — it operates on a single
  file's AST at parse time, before the graph exists. Cypher (via
  `GraphBackend::raw_query`) is already the graph-query DSL in active use by
  every `detect_x` function; the open question was packaging (inline vs.
  externalized query files), not capability. Decided to keep queries inline
  with their Rust post-processing, since each detector's confidence-grading
  and name-heuristic logic is inherently imperative and doesn't belong in a
  query file — externalizing would only relocate the retrieval half and
  fragment one detector's logic across two files for no benefit at the
  current detector count (5).
- An MCP tool wrapping pattern detection. Today it's CLI-only
  (`cmd_detect_patterns`); adding an MCP tool is a separate, unrequested
  scope expansion.
