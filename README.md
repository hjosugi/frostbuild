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

Clean local cache/output:

```bash
python3 frost.py clean --workspace sample
```

Optional Bazel comparison:

```bash
bash scripts/compare_bazel.sh
```

This requires `bazel` to be installed. If Bazel is missing, the script prints a skip message.

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
