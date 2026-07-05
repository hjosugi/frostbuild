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
