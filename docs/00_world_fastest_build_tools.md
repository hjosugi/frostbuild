<!-- i18n: language-switcher -->
[English](00_world_fastest_build_tools.md) | [日本語](00_world_fastest_build_tools.ja.md)

# 現在の高速 build tool の見取り図

## 結論

「世界で一番速い build tool」は、workload によって変わる。

```text
large polyglot monorepo:
  Buck2 / Bazel / Pants / Please

C/C++ local incremental build:
  Ninja

JS/TS monorepo:
  Turborepo / Nx

reproducible environment/package build:
  Nix
```

単独で万能の1位はない。理由は、build performance は次で決まるから。

```text
1. clean build or incremental build
2. local only or remote execution
3. one huge linker action or many small parallel actions
4. language ecosystem
5. cache hit rate
6. dependency graph accuracy
7. test selection quality
8. artifact download/materialization cost
```

## Buck2

Buck2 is one of the strongest candidates for large monorepo incremental builds.

Important ideas:

```text
- Rust implementation
- single incremental dependency graph
- more parallelism
- dynamic dependencies
- remote execution support
- deferred materialization
```

Meta says Buck2 is up to 2x faster than Buck1 in practice. That is not the same as saying Buck2 is always 2x faster than Bazel, but it shows that build engine architecture alone can create a large win.

References:

- https://github.com/facebook/buck2
- https://engineering.fb.com/2023/04/06/open-source/buck2-open-source-large-scale-build-system/
- https://buck2.build/docs/about/benefits/compared_to_buck1/
- https://buck2.build/docs/users/advanced/deferred_materialization/

## Bazel

Bazel is the strongest established baseline for large multi-language builds.

Important ideas:

```text
- explicit dependency graph
- parallel execution
- local cache
- remote cache
- remote execution
- action cache + content-addressable store
- large ecosystem of rules
```

Bazel is harder to beat on correctness and ecosystem. To beat it on speed, a new tool needs a sharper pruning layer, better scheduler, or lower overhead for common incremental cases.

References:

- https://bazel.build/
- https://bazel.build/remote/caching
- https://bazel.build/versions/8.2.0/remote/rbe
- https://github.com/bazelbuild/remote-apis

## Ninja

Ninja is extremely fast because it is intentionally simple. It expects higher-level tools like CMake or Meson to generate the build files.

Key idea:

```text
be painfully simple
load graph fast
do exactly what the generated file says
```

Ninja is hard to beat for small local incremental C/C++ style builds, but it is not a full monorepo platform with remote execution, package environment management, or cross-language test selection.

References:

- https://ninja-build.org/
- https://ninja-build.org/manual.html

## Nx / Turborepo

Nx and Turborepo are strong for JS/TS monorepos.

Common ideas:

```text
- task graph
- affected project detection
- parallel task execution
- local/remote cache
```

They are practical and easy to adopt, but they usually operate at project/task level rather than deep compiler-level or micro-partition level across all languages.

References:

- https://nx.dev/
- https://turborepo.dev/

## Nix

Nix is not mainly a fast build orchestrator. Its main value is correctness and reproducibility of build environments.

Key idea:

```text
build packages in isolation
avoid undeclared dependencies
make environment declarative
```

For FrostBuild, Nix is best used as the environment/toolchain layer, not as the whole build scheduler.

References:

- https://nixos.org/
- https://nix.dev/manual/nix/2.18/command-ref/new-cli/nix3-build

## Practical baseline to beat

For our goal, the right baseline is not Make or shell scripts. It is:

```text
Bazel with remote cache/remote execution
Buck2 with remote execution
Nx/Turborepo for JS-only workloads
Ninja for local C/C++ inner loop
```

A new tool can be faster than these only by winning in one or more of these areas:

```text
1. less graph evaluation overhead
2. more accurate affected detection
3. more aggressive but safe test selection
4. lower output materialization cost
5. better cache locality scheduling
6. less configuration overhead
7. better compiler persistent workers
8. more precise environment hashing
```
