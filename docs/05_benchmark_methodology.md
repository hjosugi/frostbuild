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

## Real Frost / Bazel comparison

Install Bazel or Bazelisk, then run:

```bash
BAZEL_BIN=/path/to/bazel scripts/compare_bazel.sh
```

The comparison is not a simulation and does not silently skip when Bazel is
missing. It runs the standard harness against the Frost and Bazel binaries and
fails unless both results have status `ok`. The generator writes a Frost
manifest and a `BUILD.bazel` file for the same linear action graph:

```text
1,000 actions
999 dependency edges
one source input and one shared header per action
the same output-writing shell operation
```

Before timing, the harness parses both generated manifests and rejects any
target-set or dependency-edge mismatch. The report stores the verified graph
contract, its edge digest, both tool versions, all samples, and host/load
metadata. Bazel's cache-restore scenario is explicitly not applicable because
this local harness does not configure an external CAS.

## Standard Ninja / Make / Frost / Bazel baseline harness

Use `frost-bench` when the comparison target is a conventional timestamp based
builder rather than the FrostBuild simulation:

```bash
./frost-bench run --suite standard --tools ninja,make --sizes 1000,10000 --iterations 5 --jobs 8
```

The standard suite generates equivalent chain-shaped workspaces for each tool
and size. It records median-of-5 timings for:

```text
clean
noop
incremental_leaf
hot_header
cache_hit_rebuild
```

During focused iteration, pass a comma-separated subset such as
`--scenarios noop` or `--scenarios noop,incremental_leaf`. The selected
scenario list is recorded in the JSON; omitting the option still runs the
complete standard suite above.

`cache_hit_rebuild` is marked not applicable for Ninja, Make, and local Bazel
because this harness does not add an external content-addressed action cache to
those tools. That keeps the report honest while preserving the scenario slot
for FrostBuild and future remote-cache runners.

Reports include host, platform, Python version, CPU count, load average, CPU
governor, and turbo state. Environment metadata is captured before measured
work begins, so the recorded load average is not the load created by the
benchmark itself. Use `--out bench/baselines/<date>-<host>.json` to commit a
reproducible baseline artifact.

From a clean clone, run every current benchmark report with:

```bash
scripts/reproduce.sh
```

The script writes timestamped reports under `bench/results/` and regenerates the
sample Frost POC workspace before running `frost.py bench`, so it does not depend
on stale local output.

## Current baseline

The committed `bench/baselines/2026-07-20-E14-frost-bazel.json` is a real
Frost 0.2.0 / Bazel 9.1.0 run captured on 2026-07-20 with 8 jobs, performance
governor, turbo enabled, and the verified 1,000-action/999-edge graph. The
starting one-minute load average was 10.96, so these numbers must not be
generalized to an idle host.

Median timings in milliseconds:

```text
tool    clean      noop     incremental_leaf   hot_header
frost   5212.958   55.335   61.136             3036.959
bazel   17349.777  175.482  176.730            8639.148
```

For this generated local workload, the measured Bazel/Frost ratios were 3.33x
clean, 3.17x no-op, 2.89x leaf-only incremental, and 2.84x shared-header
rebuild. Bazel ran without an external CAS, so no remote-cache comparison is
claimed.

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

## Java / Gradle / Maven harness

The Java suite compares source-unit and batched Frost command adapters with
Gradle and Maven:

```bash
FROST_BIN=target/release/frost \
GRADLE_BIN=/path/to/gradle \
./frost-bench java \
  --tools frost-unit,frost-batch,gradle,maven \
  --size 100 --iterations 3 --jobs 8 \
  --out bench/baselines/<date>-<host>-java.json
```

