# Language adapters

Frost has two integration levels:

1. Native C/C++ rules expose translation units and compiler depfiles directly.
2. `kind = "command"` exposes a safe, cacheable boundary around any compiler or
   build tool through a named executable and direct argv.

The second level is implemented and covered by end-to-end tests using real
`rustc`, `go`, `javac`, Python and Node when those tools are installed. It is
language-neutral, not a claim that Frost reimplements every ecosystem.

## Adapter contract

```toml
[toolchain.tools]
compiler = "some-compiler"

[target.unit]
kind = "command"
tool = "compiler"
inputs = ["src/unit.lang"]
outputs = [".frost/out/${config}/unit.artifact"]
args = ["--output", "${out}", "${in}"]
```

The executable bytes, direct argv, declared/discovered inputs, static
environment, opted-in host environment and output digests all participate in
incremental correctness. Per-platform `[platform.NAME.tools]` entries can
replace the driver while keeping the target definition unchanged.

## Ecosystem boundary choices

| Ecosystem | Useful Frost partition | Inner incremental owner | Typical boundary |
|---|---|---|---|
| Rust | crate/package or a direct `rustc` unit | Cargo/rustc | binary, rlib, generated archive |
| TypeScript | project-reference package | `tsc --build` / bundler | declared JS bundle or deterministic archive |
| Go | package or command | Go build cache | binary/archive |
| Java | source unit with `javac`, or module | javac / Gradle / Maven | class, jar |
| Python | generated file/package/test shard | invoked tool | wheel, generated module, stamp |
| Gradle/Maven/npm | project/task boundary | that build tool | jar/bundle/archive |

Duplicating Cargo, Go, Gradle, Maven or npm's private caches would add hashing
and invalidation failure modes without automatically improving pruning. Frost
therefore owns the graph above those boundaries; the ecosystem tool owns work
inside one boundary. A monorepo can mix native C++ targets, Java modules, Go
commands and TypeScript projects in the same dependency graph.

## Practical templates

Direct Rust compiler:

```toml
[toolchain.tools]
rustc = "rustc"

[target.cli]
kind = "command"
tool = "rustc"
inputs = ["src/main.rs"]
outputs = [".frost/out/${config}/cli"]
args = ["src/main.rs", "-o", "${out}"]
```

Go command:

```toml
[toolchain.tools]
go = "go"

[target.server]
kind = "command"
tool = "go"
inputs = ["go.mod", "cmd/server/**/*.go", "internal/**/*.go"]
outputs = [".frost/out/${config}/server"]
args = ["build", "-o", "${out}", "./cmd/server"]
pass_env = ["GOPROXY", "GONOSUMDB", "GOPRIVATE"]
sandbox = false
```

Java source unit:

```toml
[toolchain.tools]
javac = "javac"
pack_jar = "frost"

[target.hello]
kind = "command"
tool = "javac"
inputs = ["src/main/java/**/*.java"]
outputs = [".frost/out/${config}/hello.jar"]
clean_dirs = [".frost/tmp/${config}/hello-classes"]
args = ["--release", "21", "-d", "${clean_dir}", "${in}"]
steps = [
  { tool = "pack_jar", args = ["pack-jar", "--input", "${clean_dir}",
                                "--output", "${out}"] }
]
pass_env = ["JAVA_HOME"]
```

This tracks one stable jar instead of enumerating every class file. Frost resets
the intermediate class tree before both normal and determinism executions, so
removed inner classes cannot survive in the jar. The real-tool E2E test checks
that regression. A clean directory belongs exclusively to this action and may
contain only undeclared intermediates; Frost rejects overlaps and declared
graph paths during configuration. `frost pack-jar` sorts entries, fixes archive
timestamps, emits a standards-compliant manifest and compresses in-process. Add
`--main-class com.example.Main` for an executable JAR. An external JDK `jar`
step remains supported when module or signing options require it.

Pure-Python wheel:

```toml
[toolchain.tools]
pack_wheel = "frost"

[target.python_distribution]
kind = "command"
tool = "pack_wheel"
inputs = ["pyproject.toml", "src/**/*.py"]
outputs = [".frost/out/${config}/my_package-1.2.3-py3-none-any.whl"]
args = ["pack-wheel", "--input", "src", "--distribution", "my-package",
        "--version", "1.2.3", "--output", "${out}"]
sandbox = false
```

