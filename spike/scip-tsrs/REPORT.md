# Native TypeScript SCIP indexer spike

`spike/scip-tsrs` in `scratchpad/wt-docwatch`, based on `0870f460`. The Rust indexer calls the tsrs checker in its own process and writes SCIP through the same `scip` 0.10.0 bindings used by Infigraph.

The published package and subsequent Infigraph work are covered in the [integration follow-up](INTEGRATION.md). The measurements below describe the original isolated spike.

## Build and project load

| Item | Measured result |
| --- | --- |
| tsrs dependency | `v0.24.0`, Git revision `86de03ae428a956686af29177b50f0fdf4aee059` |
| Host | Apple silicon, macOS 27.2, 14 cores, 48 GiB RAM, rustc 1.99.1 |
| Dependency fetch | 11.42 s wall time, including a wait for the Cargo package-cache lock |
| First clean optimized build | 29.96 s wall time and 313.57 MiB allocated in `target` |
| Final working `target` | 461.48 MiB after the typed SCIP range and file-URI rebuilds. Includes artifacts from both dependency graphs. |
| Isolated project load | 4 fixture roots and 207 total files with bundled libs. The `routes.ts` walk printed 279 identifier rows with checker symbols, declaration locations, and parent chains. |

The build used a standalone Cargo workspace, `--release -j 4`, no LTO, 32 codegen units, debug info and incremental compilation off, and an empty `RUSTC_WRAPPER`. The first build includes the config and identifier probe. Cargo fetched the pinned checkout before the timed build. It compiled without a tsrs patch. The main workspace lockfile and build were outside this run.

The first disk sample was 40.4 GiB free. After concurrent host activity brought it below 17 GiB, I compressed this spike’s traces. The build stayed well below 8 GiB. Sccache and Sittir’s temporary files were left in place.

## APIs used

| Work | Calls |
| --- | --- |
| Read config, files, and libraries | `get_parsed_command_line_of_config_file`, `new_cached_fs_compiler_host`, `ProgramOptions::new`, `new_program`, `Program::source_files`, `Program::get_source_file` |
| Walk and locate | `Node::for_each_child_static`, `Node::parent`, `Node::name`, `get_source_file_of_node`, `get_token_pos_of_node`, `get_ecma_line_and_utf16_character_of_position` |
| Resolve symbols and declarations | `Program::get_type_checker`, `get_symbol_at_location_exported`, `get_symbol_of_declaration_exported` for the constructor path, `get_aliased_symbol`, `Symbol::declarations` |
| Resolve contextual properties and secondary references | `get_contextual_type_exported`, `get_property_of_type_exported`, `get_type_at_location`, `get_shorthand_assignment_value_symbol` |
| Check project health separately | `get_program_diagnostics`, `get_syntactic_diagnostics`, `get_semantic_diagnostics` |

