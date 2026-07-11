<!-- i18n: language-switcher -->
[English](README.md) | [日本語](README.ja.md)

# FrostBuild POC

FrostBuild is a small proof-of-concept for this idea:

```text
Nix-like correctness
+ Bazel/Buck2-like dependency graph and action cache
+ Snowflake-like micro-partition metadata pruning
= faster incremental builds for large polyglot monorepos
```

This zip contains:

```text
frost.py                     Python prototype build engine
sample/                      synthetic workspace with 161 build targets + Bazel files
scripts/run_poc_benchmark.sh run local POC benchmark
scripts/compare_bazel.sh     optional Bazel comparison if bazel is installed
frost-bench                  median benchmark harness for generated Ninja/Make workspaces
docs/                        build tool knowledge, papers, and 2x strategy
zig_skeleton/                Zig implementation skeleton and design note
```

## Quick start

```bash
cd frostbuild_poc
python3 frost.py bench --workspace sample --jobs 8
```

Expected output shape:

```json
{
  "micro_partition_incremental_s": 0.34,
  "naive_full_rebuild_s": 0.90,
  "speedup_naive_over_micro": 2.6,
  "micro_selected_count": 9,
  "micro_pruned_count": 152,
  "naive_target_count": 161
}
```

The benchmark is a deterministic simulation. It is not claiming that this prototype beats Bazel on real languages. It demonstrates the core strategy: for a local change, rebuild a small affected micro-partition closure instead of the full project closure.

## Commands

Generate a fresh sample workspace:

```bash
python3 frost.py init-sample --out sample --groups 20 --modules-per-group 8 --cost-ms 30 --force
```

Show the cold plan:

```bash
python3 frost.py plan --workspace sample --dry-run
```

Build:

```bash
python3 frost.py build --workspace sample --jobs 8
```

Change one source file:

```bash
printf '\n# local change\n' >> sample/src/pkg05_mod07.fb
```

Show the incremental micro-partition plan:

```bash
python3 frost.py plan --workspace sample --dry-run
```

Run incremental build:

```bash
python3 frost.py build --workspace sample --jobs 8
```

Run affected tests:

```bash
python3 frost.py test --workspace sample --jobs 8
```

Clean local output while preserving the local action cache/CAS:

```bash
python3 frost.py clean --workspace sample
```

Remove output and cache state:

```bash
python3 frost.py clean --workspace sample --cache
```

Optional Bazel comparison:

```bash
bash scripts/compare_bazel.sh
```

This requires `bazel` to be installed. If Bazel is missing, the script prints a skip message.

Run the standard build-tool baseline harness:

```bash
./frost-bench run --suite standard --tools ninja,make --sizes 1000,10000 --iterations 5 --jobs 8
```

The harness generates temporary Ninja and Make workspaces, measures clean,
no-op, leaf incremental, and hot-header rebuild scenarios, records CPU
governor/turbo/load metadata, and emits JSON. Baseline reports live in
`bench/baselines/`.

Published baseline:

```text
Build tool JSON: bench/baselines/2026-07-05-E14.json
Frost POC JSON:  bench/baselines/2026-07-05-E14-frost-poc.json
Host: E14, Linux 7.1.2, x86_64, 8 jobs, CPU governor performance, turbo enabled
```

Frost POC simulation from the linked JSON:

| Selected | Pruned | Micro incremental | Naive rebuild | Speedup |
| ---: | ---: | ---: | ---: | ---: |
| 9 | 152 | 0.2877 s | 0.6703 s | 2.33x |

Median build-tool timings from the linked JSON:

| Tool | Targets | Clean | No-op | Leaf incremental | Hot header |
| --- | ---: | ---: | ---: | ---: | ---: |
| Ninja | 1,000 | 1065.252 ms | 5.867 ms | 7.519 ms | 1041.167 ms |
| Make | 1,000 | 1229.647 ms | 129.719 ms | 126.531 ms | 1266.797 ms |
| Ninja | 10,000 | 11655.407 ms | 49.755 ms | 57.099 ms | 11618.390 ms |
| Make | 10,000 | 30857.041 ms | 2104.566 ms | 2144.258 ms | 31991.726 ms |

Reproduce all current benchmark reports from a clean clone:

```bash
scripts/reproduce.sh
```

For a quick smoke run:

```bash
FROST_BENCH_SIZES=10 FROST_BENCH_ITERATIONS=1 scripts/reproduce.sh
```

## What this POC implements

```text
1. Target graph
2. Reverse dependency graph
3. Source content hashes
4. Local action cache
5. Content-addressed store for outputs
6. Affected-target pruning from changed source files
7. Parallel execution over the selected DAG
8. Optional Bazel workspace for comparison
```

## What this POC does not implement yet

```text
1. Real compiler integration
2. Real Nix sandboxing
3. Remote execution
4. Remote cache protocol
5. Syscall tracing
6. Fine-grained symbol-level dependency inference
7. Safe production-grade regression test selection
8. Zig production engine
```

## Why this can be 2x faster in the right workload

2x is not universally guaranteed. If a build has one unavoidable huge action, no build planner can make it 2x faster without improving that compiler or linker. If a no-op build is already near zero, there is no room for 2x.

But for a large monorepo where most commits touch a small area, a micro-partition planner can beat project-level rebuild by pruning most work before execution.

Example in this sample:

```text
total build targets: 161
changed source:      src/pkg05_mod07.fb
affected targets:    9
pruned targets:      152
```

That is the main performance lever.

## Best next step

To turn this into a serious tool:

```text
Phase 1: keep Python planner, add real adapters for TS/Rust/Go
Phase 2: implement engine core in Zig or Rust
Phase 3: add remote CAS/action cache
Phase 4: add Nix-backed toolchain environments
Phase 5: add syscall-trace and static-analysis based test selection
```

## License

0BSD. You can use, copy, modify, and distribute this project for almost any purpose.
