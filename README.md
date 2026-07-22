# FrostBuild

FrostBuild is a production-oriented Rust build engine for correct, low-latency
incremental builds in large monorepos. C/C++ has native translation-unit rules;
Rust, Go, Java, TypeScript and ecosystem tools use a language-neutral, directly
executed command adapter. The core idea is to maximize work that never reaches
execution: micro-partition pruning, constructive traces, dependency-narrowed
inputs, early cutoff and content-addressed output restoration.

The normative architecture is [DESIGN.md](DESIGN.md); the manifest specification
is [docs/06_manifest_spec.md](docs/06_manifest_spec.md), and the per-language
definition of “win” is [docs/18_polyglot_win_matrix.md](docs/18_polyglot_win_matrix.md).

## Quick start

```bash
# In a directory that already has plain C/C++ or Java sources:
frost init && frost build

# Mixed native/Java trees require an explicit, reviewable choice:
frost init --language java

cargo build --release --locked
./target/release/frost -C sample_c build
./sample_c/.frost/bin/debug/app                 # frost: 42
./target/release/frost -C sample_c build        # frost: up to date
./target/release/frost -C sample_c build --explain
./target/release/frost -C sample_c plan
./target/release/frost -C sample_c graph --dot
```

Useful production workflows:

```bash
# Isolated debug/release trees
frost -C myrepo build --profile release -j 16

# Cross/device builds: declare [platform.aarch64] in frost.toml, then
frost -C myrepo build --platform aarch64 --profile release
frost -C myrepo build --all-platforms --profile release

# Build and cache test targets; select only affected tests
frost -C myrepo test --affected --explain
frost -C myrepo test --all --no-cache

# Optional workspace sandbox and determinism audit
frost -C myrepo build --sandbox
frost -C myrepo build --check-determinism

# Which scheduling strategy suits this graph? (plans, never builds)
frost -C myrepo simulate --jobs 1,4,16
frost -C myrepo build --stats -j 16          # then calibrate against a real run

# TTY builds use a live dashboard; force stable line output when needed
frost -C myrepo build --no-tui

# Rebuild on edits; restart a direct-argv dev process only after success
frost -C myrepo watch app --run .frost/bin/debug/app

# Or infer that artifact automatically
frost -C myrepo dev app -- --example-argument

# Build and run without remembering the configured output path
frost -C myrepo run app -- --example-argument

# Launch GDB/LLDB, jdb, Node inspect or Python pdb based on the artifact
frost -C myrepo debug app -- --example-argument

# Generate non-overwriting VS Code build/launch configuration
frost -C myrepo ide app --dry-run
frost -C myrepo ide app

# Diagnose required tools separately from optional developer integrations
frost -C myrepo doctor
frost -C myrepo doctor --json

# Workspace-aware target/profile/platform completion and optional fzf picker
source <(COMPLETE=bash frost)             # Bash (Zsh/Fish/PowerShell/Elvish too)
frost pick                                # TAB selects several targets
frost pick --tests

# Deterministic compressed Java archive (also usable as a command step)
frost -C myrepo pack-jar --input .frost/tmp/debug/classes \
  --output .frost/out/debug/app.jar --main-class com.example.Main

# Deterministic standards-compliant pure-Python wheel
frost -C myrepo pack-wheel --input src \
  --distribution my-package --version 1.2.3 \
  --output dist/my_package-1.2.3-py3-none-any.whl

# Graph queries: what does a change affect?
frost -C myrepo query rdeps util
frost -C myrepo query deps app --json
frost -C myrepo query somepath app //libs/util:util

# IDE, trace and persistent service
frost -C myrepo compdb
frost -C myrepo build --trace trace.json
frost -C myrepo build --daemon
frost -C myrepo daemon status
frost -C myrepo cache stats

# Preview a conservative Bazel native-C/C++ migration (never overwrites)
frost -C bazelrepo import-bazel --dry-run

# Keep Bazel authoritative while adding success-only hot restart
frost -C bazelrepo bazel-dev //apps/server:server -- --port 3000
```

## Implemented engine

- native C/C++ compilation, libraries, binaries and `cc_test`; shell or
  named-tool direct-argv tests; genrules; and a direct-argv adapter for any
  declared compiler/build tool
- deterministic glob expansion and multi-package `//package:target` labels
- debug/release/custom profiles with independent output/cache identities
- `[platform.*]` cross/device toolchains with per-platform trees and caches,
  plus one-command host-and-device builds via `--all-platforms`
- dynamic GCC/Clang depfile ingestion and generated-file order-only edges
- parallel critical-path scheduler, captured diagnostics and `--keep-going`
- interactive TTY progress with job slots, cache/timing state, critical path,
  scrollable logs, automatic plain CI/pipe output and `--no-tui`
