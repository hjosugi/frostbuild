# `frost.toml` manifest specification (v1)

Unknown fields are errors. Paths are UTF-8, workspace/package-relative, use `/`,
and may not contain empty, `..`, or absolute components. `srcs` and genrule
`inputs` accept deterministic sorted `*`, `?`, `[]`, and `**` globs. `.frost`,
`.git`, `.gitignore`, and `.frostignore` matches are excluded.

## Workspace and packages

A legacy single-file workspace contains one root `frost.toml` and uses bare
target names. When the root contains `[workspace]`, Frost discovers nested
`frost.toml` files (not through directory symlinks). Nested paths are package
relative. Labels are `//path/to/package:name`; `:name` and `name` resolve in the
current package, while `//:name` addresses a root target. Visibility is a v1
non-goal.

```toml
[workspace]
name = "demo"
default_targets = ["//apps/cli:cli"]
```

## Safe scaffolding

`frost init` scans a directory with no manifest and writes the smallest build
it can describe without guessing package-manager behavior:

- C/C++ becomes native library/binary rules, with `main()` and `include/`
  recognized textually.
- Plain Java becomes one direct `javac` batch and one deterministic JAR. A
  detected package-qualified `main` becomes the JAR `Main-Class`; otherwise the
  result is a library JAR.

If both source families exist, `--language native` or `--language java` is
required so no source family disappears silently. Gradle or Maven project
markers stop automatic Java scaffolding because direct `javac` would omit
dependencies, plugins and lifecycle tasks; use an explicit `kind = "command"`
boundary, or `--language java` only when bypassing those semantics is
intentional. `--dry-run` prints without writing, and an existing `frost.toml`
is never overwritten.

## Toolchain and profiles

```toml
[toolchain]
cc = "cc"          # defaults shown
cxx = "c++"
ar = "ar"
kofunc = "/path/to/kofun/bin/kofun" # optional; required by kofun_binary
cflags = ["-Wall"]
cxxflags = ["-std=c++20"]
ldflags = []

[toolchain.tools]
rustc = "rustc"    # named tools for kind = "command"
javac = "/opt/jdk/bin/javac"

[profile.debug]
cflags = ["-g"]

[profile.release]
cflags = ["-O3", "-DNDEBUG"]
ldflags = ["-s"]
```

`frost build --profile NAME` appends profile flags and writes
`.frost/{obj,lib,bin}/NAME/…`. Profiles coexist and have separate journal keys.
C sources use `cc`; `.cc/.cpp/.cxx/.C/.c++` use `cxx`. Any C++ source makes a
binary link with `cxx`. Compiler, C++ compiler, archiver, configured Kofun
compiler, named command tools, and sysroot identity are fingerprinted into
action keys. A named tool may be on `PATH`, absolute, or workspace-relative;
a workspace-relative wrapper is also a declared action input. C++20 modules
are not v1 functionality.

`arflags` (default `["rcsD"]`) overrides the archiver invocation for toolchains
whose `ar` lacks GNU's deterministic flag.

## Platforms (cross / device builds)

```toml
[platform.aarch64]
cc = "aarch64-linux-gnu-gcc"     # unset drivers inherit [toolchain]
cxx = "aarch64-linux-gnu-g++"
ar = "aarch64-linux-gnu-ar"
kofunc = "device-kofun"          # optional Kofun driver override
arflags = ["rcsD"]               # optional archiver-flag override
sysroot = "sysroots/aarch64"     # expands to --sysroot= on cflags/ldflags
cflags = ["-mcpu=cortex-a53"]    # appended after [toolchain] flags
ldflags = ["-static"]

[platform.aarch64.tools]
codegen = "tools/codegen-aarch64"
```

