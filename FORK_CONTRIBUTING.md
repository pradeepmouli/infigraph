# Contributing to this fork (pradeepmouli/infigraph)

This supplements [`CONTRIBUTING.md`](CONTRIBUTING.md) (build/test/style/adding-language — still the source of truth for those) with policy specific to this fork: branch topology, where hardening work lives, and how a fix makes its way upstream.

## Branch topology

- **`upstream/main`** — `intuit/infigraph`'s main branch. Never pushed to directly from this fork; only ever a PR target.
- **`origin/main`** — `upstream/main` plus whatever this fork's currently-**open** upstream PR branches are (fast-forward-merged on top, one at a time, once each has a real PR against `intuit/infigraph`). It moves two ways: (1) `git fetch upstream && git push origin upstream/main:main --force` to absorb upstream's latest — this also naturally drops any PR upstream just merged, since that PR's commits now arrive as part of `upstream/main` itself (often under different SHAs, e.g. squash-merged); (2) fast-forward `main` onto an open-PR branch (never a merge commit — the PR branch must already be rebased cleanly onto the current `main`/`upstream/main`) once that branch has an actual PR open. **Never** put work here before its PR exists — see [Why not `origin/main`](#why-not-originmain) below for the sequencing that still applies.
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

### Why not `origin/main` *before* the PR exists?

It's tempting to merge the cherry-pickable fix into `origin/main` as a staging step, before even opening the upstream PR. Don't: the moment `origin/main` has a commit that isn't either from `upstream/main` or from a branch with a real, open PR against it, it's diverged in a way nothing tracks or cleans up — exactly the state this fork spent real effort eliminating once already (11 stray commits were found, reconciled, and `origin/main` was reset to match `upstream/main` exactly). The "resolve conflicts before opening the real PR" need is already satisfied by cherry-picking onto a **fresh `upstream/main`-based branch** — that step *is* the merge-conflict triage, no pre-PR `origin/main` merge required.

The correct sequence is: (1) do the work on `feat/hardening` as usual; (2) cherry-pick onto a fresh `upstream/main`-based branch, verify independently, push, `gh pr create --repo intuit/infigraph`; (3) **only once that PR is open**, fast-forward `origin/main` onto the PR branch (`git push origin <branch>:main` or `git branch -f main <branch> && git push origin main`) so the fork's own `main` carries it while waiting on review. When upstream merges the PR, the next `git fetch upstream && git push origin upstream/main:main --force` absorbs it and `origin/main` is a pure mirror again until the next open PR.

For anything that *isn't* PR-bound — fork-specific hardening infrastructure with no plan to go upstream — `origin/main` is still never the place for it. That's what `feat/hardening` (or a `feat/*` branch based on it) is for — check it out locally rather than expecting `origin/main` to carry unreleased, non-upstreamable work.

## Hardening work tracking

`docs/DESIGN-hardening.md` is the master spec — read it before starting hardening work. Its "Implementation Status" section uses inline markdown checkboxes (not a separate tracker) as the single source of truth for what's shipped/in-progress/not-started, each unchecked item linking to a GitHub issue (label `hardening`) for anything substantial enough to warrant one. Issues reference any PR that already landed partial or adjacent groundwork — check an item's issue before starting, since it may already be scoped or half-done.

## Design → plan → implementation

Non-trivial work (new commands, new subsystems, anything touching the write-safety/lock/registry machinery) goes through `docs/superpowers/specs/YYYY-MM-DD-<topic>-design.md` (the design, reviewed and approved before code) and `docs/superpowers/plans/YYYY-MM-DD-<topic>.md` (the concrete, bite-sized implementation plan derived from it) before implementation starts. See existing specs/plans under those directories for the expected shape and level of detail.
