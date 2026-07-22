# Changelog

All notable changes follow Keep a Changelog and Semantic Versioning. Before
1.0, minor versions may contain breaking manifest or CLI changes.

## [Unreleased]

## [0.3.0] - 2026-07-22

### Added

- Tagged releases publish checksummed `frost` + `frostd` archives for static
  x86_64 Linux, the current macOS runner architecture and x86_64 Windows.
- Language-neutral `command` targets run a named `[toolchain.tools]` executable
  with direct argv, declared configuration-isolated file outputs, optional
  Makefile depfiles, static environment and explicit `pass_env`. Named tools
  can be overridden per platform and are fingerprinted. Real-tool E2E coverage
  exercises Rust, Go, Java, Python and TypeScript/Node when installed.
- `kind = "test"` accepts either the existing shell `cmd` or a named `tool`
  with direct `args`, `env` and `pass_env`. Direct language tests share
  Frost's success-only stamp, cache, failure cleanup, `--all` and `--affected`
  behavior; a real Python E2E verifies success, cache and failed-stamp cleanup.
- Command targets support ordered direct-argv `steps` and
  configuration-isolated `clean_dirs`. Compile/package pipelines such as
  `javac` → `jar` can publish one stable artifact without shell parsing; stale
  intermediate files are removed before normal and determinism executions.
  `${clean_dir}` / `${clean_dirs}` reuse the owned path in argv without
  duplicating configuration or package prefixes.
- Command targets can opt into `preserve_outputs` for incremental compilers
  that update only an affected subset. The mode is action-keyed, every retained
  output is still verified, and compiler state can be declared alongside final
  artifacts for safe failure cleanup. Native TypeScript 7 E2E coverage guards
  the output-preservation path.
- `frost pack-jar` creates sorted, fixed-timestamp, compressed Java archives
  with a standards-compliant manifest and optional `--main-class`. It avoids a
  second JVM in `javac` → JAR actions while remaining a normal fingerprinted
  direct-argv step.
- `frost pack-wheel` creates a deterministic pure-Python wheel with a
  normalized standard filename, required metadata and complete SHA-256/size
  `RECORD`. Paths and symlinks are validated, bytecode/cache files are omitted,
  and the archive is atomically published. Real Python E2E imports it.
- `build` and `test` accept `--all-platforms`, preserving parallel action
  execution inside each platform and ending with a compact host/device status
  tree.
- Bash, Zsh, Fish, PowerShell and Elvish dynamic completion resolves targets,
  profiles and platforms from the selected workspace. Static scripts are
  available for those shells and Nushell through `frost completions`.
- `frost pick` offers optional multi-target/test selection through `fzf`; it
  also has a script-friendly `--print` mode.
- `frost watch` debounces recursive native filesystem events, excludes Frost/
  Git/declared-output self-writes, rebuilds affected graphs and optionally
  restarts a direct-argv development process only after success. Broken builds
  keep the last successful process alive.
- `frost dev` adds the target-aware hot-reload loop: it infers the built
  native/JAR/JavaScript/Python artifact and runtime, restarts only after
  success, and accepts an explicit runner for emulated/custom outputs.
- `frost run` resolves one target to its artifact and executes native,
  Java/JAR, JavaScript or Python direct argv. Foreign-platform execution
  requires an explicit runner; `--print` exposes exact argv.
- `frost debug` validates native symbol flags and launches GDB/LLDB, or selects
  jdb from an executable JAR's manifest, Node inspect, or Python pdb for
  language artifacts. All paths remain direct argv and support `--print`.
  Native `frost init` scaffolds `-O0 -g` debug and `-O3 -DNDEBUG` release
  profiles.
- `frost ide` builds one target and generates artifact-aware VS Code
  `tasks.json`/`launch.json` for native, Java, JavaScript or Python debugging.
  It exposes a JSON dry run and never overwrites either existing file.
- `frost doctor` checks the configured graph and every required executable,
  then separately reports optional runtimes, debuggers, `fzf`, bubblewrap and
  Graphviz. Human output is a compact tree; `--json` preserves required vs
  optional status for setup automation.