- stat cache, parallel BLAKE3 hashing and toolchain closure fingerprinting
- append-only crash-tolerant binary journal with per-action flush
- immutable local CAS, digest-verified copy materialization and bounded GC;
  blobs over 2 MiB also receive Bazel-compatible FastCDC chunk manifests for
  verified chunk-level restoration, positional previous-version zstd residual
  deltas and persistent exact/delta reuse accounting; independent chunk
  publication and positioned private-file restoration use the bounded Rayon
  pool without weakening the final digest gate
- early cutoff, affected test selection and opt-in determinism checking
- mmap/versioned graph cache, `plan`, `explain`, Chrome trace and compdb
- `init` safely auto-detects native C/C++ or plain Java; Java becomes one
  direct `javac` batch followed by a deterministic runnable/library JAR. Mixed
  native/Java trees require `--language`, and existing Gradle/Maven markers
  stop auto-detection rather than silently bypassing their dependencies or
  plugins. Explicit command targets cover Rust, Go, TypeScript, Gradle, Maven,
  npm and other tools without a shell intermediary
- deterministic compressed JAR packing with optional `Main-Class`, avoiding a
  second JVM while publishing one stable cacheable Java artifact
- deterministic pure-Python wheel packing with normalized filename,
  `METADATA`/`WHEEL`, and complete SHA-256/size `RECORD`
- static completion generation for Bash, Zsh, Fish, PowerShell, Elvish and
  Nushell; dynamic completion resolves workspace targets, profiles and platforms
- `pick` provides multi-target fuzzy selection when `fzf` is installed
- `query deps/rdeps/somepath` over the target graph with JSON output
- `simulate`: deterministic scheduler comparison from journal durations, and
  `build --stats` to calibrate it against a real run
- per-workspace daemon (Unix socket or Windows loopback endpoint) with an
  in-process verified no-op path, plus `watch`: recursive native filesystem
  events, debounce, self-write filtering and success-only process restart
- `dev`: the same success-only restart loop with target artifact/runtime
  inference, so native/JAR/JavaScript/Python paths do not have to be repeated
- `bazel-dev`: native Bazel incremental build + success-only `bazel run`
  process-tree restart, keeping the last healthy target alive on build failure
- `run`: target-to-artifact discovery plus direct native/Java/JavaScript/Python
  execution, with explicit cross-platform runner support
- `debug`: symbol-profile validation and direct GDB/LLDB, jdb, Node inspector
  or Python pdb launch; native `init` scaffolds explicit `-O0 -g` debug and
  optimized release profiles
- `ide`: artifact-aware VS Code task/launch generation for native, Java,
  JavaScript and Python, with dry-run preview and strict no-overwrite behavior
- `doctor`: graph/toolchain readiness plus optional debugger/runtime/fzf/
  sandbox/Graphviz diagnostics, with matching machine-readable JSON
- opt-in Linux bubblewrap sandbox and process-group cancellation
- Ninja importer, conservative Bazel-query native C/C++ migration importer,
  and reproducible Ninja/Make/Frost/Bazel benchmark harness

Remote cache/execution remain v2 protocol work; the local model is deliberately
REAPI-translatable. See [remote cache](docs/07_remote_cache_study.md) and
[remote execution](docs/11_remote_execution_study.md).

## Repository layout

```text
crates/frostbuild-core/     manifest, graph, hashing, journal, graph store, CAS
crates/frostbuild-store/    stable persistence facade
crates/frostbuild-exec/     scheduler, executor, sandbox, cancellation
crates/frostbuild-daemon/   watcher, framed socket protocol, frostd
crates/frostbuild-cli/      frost command and end-to-end correctness tests
crates/frostbuild-bench/    benchmark binary entry point
sample_c/                   real compiler sample workspace
bench/                      checked-in benchmark baselines
docs/                       design decisions and research outcomes
.github/                    CI, security and release automation
frost.py, sample/           Python reference model and synthetic comparison data
zig_skeleton/               historical, superseded implementation sketch
```

The Rust workspace is authoritative. `frost.py` is retained only as a reference
model for algorithm/benchmark comparison and retires from user-facing workflows
once the Rust correctness suite covers every reference scenario. The Zig
skeleton is historical and is not an implementation choice.

## Manifest sketch

```toml
[workspace]
default_targets = ["app"]

[toolchain]
cc = "cc"
cxx = "c++"
cflags = ["-Wall"]

[profile.release]
cflags = ["-O3", "-DNDEBUG"]

[target.util]
kind = "cc_library"
srcs = ["src/**/*.c"]
includes = ["include"]

[target.app]
kind = "cc_binary"
srcs = ["src/main.cpp"]
deps = ["util"]
```

