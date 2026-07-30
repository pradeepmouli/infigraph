# Contributing to this fork (pradeepmouli/infigraph)

This supplements [`CONTRIBUTING.md`](CONTRIBUTING.md) (build/test/style/adding-language — still the source of truth for those) with policy specific to this fork: branch topology, where hardening work lives, and how a fix makes its way upstream.

## Branch topology

- **`upstream/main`** — `intuit/infigraph`'s main branch. Never pushed to directly from this fork; only ever a PR target.
- **`origin/main`** — a **pure passive mirror** of `upstream/main`. It is never a merge target for work that upstream hasn't accepted yet. It only ever moves via `git fetch upstream && git push origin upstream/main:main --force`. If you find yourself about to merge a PR into `origin/main`, stop — that's the wrong branch (see [Why not `origin/main`](#why-not-originmain) below).
- **`feat/hardening`** — the persistent branch for hardening work (see `docs/DESIGN-hardening.md`) and the branch local builds/installs are cut from. Fork-specific infrastructure that doesn't necessarily belong upstream (or isn't ready to) lives here permanently. Other `feat/*` branches continue as separate parallel work streams and merge into this one when ready.

## Where work goes

**Default: `feat/hardening`.** Branch off it, do the work, merge back into it. This is correct for anything tied to fork-specific hardening infrastructure (the instance registry, lock identity/takeover, quarantine, disk preflight, and everything else in `docs/DESIGN-hardening.md`) or anything not yet mature enough to send upstream.

**If the fix is cleanly cherry-pickable to upstream** (a bug fix or improvement that doesn't depend on fork-specific hardening machinery and upstream would plausibly accept as-is): do the work on its own branch off `feat/hardening` as usual, merge it there, and *separately* cherry-pick the specific commit(s) onto a **fresh branch cut from `upstream/main`** (not from `feat/hardening`, not from `origin/main`) to open the upstream PR. This is the same workflow already used successfully for every upstream PR this fork has opened:

```bash
git fetch upstream
git checkout -b fix/some-upstream-fix upstream/main
git cherry-pick <commit-sha>   # resolve conflicts against upstream's actual code, not the fork's
# ...independently verify the result (build, test, manually confirm the fix) before pushing...
git push origin fix/some-upstream-fix
gh pr create --repo intuit/infigraph --base main
```

Independently verify against the fresh cherry-pick — don't trust that "it worked on the fork" is sufficient, since the fork's tree has diverged and a clean cherry-pick can still behave differently against upstream's actual code.

### Why not `origin/main`?

It's tempting to also merge the cherry-pickable fix into `origin/main` first — as a staging step, or so the fork's own main branch has the fix immediately rather than waiting on upstream's review cycle. Don't: the moment `origin/main` has a commit `upstream/main` doesn't, it's diverged again, which is exactly the state this fork spent real effort eliminating (11 stray commits were found, reconciled, and `origin/main` was reset to match `upstream/main` exactly). Keeping `origin/main` a strict mirror means that reset never has to happen again. The "resolve conflicts before opening the real PR" need is already satisfied by cherry-picking onto a **fresh `upstream/main`-based branch** — that step *is* the merge-conflict triage, no intermediate `origin/main` merge required.

If `origin/main` ever needs the fix before upstream merges it, that's what `feat/hardening` (or a `feat/*` branch based on it) is for — check it out locally rather than expecting `origin/main` to carry unreleased work.

## Hardening work tracking

`docs/DESIGN-hardening.md` is the master spec — read it before starting hardening work. Its "Implementation Status" section uses inline markdown checkboxes (not a separate tracker) as the single source of truth for what's shipped/in-progress/not-started, each unchecked item linking to a GitHub issue (label `hardening`) for anything substantial enough to warrant one. Issues reference any PR that already landed partial or adjacent groundwork — check an item's issue before starting, since it may already be scoped or half-done.

## Design → plan → implementation

Non-trivial work (new commands, new subsystems, anything touching the write-safety/lock/registry machinery) goes through `docs/superpowers/specs/YYYY-MM-DD-<topic>-design.md` (the design, reviewed and approved before code) and `docs/superpowers/plans/YYYY-MM-DD-<topic>.md` (the concrete, bite-sized implementation plan derived from it) before implementation starts. See existing specs/plans under those directories for the expected shape and level of detail.
