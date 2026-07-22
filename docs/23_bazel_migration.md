# Bazel migration support

Frost does not execute Starlark and does not claim drop-in BUILD compatibility.
It does provide a conservative migration scaffold for a useful native C/C++
subset:

```bash
frost -C bazel-workspace import-bazel --dry-run
frost -C bazel-workspace import-bazel

# A smaller graph or Bazelisk/custom binary:
frost -C bazel-workspace import-bazel \
  --query 'deps(//apps/cli:cli)' --bazel /opt/bin/bazelisk
```

The importer asks Bazel itself to load macros and glob results, then consumes
`bazel query --output=xml --noimplicit_deps`. Bazel documents that XML as
including rule attributes plus `rule-input`/`rule-output` graph topology:
<https://bazel.build/versions/9.1.0/query/language#xml>. This avoids pretending
that a hand-written BUILD lexer understands arbitrary Starlark.

## Supported migration subset

- `cc_library`, `cc_binary` and `cc_test`;
- source labels from the same package;
- `deps` and `implementation_deps` between imported native rules;
- `includes`, `copts`, `local_defines`, and binary/test `linkopts`;
- one generated `frost.toml` per Bazel package, with sanitized target names and
  rewritten absolute package labels;
- root workspace/toolchain plus symbol-bearing debug and optimized release
  profiles.

Generated manifests never overwrite existing ones. `--dry-run` prints every
path and body. The full plan is validated before any write, so one unsupported
rule edge cannot leave a half-migrated workspace.

## Developer loop before or after import

Keep Bazel as the source of truth and add a success-only restart loop without
translating the graph:

```bash
frost -C bazel-workspace bazel-dev //apps/server:server -- --port 3000

# Bazelisk/custom binary and build configuration flags:
frost -C bazel-workspace bazel-dev //apps/server:server \
  --bazel /opt/bin/bazelisk --bazel-arg=--config=dev -- --port 3000
```

`bazel-dev` watches the workspace, coalesces editor events, ignores read/access
events, `.git`, `.frost`, and Bazel output-link trees, and asks Bazel itself to
perform each incremental build. In particular, opening the Bazel/Bazelisk
executable cannot feed back into another rebuild. The last successfully
launched `bazel run` process remains alive while a later build is broken;
Frost replaces its complete process tree only after `bazel build` succeeds.
BUILD/Starlark evaluation, configured graph, runfiles, cache and server
ownership all remain Bazel's—Frost adds the developer-loop policy rather than
reimplementing them.

This is process-restart hot reload. Framework-native HMR can run inside the
launched process, but Frost does not synthesize JavaScript module patches or
claim a BEP-based fine-grained update protocol.

Imported `cc_binary` targets are ordinary Frost targets, so the first-party
developer loop applies immediately:

```bash
frost build //apps/cli:cli
frost run //apps/cli:cli
frost dev //apps/cli:cli          # watch + inferred artifact restart
frost debug //apps/cli:cli        # symbol profile + GDB/LLDB
frost ide //apps/cli:cli          # non-overwriting VS Code task/launch files
```

The imported path uses Frost's graph and cache; `bazel-dev` uses Bazel's. Both
keep BUILD/MODULE files untouched and restart only after a successful build.

## Intentional stops

The importer stops on `select()` rather than flattening configuration branches,
and on external repositories, filegroups/generated sources, header-only native
targets, `alwayslink`, shared-library semantics, exported `defines`, include
prefix rewriting, library `linkopts`, data/runfiles, and Bazel make variables.
These affect semantics and cannot be translated honestly by dropping a field.

The generated `cc`/`c++` toolchain is a reviewable scaffold, not an extraction
of Bazel's configured `cc_toolchain`. Keep BUILD/MODULE files until clean,
incremental, test and binary behavior have been compared. Java, Python,
TypeScript and custom rules should initially stay Bazel-owned command
boundaries or receive their own equal-artifact import contract.

This migration surface complements, rather than replaces, Frost's real Bazel
performance harness and the capability ledger in
[14_bazel_gap_analysis.md](14_bazel_gap_analysis.md).
