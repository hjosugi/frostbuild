# FrostBuild design

This document is the normative architecture. Changes to action keys, paths,
storage formats, dependency semantics, or benchmark claims must update it.

## 1. Goals

FrostBuild is a production-oriented, local-first build engine for large
monorepos. Correct incremental results are non-negotiable. Performance targets
are a warm daemon no-op below 5 ms and 10k-action scheduling below 30 ms. These
are targets, not universal promises; results require checked-in harness JSON.

## 2. Micro-partition pruning

The scheduling unit is an action, normally one translation unit. Target
selection takes only the transitive producer closure; unchanged actions stop at
the constructive trace. Test selection uses the same graph. Corrupt catalog or
journal state falls back to a larger safe closure.

## 3. Architecture

### 3.1 Components

1. `frost` is a thin CLI for build, test, plan, clean, graph, compdb and explain.
2. `frostd` owns a workspace Unix socket, recursive watcher and build mutex.
3. Manifests compile to a versioned postcard graph store loaded through mmap.
4. The planner constructs the requested action closure.
5. The scheduler uses dependency counts and estimated critical-path priority.
6. The executor captures each action's output and passes a whitelisted env.
7. The local CAS stores immutable BLAKE3 objects and materializes by link/copy.
8. Toolchain binaries and sysroot identity participate in action keys.
9. GCC/Clang depfiles add discovered header dependencies after successful runs.

Generated files are order-only inputs until a depfile proves they are content
inputs. Journal keys and output trees are profile-separated.

### 3.2 Incremental path

The hash cache validates mtime nanoseconds, size and inode, then hashes misses
in parallel. The daemon maintains a dirty set with the platform watcher. A
matching key plus intact output is a hit; missing output restores from CAS.
Byte-identical outputs stop propagation. The binary append-only journal flushes
each completed action and ignores a torn crash tail.

### 3.3 Correctness and paths

Action keys include canonical argv, cwd, whitelisted environment, toolchain
closure and content inputs. Unknown manifest fields and non-UTF-8 manifest paths
are explicit errors. File symlinks follow their targets for stat/hash; directory
symlinks are not traversed in package discovery. Paths with spaces work as argv
and depfile entries; genrule shell substitutions require author quoting.
Failed/interrupted actions have partial outputs removed and are never journaled.
Sandbox mode hides the workspace and remounts declared inputs, source/include
roots and output directories only.

## 4. Crates

- `frostbuild-core`: manifest, graph, depfile, graph store, journal, hash cache, CAS
- `frostbuild-store`: stable storage facade
- `frostbuild-exec`: scheduler, executor, sandbox and cancellation
- `frostbuild-daemon`: protocol, watcher and `frostd`
- `frostbuild-cli`: `frost`
- `frostbuild-bench`: Rust entry point; `frost-bench` owns JSON runs

## 5. Profiles and compatibility

Debug is the default. Profile flags, outputs and journal entries are isolated.
Binary formats carry magic/version headers; mismatch safely recompiles. Single
root manifests remain compatible; `[workspace]` enables `//package:target`.

## 6. Benchmark discipline

Performance claims use `./frost-bench`, medians, warmups, dispersion and machine
metadata. PR smoke tests detect large regressions; nightly retains full JSON.

## 7. Non-goals and v2

Windows and C++20 modules are not v1 targets. macOS is best-effort without the
Linux bubblewrap sandbox. Remote cache/execution are v2; v1 preserves canonical
descriptors, immutable CAS objects, Merkle-ready paths and hermetic boundaries.