It generates the same independent Java source set, compiles with JDK release
21, verifies every expected `.class`, and compares the aggregate class digest.
Scenarios are `clean`, `noop`, and `incremental_leaf`. The two Frost layouts
make the micro-partition trade-off measurable: one JVM per source exposes
maximum pruning but expensive clean process fan-out; one batch minimizes JVM
startup but recompiles the full source set after a change. Configuration bytes,
lines and declared partition count are recorded as basic usability evidence.
Measured tools run round-robin, and their order reverses on every iteration, to
avoid systematically giving one frontend the same thermal or filesystem-cache
position. The checked comparison includes a median-of-3 four-tool report and an
alternating-order median-of-15 focused Frost/Gradle report.

Packaging is measured separately with `frost-jar`, `gradle-jar` and
`maven-jar`. Each produces one JAR containing the same required class binary
names. Validation hashes the class entry names and bytes, excluding container
metadata and compression so semantically equivalent JARs can be compared. This
report also records manifest bytes/lines as adoption-cost evidence.
See [17_java_gradle_maven_comparison.md](17_java_gradle_maven_comparison.md).

## Rust / Cargo harness

The Rust suite compares one direct Frost `rustc` crate action with Cargo's
ordinary incremental dev build:

```bash
FROST_BIN=target/release/frost ./frost-bench rust \
  --tools frost,cargo --size 100 --iterations 7 --jobs 8 \
  --out bench/baselines/<date>-<host>-rust.json
```

It generates the same 100 modules plus entry point, pins both frontends to the
same rustc and equivalent dev codegen settings, and isolates Cargo from any
user-global `CARGO_TARGET_DIR`. Clean samples delete each frontend's output and
rustc incremental state; no-op and one-module-change samples retain their
normal compiler cache. After every timed build, the harness executes the binary
and checks exact stdout against an independently maintained expected sum.

Binary bytes may differ because Cargo adds crate metadata, so semantic execution
digests are the equality gate. Tool order reverses each iteration. The checked
median-of-7 report and median-of-15 close-result confirmation are analyzed in
[19_rust_cargo_comparison.md](19_rust_cargo_comparison.md).

## Go / `go build` harness

The Go suite distinguishes a wrapper from a native package boundary:

```bash
FROST_BIN=target/release/frost ./frost-bench go \
  --tools frost-native,frost-go,go \
  --size 100 --iterations 7 --jobs 8 \
  --out bench/baselines/<date>-<host>-go.json
```

All three frontends receive the same dependency-free main package. Isolated Go
caches are reset to a runtime-only seed before clean samples, preventing
cross-frontend package-cache reuse without rebuilding the standard runtime.
`frost-go` invokes the incumbent and measures wrapper overhead.
`frost-native` invokes the selected distribution's package compiler and linker
with a declared runtime export closure.

Every timed build is followed by executable validation and an exact normalized
`go version -m` metadata check. The close clean result is repeated for 15
alternating samples with only Frost native and `go build`. Method, both clean
results, current limits and usability cost are in
[20_go_build_comparison.md](20_go_build_comparison.md).

## TypeScript / native `tsc` harness

```bash
TSC_BIN=/path/to/typescript-7/native/tsc NODE_BIN=/path/to/node \
FROST_BIN=target/release/frost ./frost-bench typescript \
  --tools frost,tsc --size 100 --iterations 7 --jobs 8 \
  --checkers 1 --frost-checkers 4 --tsc-checkers 2 \
  --out bench/baselines/<date>-<host>-typescript.json
```

Both frontends use the same native compiler, strict incremental project and
source graph. The harness copies the compiler-side standard declarations into
each workspace, fingerprints the executable, declares the declarations as
Frost inputs, compares every emitted JavaScript name and byte, and executes the
entrypoint after every timed build. `--checkers` controls both frontends;
frontend-specific overrides are valid only after a recorded parallel sweep.
The checked sweep uses forward and reverse setting order so page-cache and
thermal position do not select the winner. Method, results and limits are in
[21_typescript_tsc_comparison.md](21_typescript_tsc_comparison.md).

