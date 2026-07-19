# Changelog

All notable changes follow Keep a Changelog and Semantic Versioning. Before
1.0, minor versions may contain breaking manifest or CLI changes.

## [Unreleased]

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
  1k/10k, clean/incremental/no-op): `bench/results-2026-07-19-local.json`.

### Changed

- Graph store format bumped to version 2 (adds the platform axis); stale
  caches recompile transparently.

## [0.1.0] - 2026-07-12

- Initial production-capable local engine and reference benchmark suite.
