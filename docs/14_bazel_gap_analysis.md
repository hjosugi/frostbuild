# Bazel gap analysis: what FrostBuild adopts, already solves, or rejects

Decision record, July 2026. Sources: bazel.build docs, blog.bazel.build, the
bazelbuild/bazel issue tracker and community postmortems. Bazel is the most
capable C/C++ monorepo engine in production; this document maps each of its
capabilities and each of its chronic pain points to a FrostBuild decision:
**adopt** (lean subset), **already solved by design**, or **reject with
rationale**. The companion strategy docs are
[00](00_world_fastest_build_tools.md), [01](01_architecture_nix_bazel_micro_partition.md)
and [02](02_two_x_strategy.md).

## Capability decisions

| Bazel capability | Decision | FrostBuild form |
|---|---|---|
| Platforms, toolchains, cross-compilation (`--platforms`, `cc_toolchain`, exec/target configs) | **Adopt — shipped** | `[platform.<name>]` toolchain overlays + `--platform`; per-platform output trees, graph caches and journal namespaces; sysroot expansion; real device builds verified by an aarch64 E2E test. The full feature-configuration DSL (features/action_configs templating) is rejected: the whole resolved toolchain hashes into every action key instead. |
| Query languages (`query`/`cquery`/`aquery`: `deps`, `rdeps`, `somepath`) | **Adopt — shipped** | `frost query deps/rdeps/somepath` reads the stored graph and has JSON output. `compdb`, `plan`, `explain` and the Chrome trace cover the common action-query uses. Affected-test selection is an `rdeps` special case. |
| Remote cache / remote execution (REAPI), Build without the Bytes | **Adopt in v2** (unchanged decision, [07](07_remote_cache_study.md)/[11](11_remote_execution_study.md)) | BLAKE3 digests and relative-path action keys are REAPI-translatable now. BwoB's multi-year "output not found" bug tail is the cautionary tale for keeping v1 fully materialized. |
| Execution-requirement tags (`no-cache`, `no-sandbox`, `local`, …) | **Adopt, but typed** | Bazel's stringly tags are a documented mess (bazel#25399, bazel#6038, bazel#10205). FrostBuild uses typed manifest fields (`sandbox = false` exists today; `cache`/`network` follow the same pattern). |
| Build stamping (`--workspace_status_command`, volatile/stable split) | **Adopt** | The volatile/stable file trick is the industry's best answer to "embed the git SHA without breaking incrementality" and is small to implement. |
| Test encyclopedia: sharding protocol, flaky retries, cached results | **Adopt (partial today)** | Test results are already cached actions with `(cached)` reporting; sharding env protocol and a bounded retry policy are planned small adds. |
| Visibility / `package_group` | **Adopt (lean)** | Natural on `//pkg:target` labels; a `visibility` list on targets, enforced at manifest load. |
| Execution log & cache-miss forensics (`--execution_log_compact_file`) | **Adopt (mostly shipped)** | The journal already records per-action keys/inputs/reasons; `frost explain` surfaces why-rebuilt. A journal-diff subcommand closes the gap. |
| `select()` / configurable attributes | **Adopt tiny subset later** | Per-platform `srcs`/`flags` keyed on declared platforms only. General predicates rejected. |
| Configuration transitions (Starlark, split transitions) | **Reject** | A major source of Bazel's configured-graph blowup and memory pain. FrostBuild's fixed axes (platform × profile) cover the C/C++ need; host tools keep building for host in the same invocation because genrules/tests run host-side. |
| Aspects (user-programmable graph walks) | **Reject** | Requires the providers/rules meta-platform. FrostBuild ships the useful walks built in: compdb, graph, trace, explain. |
| bzlmod / external dependency management | **Reject full system; adopt pinned-fetch subset later** | Registry+MVS+extensions took Bazel three major versions and still churns (bazel#23023). A `[fetch]` table with URL+hash+vendor dir covers C/C++ third-party needs without a resolver. |
| Persistent workers, dynamic execution | **Defer** | Pays off for JVM-ish toolchains; clang/gcc startup is cheap. Revisit with v2 remote racing. |
| Starlark rules/macros platform | **Reject** | The core complexity bet FrostBuild inverts: declarative TOML with a fixed, well-chosen rule vocabulary. Escape hatches are genrules and commands, not an embedded language. |

## Migration surface

`frost import-bazel` asks Bazel to expand macros/globs and imports its documented
query XML for a conservative `cc_library`/`cc_binary`/`cc_test` subset. It
generates package manifests, rewrites labels, supports dry-run and refuses to
overwrite. It rejects `select()`, external repositories and every known native
attribute whose semantics Frost cannot yet preserve. This is a migration
scaffold, not Starlark compatibility or configured-toolchain extraction; the
full contract and stop list are in [23](23_bazel_migration.md). Once imported,
native targets use the same `run`, `dev` (successful-build process restart),
debugger and IDE paths as handwritten Frost targets. Before import,
`frost bazel-dev` keeps Bazel's graph/server/cache authoritative and layers the
same success-only process-tree restart policy over `bazel build`/`bazel run`.
This is restart hot reload, not a BEP module-patch protocol.

## Pain points already solved by design

These are Bazel's highest-thumbs chronic issues and the architectural bet that
avoids each. They are the *reason* FrostBuild's core looks the way it does;
regressions against this table are architecture bugs.

| Bazel pain (evidence) | Root cause | FrostBuild counter-design |
|---|---|---|
| Multi-GB JVM heaps, OOMs, `-Xmx` tuning folklore (bazel discussions #18700) | Whole Skyframe graph retained on a GC heap; RAM trades against incrementality | Graph state is an mmap'd on-disk store; the daemon holds pages, not a heap. No GC, no ceiling flag. |
| Analysis phase dominates wall time; Skymeld only overlaps it (bazel#2906, #13593) | Analysis is *program execution*: Starlark runs to produce the action graph | The action graph is data: manifest parse + fingerprint check, O(changes) warm, mmap-loaded cold. |
| Flag flip wipes the single-slot analysis cache; separate `--output_base` workaround (bazel#14179, #10902, #16804) | One analysis cache slot per server | Configuration (platform, profile, toolchain hash) keys every action; debug↔release↔device switches are lookups, and all states stay warm concurrently. Verified by E2E. |
| Sandbox slowness, worst on macOS (bazel#8230, #10130, #16711) | Per-action symlink forest, O(inputs) syscalls | bubblewrap bind-mounts: O(mounts) per action. (And the reason macOS sandboxing is explicitly out of scope in [09](09_platform_support.md).) |
| Silent host-toolchain autodetection breaks hermeticity (bazel discussions #18332, #9213) | Legacy autoconfig fallback | No autodetect fallback: drivers are declared, and every driver binary hashes into every action key — an apt-upgraded gcc is a cache miss, not staleness. |
| Cold start / server handshake failures (bazel#3602, #5858) | Fast requires a warm JVM server | Native binary, ms startup; the daemon is an optimization, not a requirement — the mmap store keeps serverless invocations warm. |
| "Just run bazel clean" incremental escapes (bazel#12462, #11200, #13135) | Mutable convenience output trees desync from graph state | Outputs are content-addressed in the CAS and re-materialized from the journal; determinism-check mode turns nondeterminism into an error instead of latent cache poison. |
| Ecosystem churn: WORKSPACE→bzlmod forced migration, `--incompatible_*` matrices (bazel#23023) | Extensibility platform + LTS breakage policy | One binary, versioned boring manifest, no external ruleset ecosystem to churn. |
| IDE integration requires third-party "de-Bazeling" aspects (hedronvision extractor) | Post-analysis actions hidden behind wrappers | Actions store real compiler argv; `frost compdb` is first-party and lossless. |
| Windows/macOS friction: path limits, symlink farms, sandbox tax | Linux-first primitives ported literally | Roadmap stance ([09](09_platform_support.md)): copy/hardlink materialization and no-sandbox-hermetic mode are planned as first-class modes, not ports of the Linux model. Device builds for such targets already work today from a Linux host via `[platform.*]` cross toolchains. |

## What Bazel still does better

Honest ledger, kept current so the project never argues against a strawman:
mature remote execution at fleet scale, dynamic execution racing, a decade of
rule ecosystems for dozens of languages, BEP-based dashboard integrations,
coverage pipelines, and organizational features (visibility everywhere,
`package_group`s at Google scale). The v2 remote work and the language-adapter
research ([10](10_language_adapters.md)) are scoped against this list.
