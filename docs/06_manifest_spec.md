# frost.toml manifest specification (v1)

This is the manifest format understood by the Rust `frost` binary
(`crates/frostbuild-cli`). It supersedes the simulation-only format used by
the Python PoC (`frost.py`), which remains supported by the PoC only.

A workspace is a directory containing a single `frost.toml` at its root.
All paths in the manifest are workspace-relative, use forward slashes, and
must not contain `..` or be absolute. Unknown fields anywhere in the
manifest are a hard error (so typos fail loudly instead of being ignored).

## `[workspace]`

```toml
[workspace]
name = "demo"                 # optional, informational
default_targets = ["app"]     # optional
```

`default_targets` names the targets built by a bare `frost build`. When
omitted, it defaults to all `cc_binary` targets, or to all targets if there
are no binaries. Every listed name must exist.

## `[toolchain]`

```toml
[toolchain]
cc = "cc"                     # C compiler, default "cc" (resolved via PATH)
ar = "ar"                     # archiver, default "ar"
cflags = ["-O2", "-Wall"]     # prepended to every compile
ldflags = []                  # prepended to every link
```

The resolved compiler binary is fingerprinted (path, size, mtime) into every
action key, so swapping the compiler invalidates the action cache
(full closure hashing is tracked in issue #28).

## `[target.NAME]`

Target names must match `[A-Za-z0-9_-]+`. Three kinds exist.

### `cc_library`

```toml
[target.util]
kind = "cc_library"
srcs = ["src/util.c"]         # required, one compile action per file
deps = []                     # other targets
includes = ["include"]        # exported -I dirs (visible to dependents)
cflags = []                   # extra flags for this target's compiles
```

Each `srcs` entry compiles to `.frost/obj/NAME/<src>.o` with
`-MD -MF <obj>.d`; the depfile is ingested after each run, so header
dependencies are discovered automatically and precisely. Objects are
archived into `.frost/lib/libNAME.a`.

### `cc_binary`

```toml
[target.app]
kind = "cc_binary"
srcs = ["src/main.c"]
deps = ["util"]
ldflags = ["-lm"]             # extra link flags for this target
```

Links its own objects plus the archives of all (transitive) `cc_library`
deps into `.frost/bin/NAME`.

### `genrule`

```toml
[target.gen_config]
kind = "genrule"
cmd = "sh tools/gen_config.sh ${out}"
inputs = ["tools/gen_config.sh"]
outputs = ["gen/config.h"]
includes = ["gen"]            # optional: export dirs to dependents
```

Runs `cmd` via `/bin/sh -c` with the workspace root as cwd. Substitutions:
`${in}` (inputs, space-joined), `${out}` (first output), `${outs}` (all
outputs, space-joined). Every declared output must exist after the command
succeeds, or the action fails. Two targets may not declare the same output.

Paths containing spaces are not supported in genrule substitutions
(tracked with the other path edge cases in issue #50).

## Include and dependency propagation

`includes` are exported transitively: a target sees its own `includes` plus
those of everything in its dep closure. Library archives propagate to the
linking binary. Genrule outputs propagate as compile inputs of dependent
cc targets, which orders code generation before compilation on the first
build; subsequent builds narrow this via depfiles.

Dependency cycles between targets are a hard error, reported with the
cycle path.

## Incrementality semantics

The action key is the blake3 digest of: command line, cwd, toolchain
fingerprint, and the content digests of every declared and discovered
input. An action is skipped when its key matches the journal
(`.frost/journal.json`) and its recorded outputs are intact on disk.
Because keys depend on input *content* (not mtimes), an action that
re-runs but reproduces byte-identical outputs stops the rebuild from
propagating downstream (early cutoff).

`frost plan` prints what would run and why; `frost build --explain` prints
the reason each action ran (which input changed, command change, or
missing output).