A platform is a toolchain overlay named in the root manifest; `host` is
reserved for the root `[toolchain]`. `frost build --platform NAME` (also on
`test`, `plan`, `graph`, `compdb`, `explain`, `clean`) selects it and is
orthogonal to `--profile`: outputs land in `.frost/{obj,lib,bin}/NAME/PROFILE/…`
and cache/journal identities carry the platform, so host and device builds stay
warm concurrently and switching between them never rebuilds. The platform's
resolved drivers are fingerprinted per build, so distinct cross-compilers never
share cache entries. Hermetic cross toolchains (for example `zig cc -target
aarch64-linux-musl` behind a wrapper script) work unchanged; genrules and shell
tests still execute on the host.

`frost build --all-platforms` and `frost test --all-platforms` run `host` and
every declared platform, keep going across platform failures, and finish with
one compact status tree. Platform runs are intentionally serialized because
the journal and content cache are shared; actions inside each run remain
parallel.

## C/C++ targets

```toml
[target.util]
kind = "cc_library"              # or cc_binary / cc_test
srcs = ["src/**/*.cpp"]           # required
deps = ["//generated:headers"]
includes = ["include"]            # transitively exported -I paths
cflags = ["-Werror"]
ldflags = ["-lm"]                 # binary/test only
```

Each translation unit gets `-MD -MF`; discovered headers become content inputs.
Generated outputs begin as order-only edges, so an unused generated header does
not invalidate every TU. Libraries use deterministic archives. `cc_test` links
like a binary and adds a cached execution action.

## Kofun targets

```toml
[toolchain]
kofunc = "/path/to/kofun/bin/kofun"

[target.compiler_seed]
kind = "kofun_binary"
srcs = ["src/compiler_seed.kofun"]
```

A `kofun_binary` has exactly one `.kofun` source, matching the current Kofun
CLI's single-input build contract. Frost runs
`kofunc build SOURCE -o BINARY --emit-c GENERATED_C` as one cacheable action.
Both artifacts are declared outputs, while the binary is the target's exported
output. The source and outputs of declared target dependencies are content
inputs. The active Kofun CLI does not expose a library artifact or Make-style
depfile, so `kofun_library` and dynamic Kofun dependency ingestion are not v1
functionality. An unchanged action is served from Frost's action cache.

## Genrules and tests

```toml
[target.generate]
kind = "genrule"
cmd = "tool ${in} -o ${out}"
inputs = ["schema/*.json"]
outputs = ["generated/model.c"]
deps = []
includes = ["generated"]

[target.integration]
kind = "test"
cmd = "scripts/integration.sh"
inputs = ["scripts/integration.sh"]
deps = ["app"]

# Prefer direct argv for language test runners.
[toolchain.tools]
pytest = "python3"

[target.python_unit]
kind = "test"
tool = "pytest"
args = ["-m", "pytest", "-q", "tests/unit"]
inputs = ["pyproject.toml", "src/**/*.py", "tests/unit/**/*.py"]
env = { PYTHONHASHSEED = "0" }
pass_env = ["PYTHONPATH"]
sandbox = false
```

Genrule substitutions are `${in}`, `${out}`, `${outs}` and execute through the
host command shell (`/bin/sh -c` on Unix, `cmd.exe /C` on Windows) at the
workspace root. Authors must quote for that host shell intentionally.
All genrule outputs must exist after success and output ownership is unique.
Tests choose exactly one of `cmd` or `tool`. A named-tool test uses direct argv
and supports `${in}`, `${deps}`, `${config}`, `${profile}` and `${platform}`;
the multi-value forms occupy a whole argument. Its tool, args, declared inputs,
dependency outputs, `env` and `pass_env` are action-key material. Both forms
write the same Frost-owned success stamp only after a zero exit, so result
caching, failure cleanup, `test --affected` and `test --no-cache` behave
identically. Test targets do not declare `outputs`, steps, clean directories or
depfiles.

## Language-neutral command targets

Use `command` when the underlying tool has a real argv interface. Unlike a
genrule, Frost does not invoke a shell, so spaces and metacharacters are passed
literally and the executable is an explicit, fingerprinted toolchain input.

