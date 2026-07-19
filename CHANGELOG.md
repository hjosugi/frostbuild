# Changelog

All notable changes follow Keep a Changelog and Semantic Versioning. Before
1.0, minor versions may contain breaking manifest or CLI changes.

## [Unreleased]

### Added

- Multi-platform device builds: `[platform.<name>]` toolchain overlays with
  driver/`arflags`/`sysroot`/flag overrides and a `--platform` flag on build,
  test, plan, graph, compdb, explain and clean. Outputs, graph caches and
  journal identities are isolated per platform, so host and cross builds stay
  warm concurrently; verified end-to-end by an aarch64 (`zig cc`) E2E test.
- `docs/14_bazel_gap_analysis.md`: adopt/solved/reject decision record against
  Bazel's capabilities and chronic pain points.
- Production Rust engine with C/C++, tests, profiles, packages and globs.
- Crash-safe journal, mmap graph cache, local CAS, daemon, sandbox and tooling.

### Changed

- Graph store format bumped to version 2 (adds the platform axis); stale
  caches recompile transparently.

## [0.1.0] - 2026-07-12

- Initial production-capable local engine and reference benchmark suite.