- `frost import-bazel` consumes Bazel's own query XML and writes a conservative
  multi-package `cc_library`/`cc_binary`/`cc_test` migration scaffold. It has a
  full dry-run, refuses overwrite and stops on configurable or unsupported
  semantics instead of silently flattening them.
- `frost bazel-dev` keeps Bazel's configured graph/server/cache authoritative,
  watches workspace changes, and restarts the complete `bazel run` process
  tree only after a successful incremental `bazel build`; broken builds keep
  the last healthy target alive.
- Host portability is now enforced structurally: test success stamps are
  executor-owned, Windows uses `cmd.exe /C`, daemon transport uses a
  workspace-published loopback endpoint, cancellation terminates the Windows
  child tree, and CI defines native macOS/Windows compile, unit, daemon and
  command-build/no-op gates.
- Eligible default-target daemon no-ops validate the whole-closure certificate
  inside `frostd` instead of spawning a second `frost`. The invoking client's
  key environment is explicit; certificates with arbitrary `pass_env` values
  conservatively use the normal path. A rotating benchmark separately records
  standalone CLI, daemon CLI and direct socket latency.
- Blobs over 2 MiB now populate a Bazel-compatible FastCDC 2020
  chunk-addressable store and versioned blob manifest. Materialization verifies
  each SHA-256 chunk and the final BLAKE3+executable digest in a private staging
  file before publication; `frost cache stats` reports persistent chunk/byte
  reuse. A dedicated CI job injects corruption, missing/wrong/truncated/single
  chunks, ordering changes and producer/consumer parameter mismatches.
- Residual chunks can carry a positional previous-artifact zstd level-19
  delta when it is smaller than a normal level-3 full-chunk transfer. Restore
  tries exact blob, exact chunk and verified delta before reporting a miss;
  patch, reconstructed chunk and final blob digests are independent gates.
- Independent FastCDC chunk hashing/publication and positioned writes into the
  private restore file now use the bounded Rayon pool while retaining ordered
  manifests and final-blob verification. The checked 64 MiB alternating A/B
  measured 1.41x faster cold publication, 1.89x faster chunk restore and 1.88x
  faster delta restore; the report records all samples and host load.

### Performance

- A checksummed whole-closure no-op certificate bypasses graph, journal and
  per-action validation only after graph-source, toolchain, keyed environment
  and every closure file stat identity match. On the checked 10k linear-chain
  rerun, Frost measured 15.620 ms median versus Ninja's 42.419 ms (2.72x).
  This is a no-op workload result, not a universal language/build claim. A
  separate rotating one-target median-of-31 measured standalone CLI at 2.043
  ms, end-to-end daemon CLI at 1.711 ms and its socket roundtrip at 0.238 ms,
  meeting the local warm-daemon 5-ms target without conflating it with 10k.
- Multi-output CAS publication deduplicates equal digests and publishes
  independent objects in parallel. File hashing now sizes its read buffer from
  8 KiB to 4 MiB instead of allocating 4 MiB for every tiny class/depfile.
  On the alternating-order 100-source Java comparison, Frost batch measured
  510.959 ms clean / 511.646 ms one-change / 2.060 ms no-op versus Gradle's
  574.947 / 578.634 / 553.540 ms (median-of-15).
- The equal-compiler 100-module Rust harness validates executable stdout after
  every sample. Frost's direct crate action measured 282.877 ms clean /
  204.870 ms one-change / 3.575 ms no-op versus Cargo's 417.192 / 237.924 /
  32.455 ms (median-of-7); a median-of-15 focused run confirmed the close
  changed-module result at 209.125 versus 243.408 ms.
- The Go harness separates a `go build` wrapper from a native package
  compiler/linker boundary and validates execution plus normalized module/build
  metadata after every sample. For 100 files, Frost native measured 112.492 ms
  one-change / 3.720 ms no-op versus Go's 156.880 / 137.847 ms; the focused
  median-of-15 clean result was 151.074 versus 160.333 ms. The full
  median-of-7 clean reversal is retained in the report, and multi-package/cgo/
  embed/test plus generated-configuration usability remain open gates.