Package manifests below a root `[workspace]` use package-relative paths and may
depend on labels such as `//libs/util:util`, `:local`, or a local bare name.

For a non-C/C++ tool, declare it once and invoke it without shell parsing:

```toml
[toolchain.tools]
rustc = "rustc"

[target.hello]
kind = "command"
tool = "rustc"
inputs = ["src/main.rs"]
outputs = [".frost/out/${config}/hello"]
args = ["src/main.rs", "-o", "${out}"]
```

The same rule shape is exercised end to end with real `rustc`, `go`, `javac`,
Python and Node installations. Package-manager adapters deliberately leave
Cargo/Go/npm/Gradle/Maven's internal incremental cache authoritative; Frost
partitions their project/task graph and keys declared boundary artifacts. See
[docs/10_language_adapters.md](docs/10_language_adapters.md).

## Completion and interactive selection

Dynamic completion asks the current `frost` binary for candidates, so targets,
profiles and platforms follow the `frost.toml` selected by `-C`:

```bash
# Bash / Zsh (dynamic targets, profiles and platforms)
source <(COMPLETE=bash frost)
source <(COMPLETE=zsh frost)

# Fish
COMPLETE=fish frost | source

# PowerShell
$env:COMPLETE = "powershell"; frost | Out-String | Invoke-Expression
```

Elvish uses the same dynamic `COMPLETE=elvish` protocol. Nushell and startup
files use the complete static command tree from
`frost completions bash|zsh|fish|powershell|elvish|nushell`; Nushell's current
generator does not provide the dynamic callback protocol. `frost pick`
adds an optional `fzf` UI; `--print` makes it useful in scripts and
`--tests` restricts the list to test targets. Completion has no `fzf`
dependency.

## Verification and benchmarks

```bash
cargo test --workspace --all-targets --locked
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
python3 -m unittest discover -s tests

./frost-bench run --suite standard \
  --tools frost,ninja,make --sizes 1000,10000 --iterations 5 --jobs 8

BAZEL_BIN=/path/to/bazel scripts/compare_bazel.sh

FROST_BIN=target/release/frost GRADLE_BIN=/path/to/gradle \
  ./frost-bench java --tools frost-unit,frost-batch,gradle,maven \
  --size 100 --iterations 3 --jobs 8

FROST_BIN=target/release/frost GRADLE_BIN=/path/to/gradle \
  ./frost-bench java --tools frost-jar,gradle-jar,maven-jar \
  --size 100 --iterations 7 --jobs 8

FROST_BIN=target/release/frost ./frost-bench rust \
  --tools frost,cargo --size 100 --iterations 7 --jobs 8

FROST_BIN=target/release/frost ./frost-bench go \
  --tools frost-native,frost-go,go \
  --size 100 --iterations 7 --jobs 8

TSC_BIN=/path/to/typescript-7/native/tsc NODE_BIN=/path/to/node \
FROST_BIN=target/release/frost ./frost-bench typescript \
  --size 100 --iterations 7 --jobs 8 \
  --checkers 1 --frost-checkers 4 --tsc-checkers 2

TSC_BIN=/path/to/typescript-7/native/tsc NODE_BIN=/path/to/node \
FROST_BIN=target/release/frost ./frost-bench typescript-projects \
  --projects 8 --modules 25 --iterations 7 --jobs 8 \
  --frost-checkers 1 --tsc-checkers 2

FROST_BIN=target/release/frost ./frost-bench python \
  --tools frost,python-build,uv \
  --size 100 --iterations 7 --jobs 4

cargo run --release --locked -p frostbuild-bench --bin frost-bench-rs -- \
  daemon-noop --frost "$PWD/target/release/frost" --iterations 31

cargo run --release --locked -p frostbuild-bench --bin frost-bench-rs -- \
  cas --size-mib 64 --iterations 7
```

Results include host/load metadata and medians. Existing baseline JSON is in
`bench/baselines/`; `scripts/reproduce.sh` reproduces the published runs.
The checked-in real Bazel 9.1.0 comparison uses the same generated 1,000-action
linear graph for both tools and verifies its target and dependency-edge sets
before timing. On the recorded E14 run, Frost was 3.17x faster for no-op and
2.89x faster for a leaf-only rebuild. This is a workload-specific local result,
not a universal speed claim; the report also records a high starting load
average and that Bazel had no external CAS configured.

The checked one-target warm no-op report rotates all paths over 31 samples:
standalone CLI measured 2.043 ms, end-to-end daemon CLI 1.711 ms and the daemon
socket roundtrip 0.238 ms. Both local 5-ms gates pass, while the separate 10k
standalone graph remains 15.620 ms; these workloads are intentionally not
conflated.