`pack-wheel` is deliberately a minimal purelib boundary. It produces a
normalized `py3-none-any` filename, core `METADATA`, `WHEEL`, and a complete
SHA-256/size `RECORD`; it does not execute arbitrary PEP 517 hooks or guess
dynamic project metadata. Use the backend itself in a normal command target
when dependencies, entry points, license files, extensions or other backend
semantics are required. The equal-output comparison and limits are in
[24_python_wheel_comparison.md](24_python_wheel_comparison.md).

Language test runner (the same shape works for JUnit launchers, `cargo test`,
`go test`, npm test runners and linters used as gates):

```toml
[toolchain.tools]
pytest = ".venv/bin/python"

[target.python_unit]
kind = "test"
tool = "pytest"
args = ["-m", "pytest", "-q", "tests/unit"]
inputs = ["pyproject.toml", "src/**/*.py", "tests/unit/**/*.py"]
pass_env = ["PYTHONPATH"]
sandbox = false
```

This is a first-class test action rather than a `command` that happens to run
tests: Frost owns its success stamp, caches only successful results, selects it
with `test --affected`, includes it in `test --all`, and removes the stamp on
failure. Named test tools run direct argv; the older `cmd` form remains for
shell integration suites.

TypeScript project with a wrapper that emits one deterministic archive:

```toml
[toolchain.tools]
ts_adapter = "tools/build-ts-project"

[target.web]
kind = "command"
tool = "ts_adapter"
inputs = ["package.json", "tsconfig.json", "src/**/*.ts", "src/**/*.tsx"]
outputs = [".frost/out/${config}/web.tar"]
args = ["--project", ".", "--output", "${out}", "--profile", "${profile}"]
sandbox = false
```

Native incremental `tsc` can instead declare its emitted JavaScript and
`.tsbuildinfo` files individually and set `preserve_outputs = true`. Frost then
keeps unchanged emitted files in place while `tsc` updates only the affected
subset, but still verifies every declared output before journaling success.

Gradle/Maven/npm wrappers use the same pattern. The wrapper is preferable when
the native command produces an output directory with a versioned or otherwise
dynamic filename: it can select the artifact and pack/copy it to `${out}`
deterministically.

## Honest limits

- `frost init` auto-detects native C/C++ and plain Java. The Java scaffold is a
  batch `javac` plus deterministic JAR, not dependency resolution or a Gradle/
  Maven model. Mixed native/Java trees require `--language`; existing Gradle/
  Maven markers make auto-detection fail with an actionable command-boundary
  message rather than guessing project tasks and artifact names.
- Command outputs are declared files, not dynamic directory trees.
- Only Makefile-format dynamic depfiles are ingested.
- Package-manager diagnostics are captured but not yet semantically decoded.
- The current toolchain fingerprint is configuration-wide: every configured
  named tool must be resolvable, and changing one conservatively invalidates
  actions that use another. Per-action tool-closure keys are needed for
  heterogeneous developer machines and less over-invalidation.
- Jobserver sharing with nested Cargo/Make and module-graph discovery via
  `cargo metadata`, `go list`, TypeScript project references, Gradle Tooling API
  or Maven reactor metadata remain native-adapter work.

Performance is measured per boundary. A fast Frost no-op around a Gradle task
does not prove faster Java compilation; clean, no-op and one-file changes must
all be compared on the same generated project. The Java harness records this
against Gradle and Maven rather than making an unqualified fastest claim.
The Rust harness likewise compares a direct one-crate rustc boundary against
Cargo with the same compiler settings and validates the executable after every
sample. Frost wins the checked dependency-free crate, while Cargo dependency,
build-script, feature and workspace behavior remain explicit open gates; see
[19_rust_cargo_comparison.md](19_rust_cargo_comparison.md).
The Go harness reports both the ordinary `go build` wrapper and a direct
package compiler/linker action. It validates execution plus `go version -m`
metadata and shows why wrapper no-op speed is not compiler speed. The native
single-package result and its generated-configuration cost are in
[20_go_build_comparison.md](20_go_build_comparison.md).
The same acceptance contract for every language is maintained in
[18_polyglot_win_matrix.md](18_polyglot_win_matrix.md).
