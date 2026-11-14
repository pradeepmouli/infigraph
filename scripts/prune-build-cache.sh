#!/usr/bin/env bash
# Prune the orphan object files (and, when it is safe, the incremental cache)
# out of a cargo target directory. Dry run unless --apply.
#
#   scripts/prune-build-cache.sh [--apply] [--include-incremental] [--target-dir DIR]
#
# This is a script and not part of `infigraph`: a shared target directory is a
# property of how a machine is set up (here, a `cargo` shim that points every
# worktree nested in the checkout at <checkout>/.shared-target), not of the
# product, and the product never deletes build artifacts it did not create.
#
# What it removes, per profile directory (<target>/debug, <target>/release, ...):
#   deps/*.o          Object files left behind by killed or aborted builds. They
#                     are never read by a later build; on 2026-09-30 and again on
#                     2026-10-08 they were 76-78 GiB of a ~90 GiB target
#                     directory, and deleting them forced no rebuild. They regrow
#                     within hours, so this is meant to be cheap to run by hand.
#   incremental/      The incremental-compilation cache -- removed only when
#                     CARGO_INCREMENTAL=0 is set in this shell, or with
#                     --include-incremental. Deleting it from a setup that uses
#                     incremental builds only costs those workspace crates a full
#                     recompile; it is a cache, not state.
#
# It refuses to run while a cargo or rustc builds into the target directory being
# pruned: a rustc whose arguments name that directory, a cargo whose working
# directory is inside the checkout that owns it (rust-analyzer's background
# `cargo check` counts), or any such process whose working directory cannot be
# read. A build in another checkout, with its own target, does not block it:
# deleting under a running build races it, but only under the same directory.
#
# Why incremental/ comes back although CARGO_INCREMENTAL = "0" is configured:
# the `[env]` table of ~/.cargo/config.toml only sets variables for the
# processes cargo *launches* (rustc, build scripts). Cargo reads its own
# incremental setting from its process environment or from `[build]
# incremental`, so `[env] CARGO_INCREMENTAL` does nothing for it. Measured on
# cargo 1.99.0 with a hello-world crate: with only the `[env]` entry the
# incremental/ directory held a 128 KB cache; with `--config
# build.incremental=false`, or CARGO_INCREMENTAL=0 exported in the shell, it was
# created empty. The producer is therefore cargo itself on every build, not
# rust-analyzer (the neighbouring .shared-target/flycheck0 is rust-analyzer's
# check output directory and holds a few KB). The lasting fix is `[build]
# incremental = false` in ~/.cargo/config.toml (sccache cannot cache
# incremental units either way); until then pass --include-incremental.
set -euo pipefail

apply=0
include_incremental=0
target_dir="${CARGO_TARGET_DIR:-}"

usage() {
  sed -n '2,5p' "$0" | sed 's/^# \{0,1\}//'
}

while [ $# -gt 0 ]; do
  case "$1" in
    --apply) apply=1 ;;
    --include-incremental) include_incremental=1 ;;
    --target-dir)
      shift
      target_dir="${1:?--target-dir needs a directory}"
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
  shift
done

if [ -z "$target_dir" ]; then
  root="$(cd "$(dirname "$0")/.." && pwd -P)"
  if [ -d "$root/.shared-target" ]; then
    target_dir="$root/.shared-target"
  else
    target_dir="$root/target"
  fi
fi
if [ ! -d "$target_dir" ]; then
  echo "no target directory at $target_dir" >&2
  exit 1
fi

target_dir="$(cd "$target_dir" && pwd -P)"
# The checkout that owns the target directory: `.shared-target` and `target`
# both sit directly under it.
owner="$(dirname "$target_dir")"

# Every running cargo or rustc, one line each: "name<TAB>cwd<TAB>args", with cwd
# "?" when it cannot be read. Prints nothing when none runs.
live_builds() {
  local pid comm cwd args
  ps -axo pid=,comm= | while read -r pid comm; do
    case "${comm##*/}" in
      cargo | rustc) ;;
      *) continue ;;
    esac
    cwd="$(lsof -a -p "$pid" -d cwd -Fn 2>/dev/null | awk '/^n/ {print substr($0, 2); exit}')"
    args="$(ps -ww -o args= -p "$pid" 2>/dev/null || true)"
    printf '%s\t%s\t%s\n' "${comm##*/}" "${cwd:-?}" "$args"
  done
}

