# Rust / Cargo comparison

This comparison asks a narrow question: for one dependency-free Rust binary
crate, how much frontend overhead remains when Frost and Cargo use the same
compiler and retain the same kind of compiler-owned incremental state?

It does not compare Frost with Cargo's dependency resolver, build scripts,
features, test runner, publishing workflow or workspace metadata. Cargo remains
the ecosystem owner for those capabilities.

## Equal contract

`frost-bench rust` generates an entry point plus 100 modules for each frontend.
Every module returns one integer and the executable prints their sum. Both
frontends compile the same source names with the same `rustc`, Rust 2021, dev
optimization/debug settings, 256 codegen units, unwind panics, no LTO, no
embedded bitcode and incremental compilation.

Frost runs one direct `rustc` command. Cargo runs its ordinary `cargo build`
path with a local `CARGO_TARGET_DIR`. A clean sample deletes the complete
frontend output tree, including rustc incremental state. No-op and changed-file
samples retain that state. The Rust toolchain and Cargo registry are not
deleted.

The harness validates more than exit status. After every timed build it runs
the produced executable and requires exact stdout. Each changed-file iteration
rewrites the last module with a new integer and independently updates the
expected sum. Cargo and Frost binaries need not be byte-identical because Cargo
supplies crate metadata, but their execution digest must match.

Frontends run round-robin and reverse order on each iteration. Missing tools
are recorded as `skipped`; compilation or validation errors are `failed`.

## Checked result

The full report is
`bench/baselines/2026-07-21-E14-rust.json`: 100 modules, 8 jobs,
median-of-7, performance governor, turbo enabled, and starting one-minute load
1.03. It used:

```text
rustc 1.96.1 (31fca3adb 2026-06-26)
cargo 1.96.2 (356927216 2026-06-26)
frost 0.2.0
```

Median timings:

| Scenario | Frost | Cargo | Cargo / Frost |
|---|---:|---:|---:|
| clean | 282.877 ms | 417.192 ms | 1.47x |
| warmed no-op | 3.575 ms | 32.455 ms | 9.08x |
| one module changed | 204.870 ms | 237.924 ms | 1.16x |

Because the incremental result is relatively close, the focused
`bench/baselines/2026-07-21-E14-rust-incremental-15.json` repeats it 15 times.
Frost measured 209.125 ms and Cargo 243.408 ms, a 1.16x ratio. Cargo's fastest
sample (230.926 ms) was still slower than Frost's slowest (222.388 ms) in that
run.

Both reports have equal execution digests after every timed build. The final
binary byte digests differ, as expected and explicitly reported.

## What was won

Frost wins this checked, dependency-free, single-crate binary contract in all
three scenarios. The result shows that a direct crate boundary can preserve
rustc incrementality while cutting frontend no-op and clean overhead.

It is not yet a universal Rust win. The next Rust gates are a multi-crate
workspace with external dependencies, build scripts/features, test artifacts,
diagnostic quality, IDE metadata and Windows/macOS evidence. A fair design
should import Cargo metadata and let Cargo remain the dependency/package owner
rather than cloning its private cache.

## Reproduce

```bash
cargo build --release --locked --bin frost

FROST_BIN=target/release/frost ./frost-bench rust \
  --tools frost,cargo --size 100 --iterations 7 --jobs 8 \
  --out bench/baselines/<date>-<host>-rust.json

FROST_BIN=target/release/frost ./frost-bench rust \
  --tools frost,cargo --size 100 --scenarios incremental_leaf \
  --iterations 15 --jobs 8 \
  --out bench/baselines/<date>-<host>-rust-incremental-15.json
```
