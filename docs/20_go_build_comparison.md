# Go / `go build` comparison

This comparison separates two integration strategies that must not be confused:

1. `frost-go` invokes `go build` as one Frost action. It can skip a no-op
   invocation, but necessarily adds overhead when Go must run.
2. `frost-native` invokes the selected Go distribution's package compiler and
   linker directly. This is a package boundary, not a wrapper win.

The Go command documentation explicitly permits external build systems to use
lower-level `go tool compile` and `go tool link` invocations. The compiler
documentation defines one invocation as one complete package. This is the
boundary used here:

- <https://go.dev/cmd/go/>
- <https://go.dev/cmd/compile/>

## Equal contract

`frost-bench go` generates one dependency-free main package with an entry point
and 100 value files. The executable prints their sum. All frontends use
Go 1.26.4, `linux/amd64`, `GOAMD64=v1`, `CGO_ENABLED=0`, PGO off, VCS stamping
off and an empty linker build ID.

Each frontend has an isolated Go build cache. Before measurement, its cache is
seeded with only the selected toolchain's runtime export closure. Before every
clean sample, the project cache is reset to that seed. This keeps precompiled
runtime packages while preventing the first frontend from warming the next
frontend's project package.

`frost-native` copies the runtime export closure into a declared, generated
toolchain bundle and fingerprints the selected distribution's `compile` and
`link` executables. It compiles the source package to a resettable intermediate
archive, then links the declared binary as one atomic Frost action.

After every timed build, the harness:

- runs the executable and requires exit 0, empty stdout and exact stderr;
- rewrites the final function to a new integer for every changed-file sample;
- runs `go version -m` and requires the exact module path, compiler, linker
  flags, CGO setting, GOOS, GOARCH and GOAMD64 metadata.

The normalized execution and build-metadata digests are equal across all three
frontends. Compiler-internal binary bytes may differ and are reported
separately.

## Checked results

The full alternating report is
`bench/baselines/2026-07-21-E14-go.json`: 100 value files, 8 jobs and
median-of-7.

| Scenario | Frost native | Frost wrapping Go | `go build` |
|---|---:|---:|---:|
| clean | 176.923 ms | 231.326 ms | 174.778 ms |
| warmed no-op | 3.720 ms | 9.857 ms | 137.847 ms |
| one file changed | 112.492 ms | 193.924 ms | 156.880 ms |

The wrapper demonstrates the structural limit: it loses when the inner tool
runs and wins only when Frost can soundly skip it.

The three-tool clean result is within noise and slightly favors `go build`.
The required focused close-result report,
`bench/baselines/2026-07-21-E14-go-clean-15.json`, compares only the two
contenders for 15 alternating samples:

| Scenario | Frost native | `go build` | Go / Frost |
|---|---:|---:|---:|
| clean | 151.074 ms | 160.333 ms | 1.06x |

For the other scenarios in the full report, `go build` / Frost native is 1.39x
for one changed file and 37.06x for no-op. The report records the full sample
arrays and starting load; the seven-sample clean reversal remains checked in
rather than being hidden.

## What was won

Frost native wins this checked, dependency-free, single-package executable
contract on the focused clean comparison, changed-file build and no-op. The
wrapper path does not win compilation.

This is not yet a general Go ecosystem win. Open gates include:

- a discovered multi-package DAG from `go list -deps`;
- source imports and external modules;
- build constraints, assembly, cgo, `go:embed`, generated code and tests;
- exact diagnostics and IDE metadata;
- Windows and macOS;
- a first-party generator so users never hand-author the generated 17-line,
  2.6 KiB native manifest and version-coupled runtime bundle.

The incumbent remains dramatically simpler for this project: its hand-authored
configuration is a three-line, 43-byte `go.mod`. Native setup must become a
generated CLI feature before it can claim a usability win.

## Reproduce

```bash
cargo build --release --locked --bin frost

FROST_BIN=target/release/frost ./frost-bench go \
  --tools frost-native,frost-go,go \
  --size 100 --iterations 7 --jobs 8 \
  --out bench/baselines/<date>-<host>-go.json

FROST_BIN=target/release/frost ./frost-bench go \
  --tools frost-native,go --scenarios clean \
  --size 100 --iterations 15 --jobs 8 \
  --out bench/baselines/<date>-<host>-go-clean-15.json
```
