# Changelog

All notable changes follow Keep a Changelog and Semantic Versioning. Before
1.0, minor versions may contain breaking manifest or CLI changes.

## [Unreleased]

### Added

- `frost init` writes a starter manifest for the C or C++ sources already in a
  directory, and the missing-manifest error names it: running frost somewhere
  new used to end at `missing frost.toml` with no next step. The scaffold
  reports what it found, splits an entry point from library code, and is
  expected to build as written — a scaffold that does not is a different dead
  end. It refuses to overwrite an existing manifest and has `--dry-run`.

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
