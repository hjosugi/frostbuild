# How to be 2x faster than current tools

## First: universal 2x is impossible

A build tool cannot be 2x faster for every workload.

Counterexamples:

```text
1. no-op build already takes 50 ms
2. build has one unavoidable 10-minute linker action
3. target is intentionally full release build
4. cache is cold and remote workers are unavailable
5. compiler dominates all runtime and cannot be parallelized
```

So the correct product claim is:

```text
2x faster for common incremental monorepo CI/local builds with small changesets.
```

That is achievable.

## Main path to 2x

The main lever is not running faster commands. The main lever is running fewer commands.

```text
speedup = old_work / new_work
```

If current tool rebuilds 160 partitions and FrostBuild rebuilds 9:

```text
work reduction = 160 / 9 = 17.8x
```

Even with planner overhead, 2x is realistic.

## 1. Micro-partition pruning

Current tools often stop at project/target granularity.

FrostBuild should go smaller:

```text
project -> package -> module -> symbol/test partition
```

Use a metadata catalog:

```text
changed file
  -> changed partition
  -> reverse dependency closure
  -> affected tests
  -> cache lookup
  -> final execution set
```

This is where Snowflake-style metadata pruning enters.

## 2. Safe regression test selection

Testing often dominates CI.

Use two sources:

```text
static dependency graph:
  imports, class references, package references

dynamic dependency graph:
  syscall/file tracing, class loader tracing, coverage data
```

Then select tests by dependency.

Default rule:

```text
if unsure, run more tests
```

Risk control:

```text
- nightly full test run
- periodic random test sampling
- quarantine flaky tests
- compare selected vs full run accuracy
- never fail closed if selection service is down; fall back to build tool baseline
```

## 3. Lazy output materialization

Remote execution can be slow if the client downloads too many artifacts.

Better:

```text
store all outputs in CAS
materialize only requested outputs
keep metadata for the rest
```

This is similar to Buck2 deferred materialization.

## 4. Cost-based scheduler

A normal scheduler only sees ready actions.

A better scheduler sees:

```text
- critical path
- action duration history
- cache locality
- worker toolchain availability
- output size
- network bandwidth
- failure rate
```

Then it schedules actions like a query planner.

```text
large remote-cache hit output:
  keep remote, do not download unless needed

long compile with cache miss:
  remote worker

short action with tiny output:
  local worker may be faster than RPC
```

## 5. Persistent workers and compiler daemons

Many builds waste time starting compilers.

Use:

```text
- TypeScript server / tsserver-like incremental state
- Rust compiler cache / sccache-style behavior
- Kotlin incremental compiler integration
- JVM persistent workers
- long-lived language workers
```

Meta reported some Kotlin modules building up to 3x faster after integrating Kotlin incremental compilation in Buck2. That suggests compiler-level incrementalism can stack with build-graph pruning.

## 6. Nix-style environment snapshots

A build cache is only correct if the environment is included in the key.

```text
bad key:
  source hash + command

good key:
  source hash + command + toolchain closure + platform + config
```

Nix-style closure hashing improves correctness and cache sharing.

## 7. Dynamic dependencies without sacrificing correctness

Some dependencies are not visible statically:

```text
codegen
reflection
dynamic import
C/C++ header discovery
Rust build.rs
TypeScript path aliases
config files
```

Use:

```text
- sandboxed execution
- syscall tracing
- compiler depfiles
- dynamic dependency recording
```

Then update the catalog after each action.

## Proposed 2x roadmap

```text
MVP:
  target-level micro-partition planner
  local CAS/action cache
  benchmark vs naive and Bazel if installed

v0.2:
  language adapters for TS/Rust/Go/Python
  import graph extraction
  affected test selection

v0.3:
  Nix dev shell / flake environment hash
  remote CAS

v0.4:
  REAPI-compatible remote execution
  lazy materialization

v0.5:
  cost-based scheduler
  historical duration DB
  cache locality routing
```

## The most likely 2x wins

Ranked by impact:

```text
1. test selection
2. micro-partition pruning below project level
3. remote cache hit rate improvement
4. lazy materialization
5. persistent compiler workers
6. remote execution scheduling
7. lower engine overhead using Zig/Rust
```

## What not to do first

Do not start by rewriting everything in Zig.

First, prove the algorithmic win:

```text
Does the planner safely reduce selected work by 50%+?
Does it miss zero required actions in safe mode?
Does it reduce CI wall time with real workloads?
```

After that, rewrite the hot engine paths.