```toml
[toolchain.tools]
javac = "javac"
pack_jar = "frost"

[target.hello_java]
kind = "command"
tool = "javac"
inputs = ["src/Hello.java"]
outputs = [".frost/out/${config}/hello.jar"]
clean_dirs = [".frost/tmp/${config}/hello-classes"]
args = ["-d", "${clean_dir}", "${in}"]
steps = [
  { tool = "pack_jar", args = ["pack-jar", "--input", "${clean_dir}",
                                "--output", "${out}"] }
]
env = { SOURCE_DATE_EPOCH = "0" }
pass_env = ["JAVA_HOME"]
depfile = ".frost/out/${config}/hello.d" # optional Makefile syntax
preserve_outputs = true # opt in only for a compiler that incrementally reuses outputs
sandbox = false
```

Every output and optional depfile must contain `${config}`. It expands to
`PROFILE` on `host` and `PLATFORM/PROFILE` otherwise, preventing debug,
release and cross-device writes from colliding. Command arguments support:

| Variable | Expansion |
|---|---|
| `${in}` | one argv item per declared `inputs` path |
| `${deps}` | one argv item per output of declared target dependencies |
| `${outs}` | one argv item per declared output |
| `${out}` | first declared output |
| `${out_dir}` | parent directory of the first output |
| `${clean_dir}` | first declared clean intermediate directory |
| `${clean_dirs}` | one argv item per clean intermediate directory |
| `${depfile}` | configured depfile path |
| `${config}` | profile or platform/profile output-tree key |
| `${profile}` / `${platform}` | selected names |

The multi-value forms `${in}`, `${deps}`, `${outs}`, and `${clean_dirs}` must
occupy a complete argument. Static `env` values and the present-or-absent value of every
`pass_env` name participate in the action key. All other host variables are
cleared; Frost then supplies its normal deterministic baseline and forces the
locale to `C`.

`steps` adds ordered named-tool invocations to the same atomic action. Every
step is direct argv, uses the same substitutions/environment/sandbox, and joins
the action key together with its tool identity. Frost stops at the first failed
step and never journals partial success. `clean_dirs` names
configuration-isolated intermediate directories that Frost removes and
recreates before the initial execution and a determinism rerun. This prevents
stale generated files—such as a removed Java inner class—from leaking into a
later archive without using an untracked shell wrapper.

Each clean directory is exclusively owned by one action. Configuration rejects
equal or nested clean directories across actions, and rejects a clean directory
that contains any declared graph input or output. Only undeclared intermediate
files belong there; stable final artifacts remain in `outputs`.

By default Frost removes every declared output immediately before an action
reruns. Set `preserve_outputs = true` on a `command` target only when the tool's
incremental protocol reads or deliberately leaves prior outputs in place (for
example native `tsc` with `.tsbuildinfo`). The action key includes this choice.
Every retained file is still content-verified after success, and all compiler
state needed for a safe retry should itself be a declared output; a failed
action removes the possibly mixed output set. Clean builds and `frost clean`
continue to remove the whole configuration output state.

Declared outputs are files, not opaque directory trees. This makes ownership,
digest verification, early cutoff and remote-cache translation unambiguous.
An ecosystem command that produces a variable tree should either expose one
stable boundary artifact (for example a jar), use a small adapter that packs
the tree deterministically, or remain wholly owned by Cargo/npm/Gradle/Maven.
The optional depfile is Makefile-format; tools using another dependency format
need an adapter. `--sandbox` also requires every workspace input to be declared,
so package managers that traverse a module cache normally use `sandbox = false`.

## Incrementality and diagnostics

The BLAKE3 action key covers canonical argv/cwd, environment whitelist,
toolchain closure, declared and discovered content. The binary journal is
append-only and ignores incomplete crash tails. The CAS restores missing output
without execution; byte-identical output cuts off downstream work.

`frost plan`, `build --explain`, `explain TARGET`, `graph --dot`, `compdb`, and
`build --trace FILE` expose planning and execution. `--sandbox` hides undeclared
workspace paths on Linux; `--check-determinism` reruns selected actions.