The companion `typescript-projects` command generates independent composite
projects plus one root project-reference solution. Frost schedules one compiler
action per project; direct `tsc --build` owns the solution. It records both the
outer action limit and checker workers per process so `jobs × checkers`
oversubscription is visible, then validates all JavaScript/declaration bytes and
executes every project.

## Python wheel harness

```bash
FROST_BIN=target/release/frost ./frost-bench python \
  --tools frost,python-build,uv --size 100 --iterations 7 --jobs 4 \
  --out bench/baselines/<date>-<host>-python-wheel.json
```

The three frontends package the same pure-Python source tree. `python-build`
and `uv` both call `setuptools.build_meta` without build isolation; Frost uses
the deterministic built-in `pack-wheel`. Every timed wheel is checked for
exact source names/bytes, core identity metadata, pure-Python compatibility
tag, complete SHA-256/size `RECORD`, and exact execution after extraction.
Optional backend metadata and ZIP layout remain outside the equality contract
and complete-archive digests are reported separately. Clean samples remove
workspace output/backend state while retaining installed tools and uv's cache.
Results and current unsupported packaging semantics are analyzed in
[24_python_wheel_comparison.md](24_python_wheel_comparison.md).

## FastCDC / DeltaCDC CAS harness

The low-level Rust harness separates output hashing, immutable whole-blob
publication, exact restore, exact-chunk restore, one-byte delta publication and
delta restore:

```bash
cargo run --release --locked -p frostbuild-bench --bin frost-bench-rs -- \
  cas --size-mib 64 --iterations 7 \
  --out bench/baselines/<date>-<host>-cas.json
```

The fixture is deterministic pseudo-random data. Whole objects are deleted
before chunk measurements; the newly changed chunk is deleted before *each*
delta measurement. Serial and automatic-parallel modes run in alternating
order within every iteration, so neither mode always receives the warmer page
cache or cooler CPU. The report retains every sample plus OS/architecture,
logical CPU count, `RAYON_NUM_THREADS`, starting load average, CPU governor,
turbo state when discoverable, and temp root.

The checked
[`2026-07-21-E14-cas.json`](../bench/baselines/2026-07-21-E14-cas.json)
used a 64 MiB fixture and median-of-7 on 8 logical CPUs. It began at load
average 11.13 / 16.35 / 20.54, governor `powersave`, with the fixture under
`/tmp`:

| Verified CAS path | Serial median | Parallel median | Speedup |
|---|---:|---:|---:|
| Cold blob + FastCDC publish | 136.920 ms | 96.996 ms | 1.41x |
| Exact-chunk restore | 73.910 ms | 39.210 ms | 1.89x |
| Delta-backed chunk restore | 75.647 ms | 40.196 ms | 1.88x |

The exact whole-blob restore median was 39.537 ms; a one-byte update plus
parallel chunk/delta publication was 126.105 ms. Across the initial artifact
and seven one-byte versions, 86.75% of logical chunk bytes were reused and the
seven retained residual patches totaled 518 bytes. This is evidence that
bounded chunk parallelism helps on this machine. It is not an SSD durability
benchmark (`fsync` is outside the current local CAS contract), a remote-transfer
measurement, or evidence that DeltaCDC beats whole-blob compression on every
corpus.

## Warm daemon no-op harness

The Rust harness separates three latencies that must not be conflated:

```bash
cargo run --release --locked -p frostbuild-bench --bin frost-bench-rs -- \
  daemon-noop --frost /absolute/path/to/release/frost --iterations 31 \
  --out bench/baselines/<date>-<host>-daemon-noop.json
```

It generates and builds a one-target native workspace, starts an in-process
daemon, warms all paths, then rotates standalone CLI, `frost build --daemon`
CLI and direct framed-socket certificate requests. Every sample must report
`up to date`; the direct request names a nonexistent fallback program, proving
a hit did not spawn another process. The report keeps raw samples, before/after
load, governor and temp root, and evaluates server and end-to-end 5-ms gates
separately.

