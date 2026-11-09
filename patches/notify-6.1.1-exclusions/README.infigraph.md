# notify 6.1.1 with FSEvents exclusion paths (infigraph patch)

This is the unmodified `notify` 6.1.1 crate from crates.io (license: `LICENSE-CC0`, kept as
shipped) plus one macOS-only addition, wired in through `[patch.crates-io]` in the workspace
`Cargo.toml`. Remove the patch and the `[patch.crates-io]` entry once upstream exposes
`FSEventStreamSetExclusionPaths`.

Patched, and nothing else (`src/fsevent.rs`):

1. `FsEventWatcher` gains a private `exclusion_paths: Vec<PathBuf>` field (initialised empty,
   shown in `Debug`).
2. `FsEventWatcher::MAX_EXCLUSION_PATHS` (8, the limit FSEvents enforces) and
   `FsEventWatcher::set_exclusion_paths(Vec<PathBuf>) -> Result<()>`, which rejects more than 8.
3. `FsEventWatcher::apply_exclusion_paths`, called in `run()` between `FSEventStreamCreate` and
   the stream being scheduled and started (the only point `FSEventStreamSetExclusionPaths`
   allows). It uses the raw binding `fsevent-sys` 4.1.0 already declares.

With no exclusion paths set the behaviour is identical to upstream. `Cargo.toml` and
`Cargo.toml.orig` are the upstream files, untouched.
