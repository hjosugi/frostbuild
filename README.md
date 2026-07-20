# FrostBuild

FrostBuild is a production-oriented Rust build engine for correct, low-latency
incremental C/C++ builds in large monorepos. The core idea is to maximize work
that never reaches execution: micro-partition pruning, constructive traces,
depfile-narrowed inputs, early cutoff and content-addressed output restoration.

The normative architecture is [DESIGN.md](DESIGN.md); the manifest specification
is [docs/06_manifest_spec.md](docs/06_manifest_spec.md).

## Quick start

```bash
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

# Build and cache test targets; select only affected tests
frost -C myrepo test --affected --explain
frost -C myrepo test --all --no-cache

# Optional workspace sandbox and determinism audit
frost -C myrepo build --sandbox
frost -C myrepo build --check-determinism

# Which scheduling strategy suits this graph? (plans, never builds)
frost -C myrepo simulate --jobs 1,4,16
frost -C myrepo build --stats -j 16          # then calibrate against a real run

# Graph queries: what does a change affect?
frost -C myrepo query rdeps util
frost -C myrepo query deps app --json
frost -C myrepo query somepath app //libs/util:util

# IDE, trace and persistent service
frost -C myrepo compdb
frost -C myrepo build --trace trace.json
frost -C myrepo build --daemon
frost -C myrepo daemon status
```

## Implemented engine

- real C/C++ compilation, libraries, binaries, `cc_test`, shell tests, genrules
- deterministic glob expansion and multi-package `//package:target` labels
- debug/release/custom profiles with independent output/cache identities
- `[platform.*]` cross/device toolchains with per-platform trees and caches
- dynamic GCC/Clang depfile ingestion and generated-file order-only edges
- parallel critical-path scheduler, captured diagnostics and `--keep-going`
- stat cache, parallel BLAKE3 hashing and toolchain closure fingerprinting
- append-only crash-tolerant binary journal with per-action flush
- immutable local CAS, hardlink/copy materialization and bounded GC
- early cutoff, affected test selection and opt-in determinism checking
- mmap/versioned graph cache, `plan`, `explain`, Chrome trace and compdb
- `query deps/rdeps/somepath` over the target graph with JSON output
- `simulate`: deterministic scheduler comparison from journal durations, and
  `build --stats` to calibrate it against a real run
- Unix-socket daemon, recursive watcher, protocol versioning and fallback
- opt-in Linux bubblewrap sandbox and process-group cancellation
- Ninja subset importer and reproducible Ninja/Make/Frost benchmark harness

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

## Verification and benchmarks

```bash
cargo test --workspace --all-targets --locked
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
python3 -m unittest discover -s tests

./frost-bench run --suite standard \
  --tools frost,ninja,make --sizes 1000,10000 --iterations 5 --jobs 8
```

Results include host/load metadata and medians. Existing baseline JSON is in
`bench/baselines/`; `scripts/reproduce.sh` reproduces the published runs. A 2x
claim is workload-specific, never universal, and must be backed by harness JSON.

## Installation and versioning

Before 1.0, FrostBuild follows SemVer with breaking changes allowed in minor
versions. Build locally with Cargo, use `cargo install --path crates/frostbuild-cli`,
or download the static Linux binary and checksum from a tagged GitHub release.

Contributions follow [CONTRIBUTING.md](CONTRIBUTING.md). Research decisions for
predictive selection, learned scheduling, platform support and language adapters
live in `docs/` and keep safe behavior as the default.

The [issue implementation matrix](docs/13_issue_implementation_matrix.md) maps
the roadmap to code and identifies acceptance gates needing external evidence.