# The list is overridable (PRUNE_BUILD_CACHE_PROCESSES, in the same format) so a
# test can say what is running. A bare name with no cwd is read as unreadable,
# so an old-style list still refuses.
if [ -n "${PRUNE_BUILD_CACHE_PROCESSES+set}" ]; then
  listing="$PRUNE_BUILD_CACHE_PROCESSES"
else
  listing="$(live_builds)"
fi

# Refuse only what builds into this target directory: a cargo or rustc whose
# arguments name it, a cargo whose working directory is inside the owning
# checkout, or any of them whose working directory cannot be read (fail closed).
busy=""
while IFS=$'\t' read -r name cwd args; do
  name="${name##*/}"
  case "$name" in
    cargo | rustc) ;;
    *) continue ;;
  esac
  if [ -z "${cwd:-}" ] || [ "$cwd" = "?" ]; then
    busy="$busy $name(cwd unreadable)"
    continue
  fi
  case "${args:-}" in
    *"$target_dir"*)
      busy="$busy $name(builds into the target)"
      continue
      ;;
  esac
  if [ "$name" = cargo ]; then
    case "$cwd/" in
      "$owner"/*)
        busy="$busy $name(in $owner)"
        continue
        ;;
    esac
  fi
done <<<"$listing"
if [ -n "$busy" ]; then
  echo "refusing to prune $target_dir: a running build uses it:${busy}. Stop it (or wait) and run again." >&2
  exit 1
fi

prune_incremental=0
if [ "$include_incremental" = 1 ] || [ "${CARGO_INCREMENTAL:-}" = "0" ]; then
  prune_incremental=1
fi

python3 - "$target_dir" "$apply" "$prune_incremental" <<'PY'
import os
import shutil
import sys

target, apply, prune_incremental = sys.argv[1], sys.argv[2] == "1", sys.argv[3] == "1"
GIB = 1024 ** 3


def tree_bytes(path):
    total = 0
    for root, _dirs, files in os.walk(path):
        for name in files:
            try:
                total += os.lstat(os.path.join(root, name)).st_size
            except OSError:
                pass
    return total


orphans = 0
orphan_bytes = 0
incremental_bytes = 0
incremental_skipped = 0
for profile in sorted(os.listdir(target)):
    profile_dir = os.path.join(target, profile)
    if not os.path.isdir(profile_dir) or os.path.islink(profile_dir):
        continue
    deps = os.path.join(profile_dir, "deps")
    if os.path.isdir(deps):
        with os.scandir(deps) as entries:
            for entry in entries:
                if entry.name.endswith(".o") and entry.is_file(follow_symlinks=False):
                    size = entry.stat(follow_symlinks=False).st_size
                    orphans += 1
                    orphan_bytes += size
                    if apply:
                        os.unlink(entry.path)
    incremental = os.path.join(profile_dir, "incremental")
    if os.path.isdir(incremental) and not os.path.islink(incremental):
        size = tree_bytes(incremental)
        if prune_incremental:
            incremental_bytes += size
            if apply:
                shutil.rmtree(incremental)
        else:
            incremental_skipped += size

verb = "removed" if apply else "would remove"
print(f"{verb} {orphans} orphan .o files ({orphan_bytes / GIB:.2f} GiB)")
if prune_incremental:
    print(f"{verb} incremental/ ({incremental_bytes / GIB:.2f} GiB)")
elif incremental_skipped:
    print(
        f"left incremental/ ({incremental_skipped / GIB:.2f} GiB): CARGO_INCREMENTAL is not 0 "
        "in this shell; pass --include-incremental to remove it"
    )
if not apply:
    print("dry run -- nothing was deleted; re-run with --apply")
PY