The checked local report is
[`2026-07-22-issue-25-daemon-noop.json`](../bench/baselines/2026-07-22-issue-25-daemon-noop.json).
At starting load average 5.49 / 20.14 / 27.78 on 8 logical CPUs with the
performance governor, its medians were 2.043 ms standalone CLI, 1.711 ms daemon
CLI and 0.238 ms daemon socket roundtrip. Both the server and user-visible
end-to-end 5-ms gates pass for this one-target certificate, and daemon mode is
1.19x faster than standalone. This proves the former second-process
architecture is gone; it does not imply sub-5-ms for a 10k-file certificate or
under arbitrary contention. The report keeps the earlier workload distinct
from the checked 10k standalone result.

## 10k daemon versus Ninja harness

The large-graph harness generates one 10,000-target linear genrule graph with
equal commands and declared inputs for Frost and Ninja:

```bash
cargo run --release --locked -p frostbuild-bench --bin frost-bench-rs -- \
  daemon-graph --frost /absolute/path/to/release/frost \
  --targets 10000 --iterations 31 \
  --out bench/baselines/<date>-<host>-daemon-10k.json
```

After building both graphs it starts the daemon and warms every path. No-op
iterations rotate standalone Frost, end-to-end daemon CLI, direct framed
socket and Ninja so no path always runs first. The harness then changes the
last source once per iteration and alternates Frost/Ninja order, measuring a
one-file, one-action leaf rebuild without silently treating that result as a
no-op win. Every no-op must explicitly report no work, and the direct socket
uses a nonexistent fallback executable to prove the response stayed inside
the daemon.

The watcher shortcut is installed only after the ordinary checksummed
certificate validates graph, toolchain, key environment and every recorded
input/output identity. It additionally requires all file evidence to be an
existing, non-symlink path below the workspace and outside `.git`. A quick hit
still checks the certificate identity, toolchain and client environment, then
waits for a marker event that drains earlier watcher events. Source, manifest
or `.frost` output events invalidate the proof. Missing/external/symlinked
evidence, arbitrary `pass_env`, watcher errors and marker timeouts all use the
complete certificate or child-build fallback instead.

The checked report is
[`2026-07-23-issue-25-daemon-10k.json`](../bench/baselines/2026-07-23-issue-25-daemon-10k.json).
It used Ninja 1.13.2, 8 logical CPUs, the performance governor and a starting
load average of 9.79 / 8.67 / 5.78:

| 10k linear graph, median of 31 | Time | Ninja / Frost |
|---|---:|---:|
| Frost standalone no-op | 30.453 ms | 2.06x |
| Frost daemon CLI no-op | **2.396 ms** | **26.17x** |
| Frost direct socket no-op | 0.229 ms | 273.75x |
| Ninja no-op | 62.693 ms | 1.00x |
| Frost daemon leaf change | 390.660 ms | — |
| Ninja leaf change | 63.764 ms | — |

This passes #25's exact 10k daemon median below 5 ms and greater-than-2x Ninja
gates on the recorded host. It is not evidence that Frost wins a changed leaf;
the same report shows the opposite, and no claim should omit that result or the
recorded load.

## How to make the benchmark real

Replace simulated `.fb` sources with actual adapters:

```text
TypeScript:
  extend the checked native-tsc project with project references and watch
  measure equal bundling artifacts separately with esbuild/bun

Rust:
  extend the checked single-crate harness with cargo metadata
  cover multi-crate dependencies, features, build scripts and tests

Go:
  extend the checked package compiler/linker boundary with go list -deps -json
  cover imports/modules, build constraints, cgo, embed and tests

Python:
  extend the checked pure-wheel contract to PEP 517 metadata/extensions
  parse imports and pytest collection

Docker:
  treat layers as artifact partitions
```

Then benchmark on a real monorepo.
