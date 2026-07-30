# DESIGN — `infigraph doctor` (R6.4)

**Status:** Approved (design phase) — 2026-07-30
**Scope:** A health-check command for infigraph's own installation state: instance registry, lock files, watchers, disk usage, sidecars, hooks/toolchain, and MCP handshake sanity.

## Motivation

`docs/DESIGN-hardening.md` §6 (R6.4) proposes a single command that runs verify, instance-registry scan, lock status, disk usage per project, sidecar freshness, hook installation/version checks, and toolchain/codesign validity — printing PASS/WARN/FAIL with remediation per check.

On 2026-07-30 this was tested manually (via an ad hoc audit, not a real command) and found real, damaging drift:

- **Registry/disk drift** — 5 fully-indexed projects (`lspeasy`, `langium-zod`, `to-skills`, `zod-to-form`, `vectormark`) existed on disk with valid `.infigraph/` state but were missing from the instance registry entirely.
- **Disk-space damage** — the machine was down to 9.4GB free. This directly crashed the MCP server: a `mcp.lock` heartbeat failure, Kuzu read-only/empty-database errors, and a hard crash mid-`index_project` on the `sittir` project (client-side "Connection closed" at the same timestamp as a quarantined graph directory `sittir/.infigraph/graph.corrupt.1785386139`). The server self-recovered, but only because quarantine-on-corruption (PR9) was already in place — without it this would have wiped real data.
- **Stale sidecars** — the same 5 unregistered projects also had embeddings ~14 hours older than their graph (a reindex ran without refreshing them).
- **Watcher liveness gaps** — `cli-watch` lock files never update their `last_heartbeat` field (PR7b wired heartbeats into `mcp.lock` only, not `cli-watch`), so a frozen daemon watcher is indistinguishable from an idle one using lock state alone. `get_watch_status` additionally does not report daemon code watchers at all — diagnosing this required manually correlating `lsof` output against lock file contents.
- **Stale/zero-byte locks** — a `vectormark` `watch.lock` had been zero bytes with no live holder since 2026-07-14.
- **MCP handshake bug** — the server's `initialize` response reports `serverVersion: "0.1.0"` regardless of the actual installed version (`3.2.6`), making client-side version verification impossible.

Each of these was found and fixed by hand this session. `infigraph doctor` exists to make this a repeatable, automated check instead of manual `lsof`/`ps`/log archaeology.

## Architecture

A new module, `crates/infigraph-core/src/doctor.rs`, holds all check logic. Both the CLI subcommand and the MCP tool call the same entry point and differ only in how they format the result — there is exactly one place the actual checks are implemented.

```rust
pub enum CheckStatus { Pass, Warn, Fail }

pub struct CheckResult {
    pub category: &'static str,   // e.g. "registry", "locks", "watchers", "disk", "sidecars", "toolchain", "mcp"
    pub name: String,             // e.g. "vectormark: watch.lock stale"
    pub status: CheckStatus,
    pub message: String,
    pub remediation: Option<String>,
}

pub struct DoctorReport {
    pub checks: Vec<CheckResult>,
    pub scope: DoctorScope,
}

pub enum DoctorScope {
    Project(PathBuf),
    Global,
}

pub struct DoctorContext {
    pub registry: RegistrySnapshot,
    pub scope: DoctorScope,
    pub installed_binary: BinaryInfo,     // version, build-hash, codesign status
    pub disk_free_bytes: u64,
    // populated only when scope == Global and scan-roots are configured (see below)
    pub unregistered_candidates: Vec<PathBuf>,
}

pub fn run_doctor(ctx: DoctorContext) -> DoctorReport;
```

`run_doctor` assembles the context once (single registry read, single disk-free check, single binary-info lookup) and calls each check-category function in sequence, concatenating their `Vec<CheckResult>` into the final report. Each category is a free function with the signature `fn check_X(ctx: &DoctorContext) -> Vec<CheckResult>` — new categories are added by writing a new function and adding one line to the fixed call sequence in `run_doctor`, not by touching a shared trait/registry mechanism (YAGNI: the category list is small and fixed, a plugin system would be overbuilt).

