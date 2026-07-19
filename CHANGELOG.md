# Changelog

All notable changes follow Keep a Changelog and Semantic Versioning. Before
1.0, minor versions may contain breaking manifest or CLI changes.

## [Unreleased]

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