- The native TypeScript 7 harness byte-compares 101 emitted JavaScript files
  and executes them after every sample. A forward/reverse 14-sample checker
  sweep found four workers best for Frost and two for direct `tsc`. In the
  optimized median-of-7 report Frost measured 259.409 ms clean / 49.391 ms
  one-change / 2.468 ms no-op versus `tsc` at 228.080 / 42.467 / 41.318 ms:
  only the no-op boundary is won (16.7x), while compiler-running scenarios and
  the project-reference/watch/bundling ecosystem remain open.
- The TypeScript project-reference harness compares eight Frost project
  actions with one native `tsc --build` solution and validates 416 emitted
  JavaScript/declaration files plus eight executions after every sample. Outer
  `-j8 × 1 checker` was Frost's best worker split. Frost won no-op at 3.200 vs
  6.556 ms but lost clean (940.313 vs 656.893 ms) and one-project change
  (50.792 vs 44.386 ms); environment load remains recorded in the report.
- The pure-Python wheel harness validates exact 101-source contents,
  Name/Version/tag, every `RECORD` hash and extracted execution after every
  sample. Frost measured 21.295 ms clean / 2.600 ms unchanged / 7.806 ms after
  one source change versus `uv build` at 326.911 / 290.841 / 290.786 ms and
  `python -m build` at 766.806 / 619.512 / 612.785 ms. This wins the minimal
  pure-wheel contract; arbitrary PEP 517 metadata, extensions and pytest stay
  open.

### Fixed

- The manifest-free graph warm path trusted directory mtimes to detect added,
  removed or renamed source entries. NTFS can expose the same parent timestamp
  immediately across an entry mutation, allowing a stale graph to remain
  current. Graph-store v6 also fingerprints sorted native entry names and
  filesystem kinds, so discovery correctness no longer depends on timestamp
  resolution.
- Filesystem access/open/close notifications were treated as source edits.
  Executing a workspace-local Bazel wrapper could therefore trigger an
  unbounded `bazel-dev` build/restart feedback loop. Generic watch, Bazel watch
  and daemon dirty tracking now accept create/modify/remove events and ignore
  access-only events; the success-only Bazel E2E guards the loop.
- Concurrent actions publishing the same CAS digest now use distinct staging
  paths. The former digest-plus-pid name could let two executor threads copy
  through the same inode while one renamed it into the immutable store.
- **A corrupt CAS object was restored and the build reported as current.**
  `materialize` copied an object into place without checking it against the
  digest that names it, so bit rot or a truncated write produced an artifact
  that never existed, delivered as a cache hit. Reproduced by flipping one
  byte: frost said `up to date` and left a binary differing from a correct
  build. Objects are now verified on restore; a bad one is removed and the
  action re-runs. The cost is one hash, only on the restore path.

### Fixed

- The shell frost runs every genrule and shell test through was the one tool
  frost chooses and did not account for. `/bin/sh` now sits in the toolchain
  fingerprint beside the C drivers, so replacing it invalidates the actions
  that depend on it. The manifest has no way to name the shell, which is
  precisely why frost has to.

### Fixed

- A file's mode was not part of its digest, so `chmod -x` on a script a
  genrule runs changed no bytes and frost reported the build as current —
  while a clean build of the same tree failed. The executable bit now joins
  the content digest, and the stat check notices a mode change so the cached
  digest is not reused. The hash cache format is bumped, so the first build
  after upgrading re-hashes.

### Added

- `docs/16_action_key_audit.md` enumerates every input that can change what an
  action produces, whether it reaches the action key, and the argument for
  each deliberate exclusion. Three known gaps are named rather than left to be
  rediscovered: interpreters a genrule invokes, umask, and filesystems with
  whole-second mtime.

### Added

- `frost init` writes a starter manifest for native C/C++ or plain Java sources,
  and the missing-manifest error names it. Native sources become library/
  binary rules; Java becomes one `javac` batch plus a deterministic executable
  or library JAR. Generated builds are exercised as written. Mixed source
  families require `--language`, while Gradle/Maven markers stop Java
  auto-detection so existing dependency/plugin semantics are never silently
  bypassed. It refuses overwrite and supports `--dry-run`.

