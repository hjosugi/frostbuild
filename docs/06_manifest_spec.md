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
compiler, and sysroot identity are fingerprinted into action keys. C++20
modules are not v1 functionality.

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

## Genrules and shell tests

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
```

Genrule substitutions are `${in}`, `${out}`, `${outs}` and execute through
`/bin/sh -c` at the workspace root. Authors must shell-quote intentionally.
All genrule outputs must exist after success and output ownership is unique.
Shell tests receive dependency outputs as content inputs and write a success
stamp. `frost test --no-cache` forces successful tests to rerun.

## Incrementality and diagnostics

The BLAKE3 action key covers canonical argv/cwd, environment whitelist,
toolchain closure, declared and discovered content. The binary journal is
append-only and ignores incomplete crash tails. The CAS restores missing output
without execution; byte-identical output cuts off downstream work.

`frost plan`, `build --explain`, `explain TARGET`, `graph --dot`, `compdb`, and
`build --trace FILE` expose planning and execution. `--sandbox` hides undeclared
workspace paths on Linux; `--check-determinism` reruns selected actions.