I read the installed `FileIndexer.js`, `ProjectIndexer.js`, and their naming helpers. The spike keeps the `scip-typescript npm` symbol scheme, package-relative descriptors, declaration traversal order, per-file `local N` counter, and object-literal `key<N>:` convention. Definition roles use the SCIP `Definition` bit. References use zero as in the oracle. The Rust writer uses typed SCIP ranges, explicit UTF-16 document columns, and a file URI for the project root. Infigraph’s `occurrence_range` at the base commit already accepts typed ranges and prefers them over the legacy array. Both golden inputs and outputs decoded with the generated Rust bindings. The native output passed symbol syntax and range checks in `dump`. Full graph ingestion was outside this spike. [SCIP protocol](https://github.com/scip-code/scip/blob/main/scip.proto)

## Golden diff

The oracle is the installed `scip-typescript` **0.4.0**, which loads TypeScript **5.9.3**. I copied the four `order-service` sources byte for byte into scratch and supplied a common strict NodeNext/ES2022 tsconfig and `@types/express@4.17.25`. The resolved dependency set included `@types/node@26.6.6`. I used the existing `@sittir/typescript` tsconfig at Sittir `0fda0b5fd0bbaacc71afa06d9cd812e2a6628bc0`, with its existing dependencies. Both programs loaded 134 Sittir roots for indexing. Both emitted 132 documents because `src/wrap.ts` and `src/portable/map.ts` exceed the oracle’s default 1 MB limit. All outputs went to scratch.

The first Sittir oracle run started from scratch. Its `--cwd` option did not set the TypeScript host’s process directory, so it missed ambient Node declarations. I corrected the launch to start from the package directory and excluded the first run from the table. The traced oracle reproduced the corrected SCIP bytes.

Each count is a set of exact `(symbol, relative file, range, role)` tuples. The comparison decodes both range encodings to zero-based half-open coordinates. It compares literal symbol strings and includes locals. Recall divides exact matches by oracle tuples. Precision divides them by native tuples.

| Corpus and native library input | Documents | Oracle | Native | Exact | Oracle only | Native only | Recall | Precision | Jaccard |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Order service, shared 5.9.3 libs | 4 | 682 | 682 | 682 | 0 | 0 | 100% | 100% | 100% |
| Order service, bundled 7.1 libs | 4 | 682 | 682 | 519 | 163 | 163 | 76.100% | 76.100% | 61.420% |
| Sittir TypeScript, shared 5.9.3 libs | 132 | 321,590 | 321,441 | 320,971 | 619 | 470 | 99.808% | 99.854% | 99.662% |
| Sittir TypeScript, bundled 7.1 libs | 132 | 321,590 | 321,618 | 309,157 | 12,433 | 12,461 | 96.134% | 96.126% | 92.548% |
| Symbol and Unicode control, shared 5.9.3 libs | 1 | 49 | 49 | 49 | 0 | 0 | 100% | 100% | 100% |

With shared libraries, Sittir’s global-symbol recall was 99.944% (272,956 of 273,110) and its definition recall was 99.938% (69,249 of 69,292). The synthetic control covered a class, constructor, interface, methods, properties, function, variables, enum members, shorthand, repeated object-literal keys, contextual object members, and a symbol after `é😀` on the same line. It matched both `key0:` and `key1:` and the UTF-16 range after the emoji.

### Difference attribution

Counts are oracle-only / native-only. `classify.py` assigned every unmatched tuple a cause. Per-tuple ledgers and source-resolution traces are in `.runs`.

| Cause | Fixture, bundled | Sittir, shared | Sittir, bundled |
| --- | ---: | ---: | ---: |
| TypeScript package version in otherwise identical standard-library symbols | 163 / 163 | 0 / 0 | 11,814 / 11,814 |
| TS7 library mapping skips legacy ESNext collection and float16 files with the 5.9.3 directory | 0 / 0 | 152 / 0 | 0 / 0 |
| Standard-library declarations moved from ESNext to ES2025 | 0 / 0 | 0 / 0 | 152 / 152 |
| Additional merged declarations in the bundled ES2026 collection, math, and JSON libraries | 0 / 0 | 0 / 0 | 0 / 25 |
| Identical union declaration sets returned in a different order, changing local ordinals | 0 / 0 | 357 / 357 | 357 / 357 |
| Earlier index-signature reference shifted subsequent local ordinals | 0 / 0 | 108 / 108 | 108 / 108 |
| TS7 resolves three `process.env` accesses to the Node index signature | 0 / 0 | 0 / 3 | 0 / 3 |
| Shared ordinal counter and union order changed a `typeLiteral<N>:` descriptor | 0 / 0 | 2 / 2 | 2 / 2 |
| **Total** | **163 / 163** | **619 / 470** | **12,433 / 12,461** |

- The fixture’s entire bundled-mode deficit is the `typescript 5.9.3` versus `typescript 7.1.0-beta` package identity in the symbol string. The shared-library run confirms that it has no unexplained loss.
- The 152 legacy library tuples comprise 132 `Set#`, 14 `ReadonlySet#`, and six `Math#` references. TS7 maps `esnext.collection` and `esnext.float16` to `lib.es2026.collection.d.ts` and `lib.es2025.float16.d.ts` respectively. Those names are absent from the legacy directory. The bundled TS7 files contain the promoted declarations and additional merges.
- The 357 union-order pairs occur in `src/portable/index.ts`, `tests/anchored-holes.test.ts`, and `src/render-engine.ts`. The selected declaration sets agree. For example, the nine `op` declarations are visited in a different order, which changes when local descriptors and their owners get numbered. In `render-engine.ts` the first union lookup emits the same set of IDs, but the later narrowed lookup exposes the different assignment.
- In `src/backend.ts` and `tests/backend-boundary.test.ts`, the additional `process.env` lookup reserves a local ordinal before later declarations. At the two `NativeBackendStatusLike.name` reference sites the combination of that shift and union order changes `typeLiteral13:` to `typeLiteral15:`.

## Sittir wall time and peak memory

Three warm, alternating process runs from the same project root, with tracing and diagnostics off. `/usr/bin/time -l` supplied wall time and maximum resident set size. These were ordinary runs on a shared machine. Each run reproduced its golden SCIP bytes. The native run used the oracle’s 5.9.3 declaration directory.

| Indexer | Wall seconds by run | Median wall | Peak RSS MiB by run | Median peak RSS |
| --- | --- | ---: | --- | ---: |
| Stock scip-typescript 0.4.0 | 26.97, 25.95, 30.26 | **26.97 s** | 2792.44, 2694.17, 2732.92 | **2732.92 MiB** |
| In-process tsrs spike | 1.38, 1.31, 1.49 | **1.38 s** | 320.00, 319.47, 318.00 | **319.47 MiB** |

That is 19.5× lower median wall time and 8.6× lower median peak RSS for the minimal spike versus stock. The stock run also computes hover text and relationships. The measurement includes that difference in work and cannot isolate the Rust or IPC effect. Native time includes program loading, checker queries, and protobuf serialization.

## Conformance, blockers, and fallback

- Upstream reports all **13,700** error baselines and **12,998** each of the type and symbol baselines matching its pinned TypeScript 7.1.0-beta reference under the upstream emit sequence. It reports four checking-only diagnostic artifacts. These numbers compare to the Go compiler and do not measure SCIP compatibility with the 5.9.3 oracle. [Pinned tsrs compatibility report](https://github.com/oxc-project/tsrs/blob/86de03ae428a956686af29177b50f0fdf4aee059/docs/COMPATIBILITY.md#conformance)
- No tsrs panics or project, syntax, or semantic diagnostics were observed in the fixture’s bundled diagnostic run or the two Sittir diagnostic runs. A direct drop-in still needs to handle the library identity, declaration ordering, and index-signature differences above. The program and checker were used once per process. Repeated project reclamation, JSX, project references, and matching hover/relationship output were outside the measured scope.
- The fallback probe used `typescript@7.1.0-beta`, git head `3e9236361fa97a9e6c685f4833ac457ae2d8c50a`. On stdio, plain `tsc --api` uses a self-delimiting MessagePack stream of `[type, method bytes, payload bytes]`. `initialize` sent `93 01 c4 0a … c4 04 …` and got `93 04 c4 0a … c4 97 …`. The bin lengths are byte counts, with big-endian 16- and 32-bit lengths for larger fields. `tsc --api --async` answered a JSON-RPC request framed by `Content-Length` and `\r\n\r\n`. [Published TypeScript 7.1.0-beta package](https://registry.npmjs.org/typescript/7.1.0-beta)
- The SDK created a program, received a binary `getSourceFile` payload, and resolved a definition and reference in one `getSymbolsAtLocations` request. The AST reported `pos=27`, `start=28`, `end=34` for `target` after `é😀`. Its UTF-8 byte start was 31. The file’s wire `end` was 53, matching 53 UTF-16 units versus 56 bytes. The SDK reads the position fields directly from the binary payload. An initial probe passed a string `target` where the programmatic options require a numeric `ScriptTarget`. The beta returned a recovered Go panic in the RPC response. Omitting that option let the probe complete. No IPC throughput measurement was made.

## Reproduction

Run from `spike/scip-tsrs` with the existing Sittir checkout path. The setup needs the stock indexer 0.4.0 and its TypeScript 5.9.3 dependencies.

```sh
spike_root="$PWD"
spike_pkg=/Users/pmouli/GitHub.nosync/refactory-lang/sittir/packages/typescript
spike_lib="$HOME/.local/lib/node_modules/@sourcegraph/scip-typescript/node_modules/typescript/lib"
mkdir -p .runs
CARGO_TARGET_DIR="$PWD/target" CARGO_INCREMENTAL=0 RUSTC_WRAPPER= cargo build --release --locked -j 4
(cd "$spike_pkg" && scip-typescript index --cwd "$spike_pkg" --no-progress-bar \
  --output "$spike_root/.runs/sittir.oracle-cwd.scip" "$spike_pkg")
target/release/scip-tsrs-spike index "$spike_pkg/tsconfig.json" \
  .runs/sittir.shared-lib.typed.scip --lib "$spike_lib" --trace .runs/sittir.shared-lib.jsonl
node compare.cjs .runs/sittir.oracle-cwd.scip .runs/sittir.shared-lib.typed.scip \
  .runs/sittir.shared-lib.final.diff.jsonl
```

Omit `--lib` for the bundled test. To trace the oracle, run it with `NODE_OPTIONS=--require=$spike_root/trace-oracle.cjs` and `SCIP_ORACLE_TRACE` set to an absolute scratch output path. The existing local evidence includes the fixture config, three timing pairs, annotated difference ledgers, and both transports’ fallback samples. `tsc-api-probe.mjs` accepts the path to the installed `typescript@7.1.0-beta` package and a scratch output directory.

## Confidence and recommendation

**High** confidence in feasibility and the measured results for these inputs. **Medium** confidence in the integration direction. Pursue **in-process tsrs within a standalone Rust indexer**. Keep scip-typescript 0.4.0 as the current production oracle until the remaining identity differences are resolved and parity is checked on more projects. IPC to `tsc` is viable as a contingency, but it adds transport and codec work while still requiring TS7-to-TS5 symbol compatibility.