## Scope: project-scoped by default, `--global` for the full sweep

Every other infigraph MCP tool defaults to the current project (`path` parameter), and `doctor` follows the same convention rather than surprising an agent session by sweeping every registered project on the machine.

**Default (`DoctorScope::Project(path)`)** — checks scoped to one project:
- Is this project correctly registered in the instance registry?
- Lock health for this project's `graph.lock` / `watch.lock` / `mcp.lock` (if applicable)
- Watcher liveness for this project's watcher process, if one is running
- Sidecar freshness for this project's embeddings vs. its graph

**Always included regardless of scope** — disk-space and toolchain/binary checks run every time, project-scoped or global, because they're cheap (a handful of syscalls) and because disk space specifically has already proven capable of crashing the server regardless of which project happened to trigger the write:
- Disk-space floor check (see thresholds below)
- Installed binary version/build-hash/codesign validity

**`--global` flag (CLI) / `scope: "global"` (MCP)** — adds:
- Full registry sweep: every registered project's lock/watcher/sidecar checks, not just the current one
- Unregistered-project discovery (see below)

### Unregistered-project discovery (scan-roots)

Finding projects that are on disk but were *never* registered — the exact bug that caused 5 silent-drift projects tonight — cannot be done by reading the registry (the registry is precisely what's missing the entry). A full-disk `find` for every `.infigraph` directory is too slow and permission-fraught to run automatically.

Resolution: an opt-in, user-configured list of **scan roots** — parent directories doctor is allowed to walk (bounded depth) looking for `.infigraph` dirs not present in the registry. Configured via `~/.infigraph/scan_roots.txt` (one path per line) or an `INFIGRAPH_SCAN_ROOTS` environment variable (colon-separated), matching the pattern already used elsewhere in this codebase for opt-in paths. If no scan roots are configured, this specific sub-check reports itself explicitly as **skipped** (not silently "no drift found") — the report must never claim a clean bill of health for something it didn't actually check.

This sub-check only runs in `--global` mode; a project-scoped doctor run has no reason to discover unrelated projects.

## Check battery, by category

1. **Registry** — is the current (or, in global mode, every registered) project's registry entry consistent with its on-disk `.infigraph/` state? In global mode with scan-roots configured: are there `.infigraph/` dirs under those roots missing from the registry entirely?
2. **Locks** — for each relevant lock file (`graph.lock`, `watch.lock`, `mcp.lock`): is the recorded holder PID alive? Does the recorded build-hash match the installed binary's build-hash? Is the lock file zero-byte/malformed with no live holder (stale remnant)?
3. **Watchers** — is there a live watcher process for this project matching what the lock/registry expects? Cross-reference `ps`-visible infigraph watcher processes against lock contents directly (do not rely solely on `get_watch_status`, which is known to omit daemon code watchers). Flag orphaned watchers (no corresponding project), duplicate watchers on one project, and — for lock types that carry a real heartbeat field (`mcp.lock`) — WARN if `last_heartbeat` is more than 5 minutes stale with the holder PID still alive (a live-but-wedged process). For lock types that don't carry a working heartbeat (`cli-watch`'s current PR7b limitation), the check explicitly reports "cannot prove liveness — heartbeat not implemented for this lock type" as its own WARN, rather than guessing at freshness from a field that never updates.
4. **Disk** — free space on the filesystem hosting `.infigraph/` data. Thresholds: **FAIL below 2GB free**, **WARN below 10GB free**, PASS otherwise. (Tonight's incident occurred at 9.4GB free with active indexing load — 10GB is a deliberately conservative WARN floor, not a guess at the exact failure point.) Also reports per-project graph directory sizes, purely informationally (no PASS/WARN/FAIL classification on size alone — there's no absolute size that's inherently wrong without knowing the source project's scale, only a size that's surprising relative to history, which is out of scope for this iteration). This still makes an unusually large graph (the 43GB `sittir` bloat incident) visible in a global sweep's output even though it isn't classified.
5. **Sidecars** — for the project(s) in scope, compare `embeddings.bin` (and `docs_embeddings.bin` where present) mtime against the graph DB's mtime; WARN if the sidecar is more than 1 hour older than the graph (a reindex without a sidecar refresh is the failure mode this catches, not routine staleness — 1 hour is comfortably past any normal reindex-to-embed gap).
6. **Toolchain/binary** — installed `infigraph`/`infigraph-mcp` binary version and build-hash (via `infigraph --version`), codesign validity (`codesign -dv`), and cross-check against what any currently-held lock's `build_hash` field expects (surfacing the "binary was reinstalled but old processes are still running the previous build" scenario found tonight, even when it's benign).
7. **MCP handshake** — verifies the MCP server's `initialize` response reports the correct `serverVersion` (fixing the `"0.1.0"` hardcoded-placeholder bug is a prerequisite for this check to mean anything, and is being tracked as a small standalone fix, not part of doctor's own scope).