### Fixed

- A `srcs` or `inputs` glob that matched no files was accepted. A typo like
  `srcs/**/*.c` for `src/**/*.c` produced a library with nothing in it, built
  without complaint, and failed later at the link with a message about symbols
  rather than about the glob. An empty match is now an error naming the target
  and the pattern.

- **Wrong binary returned from cache when an include-path environment variable
  changed.** `CPATH`, `C_INCLUDE_PATH`, `CPLUS_INCLUDE_PATH`, `LIBRARY_PATH`,
  `SDKROOT`, `MACOSX_DEPLOYMENT_TARGET` and `SystemRoot` select which headers
  and libraries a compiler finds, with no change to the command line or to any
  declared input, and none of them were part of the action key. Building with
  `CPATH=/a` and then `CPATH=/b` reported everything cached and left the
  binary built against `/a` in place. These variables are now keyed;
  `PATH`, `HOME`, `TMPDIR`, `TMP` and `TEMP` stay out of the key, since PATH's
  effect on the compiler is already captured by hashing the resolved driver
  binaries and the rest name scratch locations that must not change output.
- Two toolchain fingerprint functions computed different values — one mixed in
  `cc --print-sysroot`, the other did not — and the CLI only ever called the
  weaker one. The unused function is gone, with a note on why the sysroot
  needs no separate treatment: an explicit `--sysroot=` reaches the key
  through argv, a default sysroot is a property of the hashed driver binary,
  and the headers read from it arrive as depfile-discovered inputs.

- `--profile` accepted any name. A typo built with no profile flags into its
  own output tree and said nothing, so `--profile relase` quietly produced a
  different binary than `--profile release`. Once a workspace declares any
  profile, an undeclared name is now an error; `debug` always works, and an
  empty `[profile.<name>]` section still asks for a bare tree on purpose.
- The daemon could not start from a workspace more than a few directories
  deep. Its socket lived inside the workspace, and a Unix socket address is
  capped near 100 bytes, so `frost daemon start` failed with `SUN_LEN` and no
  mention of paths. The socket is now a short, stable name in the user's
  runtime directory, derived from the workspace path so each workspace still
  gets its own daemon.
- A daemon killed rather than shut down left a socket file that blocked every
  later start. A stale socket is now detected and replaced; a live one reports
  that the daemon is already running.
- `frost build --daemon` slept 20 ms after every successful build, to let the
  watcher deliver events for the build's own writes before clearing a counter
  that only `daemon status` reads. Every build paid it. Removed.

### Changed

- The line every build ends with now leads with what happened and drops every
  term that is zero. `frost: 0 executed, 5 cached (5 actions, 0 pruned of 5)
  in 12 ms` reads `frost: up to date · 5 actions · 12 ms`; a partial build
  reads `frost: 2 built, 3 cached · 5 actions · 40 ms`; a failure leads with
  the failure. The share of the graph left out appears only when a subset was
  built (`2 of 9 actions`), since a full build does not need to be told it
  built everything.
- `--stats` no longer reports `0 ms, 0.0%, 0.00x` for a build that executed
  nothing, and distinguishes three cases it previously conflated: the graph
  bounds the build, there is scheduling headroom, or the recorded durations
  are stale and predict a longer critical path than the run took.

- Unknown target, profile and platform names suggest the closest declared one
  instead of printing the whole list: `unknown target "ap". did you mean
  "app"?`. A name that resembles nothing still gets the list, because a wrong
  suggestion is worse than none.

### Performance

- 10k-target no-op build: 285 ms -> 176 ms (-38%), closing the gap to Ninja
  from 6.0x to 3.9x on the same workspace. Three findings, in the order the
  measurements produced them:
  - Completing an action woke every worker. On a dependency chain only one
    action becomes runnable at a time, so `notify_all` cost `actions * jobs`
    wakeups to do `actions` units of work — 50,925 condvar wakeups for 10,000
    actions. Workers are now woken one per newly runnable action.
  - The toolchain fingerprint loaded the workspace-wide content cache, megabytes
    covering every source file, to digest three compiler binaries. It now keeps
    its own stamp and re-hashes only when a driver actually changed.
  - A path that is one action's output and the next action's input was stat'd
    twice. A build is a single point in time, so the second check reuses the
    first result; frost invalidates whenever it writes a path itself.