The checked Java comparison uses byte-identical outputs from the same 100-source
set and records clean/no-op/one-source-change timings for Frost source-unit and
batch layouts, Gradle and Maven. In an alternating-order median-of-15 comparison,
Frost batch was 1.13x faster than Gradle clean, 1.13x faster after one source
change and 269x faster no-op. Micro-partitions still made the changed-source
case faster, but one `javac` JVM per source destroyed clean-build performance;
they need a persistent worker or adaptive batching. These are workload-specific
results, not a universal fastest claim. A separate semantic JAR comparison
uses Frost's deterministic built-in packer; its median-of-7 result beat Gradle
9.3.1 clean, changed-source and no-op by 1.12x, 1.07x and 247x while using a
16-line manifest. See
[docs/17_java_gradle_maven_comparison.md](docs/17_java_gradle_maven_comparison.md).

The checked Rust comparison uses the same dependency-free 100-module crate,
the same rustc and equivalent incremental dev settings. Frost measured
282.877 ms clean, 3.575 ms no-op and 204.870 ms after one module change versus
Cargo's 417.192, 32.455 and 237.924 ms. A focused median-of-15 run confirmed
the close changed-module result at 209.125 versus 243.408 ms. Every timed build
ran the binary and checked exact stdout. This wins the simple crate contract,
not Cargo's dependency/build-script/test ecosystem; see
[docs/19_rust_cargo_comparison.md](docs/19_rust_cargo_comparison.md).

The checked Go comparison separates a `go build` wrapper from a native
package compiler/linker boundary. Frost native measured 112.492 ms after one
file change and 3.720 ms no-op versus `go build` at 156.880 and 137.847 ms.
The three-tool seven-sample clean result slightly favored Go; the required
two-tool median-of-15 confirmation measured Frost at 151.074 ms and Go at
160.333 ms. Execution and normalized `go version -m` metadata match after
every sample. This wins one dependency-free package, not the multi-package,
cgo/embed/test ecosystem, and Go's three-line configuration is still much
simpler. See
[docs/20_go_build_comparison.md](docs/20_go_build_comparison.md).

The checked TypeScript comparison uses native TypeScript 7 for both frontends,
byte-compares all 101 emitted JavaScript files and executes the entrypoint after
every sample. With independently swept checker counts, Frost measured
259.409 ms clean / 49.391 ms one-module change / 2.468 ms no-op versus direct
`tsc` at 228.080 / 42.467 / 41.318 ms. Frost therefore wins only the unchanged
project boundary (16.7x); direct `tsc` remains faster whenever the compiler
runs. In the eight-project-reference report Frost also wins no-op (3.200 vs
6.556 ms), but native `tsc --build` remains faster clean and after one project
change. Generic `frost watch`/process restart now ships; persistent TypeScript
watching, browser HMR, bundling and monorepo runners remain open. See
[docs/21_typescript_tsc_comparison.md](docs/21_typescript_tsc_comparison.md).

The checked Python packaging comparison builds the same 101-source pure wheel
through Frost's standards-compliant packer, `python -m build` and `uv build`.
Frost measured 21.295 ms clean / 2.600 ms unchanged / 7.806 ms after one file
change, versus uv's 326.911 / 290.841 / 290.786 ms. Every timed wheel had
matching source bytes and identity/tag metadata, a fully verified `RECORD`,
and exact execution after extraction. This wins the minimal pure-wheel
contract, not arbitrary PEP 517 backends, extensions or pytest. See
[docs/24_python_wheel_comparison.md](docs/24_python_wheel_comparison.md).

## Installation and versioning

Before 1.0, FrostBuild follows SemVer with breaking changes allowed in minor
versions. Build locally with Cargo, use `cargo install --path crates/frostbuild-cli`,
or download a checksummed Linux, macOS or Windows archive from a tagged GitHub
release. Each archive contains both `frost` and the optional `frostd` daemon.

Contributions follow [CONTRIBUTING.md](CONTRIBUTING.md). Research decisions for
predictive selection, learned scheduling, platform support and language adapters
live in `docs/` and keep safe behavior as the default.

What the action key covers, and the gaps it does not, are enumerated in
[docs/16_action_key_audit.md](docs/16_action_key_audit.md). What was learned in
the 20 July 2026 investigation — including two performance hypotheses that
were wrong and a benchmark that was not measuring what it claimed — is in
[docs/17_session_log_2026-07.md](docs/17_session_log_2026-07.md).

The [issue implementation matrix](docs/13_issue_implementation_matrix.md) maps
the roadmap to code and identifies acceptance gates needing external evidence.
