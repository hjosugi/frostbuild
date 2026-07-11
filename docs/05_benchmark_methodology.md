<!-- i18n: language-switcher -->
[English](05_benchmark_methodology.md) | [日本語](05_benchmark_methodology.ja.md)

# Benchmark methodology

## What to benchmark

Do not only benchmark clean builds.

Real developer productivity depends on:

```text
1. no-op build
2. small incremental change
3. medium package-level change
4. cross-cutting config change
5. clean build
6. test-only change
7. CI cold cache
8. CI warm cache
9. remote cache hit
10. remote execution with artifact download
```

## Metrics

Collect:

```text
wall_time_ms
cpu_time_ms
planner_time_ms
graph_load_time_ms
action_count_total
action_count_executed
action_count_cached
action_count_pruned
cache_hit_rate
cas_upload_bytes
cas_download_bytes
materialized_output_bytes
critical_path_ms
worker_queue_ms
selected_test_count
skipped_test_count
missed_test_failures
```

## Baselines

Compare against:

```text
naive full rebuild
Bazel local
Bazel remote cache
Bazel remote execution
Buck2 if available
Nx/Turborepo for JS workspace
Ninja for C/C++ workspace
```

## Required fairness

Use the same:

```text
source graph
compiler command
machine
parallelism
cache state
output requirements
```

Do not compare:

```text
FrostBuild with warm cache
vs
Bazel with cold cache
```

## How this POC benchmarks

The POC uses a synthetic graph:

```text
20 independent packages
8 modules per package
1 app target depending on package heads
161 build targets total
```

A change in one leaf module affects:

```text
8 modules in one package
+ app
= 9 build targets
```

So the planner prunes:

```text
161 - 9 = 152 targets
```

Run:

```bash
python3 frost.py bench --workspace sample --jobs 8
```

This compares:

```text
micro-partition incremental build
vs
naive full rebuild of all build targets
```

It is a simulation benchmark. It proves the pruning strategy, not compiler speed.

## How to compare with Bazel

If Bazel is installed:

```bash
bash scripts/compare_bazel.sh
```

The sample workspace includes:

```text
sample/MODULE.bazel
sample/BUILD.bazel
sample/tools/gen.py
```

The Bazel comparison is optional because this zip must run even on machines without Bazel.

## Standard Ninja / Make baseline harness

Use `frost-bench` when the comparison target is a conventional timestamp based
builder rather than the FrostBuild simulation:

```bash
./frost-bench run --suite standard --tools ninja,make --sizes 1000,10000 --iterations 5 --jobs 8
```

The standard suite generates identical chain-shaped workspaces for each tool and
size. It records median-of-5 timings for:

```text
clean
noop
incremental_leaf
hot_header
cache_hit_rebuild
```

`cache_hit_rebuild` is marked not applicable for Ninja and Make because this
harness does not add an external content-addressed action cache to those tools.
That keeps the report honest while preserving the scenario slot for FrostBuild
and remote-cache runners.

Reports include host, platform, Python version, CPU count, load average, CPU
governor, and turbo state. Use `--out bench/baselines/<date>-<host>.json` to
commit a reproducible baseline artifact.

From a clean clone, run every current benchmark report with:

```bash
scripts/reproduce.sh
```

The script writes timestamped reports under `bench/results/` and regenerates the
sample Frost POC workspace before running `frost.py bench`, so it does not depend
on stale local output.

## Current baseline

The committed `bench/baselines/2026-07-05-E14.json` was captured on
2026-07-05 with 8 jobs, CPU governor `performance`, and turbo enabled.

Median timings in milliseconds:

```text
tool   size   clean      noop      incremental_leaf   hot_header
ninja  1000   1065.252   5.867     7.519              1041.167
make   1000   1229.647   129.719   126.531            1266.797
ninja  10000  11655.407  49.755    57.099             11618.390
make   10000  30857.041  2104.566  2144.258           31991.726
```

On this synthetic chain, Ninja is much faster than Make for no-op and single
leaf incremental checks at 10k targets. Full-chain clean and hot-header rebuilds
remain dominated by action process fan-out; those are the scenarios FrostBuild
must avoid through pruning, caching, or coarser action batching before claiming
end-to-end wins.

Ninja no-op decomposition for the generated 10k workspace was captured with
`ninja -d stats` because `strace` was not available on this machine:

```text
.ninja parse      1       11.0 ms
.ninja_log load   1        5.0 ms
.ninja_deps load  1        0.0 ms
node stat         20003  176.8 ms
```

The high-level takeaway is that no-op cost is primarily dependency graph
loading plus filesystem metadata checks; FrostBuild's no-op path should
therefore track graph-load time and stat/cache lookup counts separately from
action execution time.

## How to make the benchmark real

Replace simulated `.fb` sources with actual adapters:

```text
TypeScript:
  parse imports with tsserver or swc
  build with tsc/esbuild/bun

Rust:
  parse cargo metadata
  use cargo check/build per crate or finer rustc units

Go:
  use go list -deps -json
  use package-level actions

Python:
  parse imports and pytest collection

Docker:
  treat layers as artifact partitions
```

Then benchmark on a real monorepo.