## CLI surface

```
infigraph doctor [PATH] [--global]
```

- `PATH` defaults to the current working directory's project (same convention as other infigraph CLI commands).
- `--global` switches to the full registry sweep described above.
- Output: human-readable, grouped by category, each line `[PASS|WARN|FAIL] <name>: <message>` with a `  → <remediation>` line underneath when present. A summary line at the end (`3 WARN, 1 FAIL, 12 PASS`).
- **Exit code**: `0` if every check is PASS, `1` if the worst status is WARN, `2` if any check is FAIL — so `infigraph doctor` is usable directly in scripts, cron jobs, or a pre-flight check without needing to parse output.

## MCP surface

```
mcp__infigraph__doctor(path?: string, scope?: "project" | "global")
```

- Omitting both defaults to project scope on the caller's current working directory, matching the CLI.
- Output is the same categorized report, formatted as structured text consistent with other infigraph MCP tool output conventions.
- **Compression exemption**: doctor's output should be excluded from the context-compression pipeline the same way security tools (`detect_security_issues`, `detect_taint_flows`, etc.) already are. A diagnostic report's value is in its completeness — a compressed doctor report that silently drops a FAIL line defeats the entire point of the command.

## Error handling

Each check-category function is called in isolation such that an internal failure (permission denied walking a directory, a corrupt/unparseable lock file, an unreadable registry entry) becomes a **FAIL result for that specific check**, carrying the underlying error as its message — it must never panic or bubble an error that aborts the whole `run_doctor` call. A partially-failed run still returns every other category's results. This follows the same "no silent caps" principle used elsewhere in this project: if a check couldn't actually run, the report says so explicitly rather than omitting the line and looking clean.

## Testing

- **Unit tests per check-category function**, driven by a synthetic `DoctorContext` built directly in the test (no real filesystem/registry needed for most cases): a registry entry with no matching `.infigraph/` dir, a lock file with a mismatched `build_hash`, a zero-byte lock file with no live PID, a sidecar mtime older than its graph by a controlled delta, disk-free values straddling both thresholds. Each asserts the exact `CheckStatus` and that the message/remediation text is non-empty and specific — no case should produce a generic "something's wrong" message. Mirrors the fixture style already used in `crates/infigraph-core/tests/write_lock_perf.rs`.
- **One real-temp-directory integration test**: register a project against a temp registry, corrupt its lock's `build_hash` field on disk, run `run_doctor` against the real context, assert the report contains a FAIL for that specific lock check with the expected remediation text.

## Non-goals (this iteration)

- Automated remediation ("doctor --fix"). This design only reports; applying fixes (re-registering projects, clearing stale locks, freeing disk space) stays a human/agent decision informed by the report, not something doctor does on its own. A future iteration could add this once the check battery has proven itself.
- Real heartbeats for `cli-watch` locks. The design accounts for this gap (the watcher check explicitly reports "cannot prove liveness" rather than guessing) but does not fix the underlying PR7b limitation — that's tracked as separate follow-up work, not part of this spec.
- Fixing the `serverVersion: "0.1.0"` handshake bug itself — noted as a prerequisite for check #7 to be meaningful, tracked separately.