- The hash cache read path no longer takes a lock, and journal reads take none
  at all: entries recorded by the previous build are immutable during this one.
  (Measured at -1.3% on its own — the contention this removed was not the
  bottleneck. Kept because it is simpler, not because it was the win.)

### Added

- `frost simulate`: compares every scheduler/estimator pair over a sweep of
  worker counts by planning the build rather than running it. Durations come
  from the journal, ordering from the same `Schedule` the engine uses, and no
  cache is touched, so the comparison is deterministic and safe to run
  mid-session. `--json` for CI gating.
- `build --stats`: makespan, worker utilization and distance from the
  estimated critical path, so a real run can calibrate the simulator.
- `frostbuild-bench` is now a measurement library (`Sweep`, `Point`,
  `render_table`) rather than a stub binary.

### Changed

- `--scheduler` and `--estimator` are real. `--estimator` was previously
  accepted and then ignored: every build used journal-or-constant regardless
  of the flag. `learned` now differs from `journal` where it matters — an
  action with no history gets the median duration of its kind from this
  workspace's journal instead of a hardcoded constant.

### Fixed

- The critical-path scheduler degraded after the first wave: actions unlocked
  later were re-prioritized by a cruder key than the one used to build the
  initial ready queue.
- Actions inherited stdin, so a command that reads it (`cat > out` when
  `${in}` expanded to nothing) blocked forever with no diagnostic. Actions now
  get `/dev/null`.
- A Ctrl-C arriving after the raw-TTY dashboard started but before a newly
  spawned action registered its process group could miss that action and leave
  Frost waiting for it. Cancellation and process-group registration now share
  one lock and close that race.

## [0.2.0] - 2026-07-19

### Added

- Multi-platform device builds: `[platform.<name>]` toolchain overlays with
  driver/`arflags`/`sysroot`/flag overrides and a `--platform` flag on build,
  test, plan, graph, compdb, explain and clean. Outputs, graph caches and
  journal identities are isolated per platform, so host and cross builds stay
  warm concurrently; verified end-to-end by an aarch64 (`zig cc`) E2E test.
- `frost query {deps,rdeps,somepath}`: configuration-free target-graph
  queries with `--json` output; `rdeps` is the "what does this change
  affect?" monorepo-CI primitive.
- `docs/14_bazel_gap_analysis.md`: adopt/solved/reject decision record against
  Bazel's capabilities and chronic pain points.
- `docs/15_research_cache_layers.md`: layered cache research direction
  (equivalence / dimension hashes / distance) with adoption priorities.
- Refreshed benchmark evidence on a desktop host (frost vs ninja vs make,
  1k/10k, clean/incremental/no-op): `bench/baselines/2026-07-19-E14-v0.2.0.json`.

### Performance

- Graph construction on deep dependency chains dropped from O(n^3) to
  O(n + edges) via structurally shared transitive export sets (#78):
  a 10k-target linear chain now configures in 275 ms instead of ~19 min,
  with action argv and cache keys byte-for-byte unchanged.
- Manifest-free warm path: the graph store embeds a sources stamp
  (manifest/ignore-file bytes + per-directory mtime_ns) plus the resolved
  toolchain and default targets, so warm invocations of every subcommand
  skip TOML parsing entirely; the hash cache moved from JSON to versioned
  postcard. 10k-target no-op build: 445 ms → 241 ms. Remaining gap to
  Ninja's ~50 ms is tracked in #81 (resident daemon targets <5 ms, #25).

### Changed

- Graph store format bumped to version 3 (platform axis, sources stamp,
  embedded toolchain); stale caches recompile transparently.
- Hash cache lives at `.frost/hashcache.bin`; the legacy JSON file is
  removed opportunistically.

## [0.1.0] - 2026-07-12

- Initial production-capable local engine and reference benchmark suite.
