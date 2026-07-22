# Polyglot win matrix

“Fastest build tool” is not one number. Frost earns the claim one language and
artifact contract at a time, with failures and missing competitors kept in the
report.

## Required comparison contract

Every language report must record:

1. the same generated or checked-in source graph;
2. the same compiler/runtime version and equivalent output-affecting flags;
3. semantic artifact validation, not merely a successful exit;
4. clean, warmed no-op, one-leaf change, shared/hot dependency change and
   local-CAS restoration where applicable;
5. round-robin frontend order, reversed or rotated per iteration;
6. all samples, medians, machine/load/governor/turbo metadata and tool versions;
7. configuration files, bytes and lines;
8. missing tools as `skipped` and execution failures as `failed`.

A frontend wins a workload only when its median is lower and its validated
artifact contract is equal. A language is not “won” because Frost wraps the
incumbent and skips an unchanged project boundary cheaply.

## Product gates

| Gate | Required evidence |
|---|---|
| Correctness | exact required artifact set; content/semantic digest; stale-output deletion; missing/corrupt CAS fallback |
| Incrementality | leaf and shared-dependency scopes are observed, not inferred from the manifest |
| Performance | alternating/rotating order, at least median-of-7 for product claims and median-of-15 for close results |
| Usability | concise native or generated rule, actionable failure, completion, `fzf` selection, clean/profile/platform behavior |
| Ecosystem | dependencies, tests, packaging and IDE metadata have an explicit owner; unsupported areas are named |
| Platforms | Linux plus CI evidence for Windows and macOS; cross/device configurations remain cache-isolated |

## Current state

| Language / boundary | Correctness | Performance evidence | Status |
|---|---|---|---|
| C/C++ | Native translation units, depfiles, libraries/tests, real compilers | 1k Frost/Bazel checked graph; 10k Frost/Ninja no-op | measured workloads, not universal |
| Java classes | Same 100 class names and bytes across Frost/Gradle/Maven | Frost batch beats Gradle and Maven clean/change/no-op in checked reports | simple module contract won |
| Java JAR | Same 100 class entry names and bytes; stale inner-class and `frost init` → executable-JAR/run/debug E2E | Frost beats Gradle `jar` and Maven `package` on all three checked scenarios | simple JAR contract and zero-manifest onboarding won |
| Rust binary crate | Same 101-source crate; executable stdout validated after every sample | Frost beats Cargo clean/change/no-op in median-of-7; close change result confirmed median-of-15 | dependency-free crate contract won |
| Go binary package | Same 101-source package; execution and `go version -m` metadata validated after every sample | native Frost beats `go build` change/no-op in median-of-7 and clean in focused median-of-15 | dependency-free package contract won |
| TypeScript project / solution | Byte-identical single-project JS; byte-identical 416-file eight-reference solution; exact Node execution after every sample; native compiler declaration closure declared | Frost beats native `tsc` no-op 16.7x for one project and 2.05x for the solution, but loses clean/change; project action fan-out helps without overcoming shared-process `tsc --build` | no-op boundaries won; compiler and ecosystem gates open |
| Python pure wheel | Same 101 source names/bytes; Name/Version/tag; fully verified `RECORD`; exact execution after every sample | Frost beats `uv build` clean/no-op/change by 15.35x/111.86x/37.25x and `python -m build` by more | minimal pure-wheel contract won; PEP 517 ecosystem/pytest open |
| Gradle/Maven/npm project boundary | Direct-argv, fingerprinted artifact boundary | fast Frost boundary no-op is not inner-language speed | integration only |

## Micro-partition policy

Partitioning is adaptive, not ideological:

```text
estimated avoided compiler work
        > partition startup + scheduling + publication overhead
    => use the smaller partition
otherwise
    => batch
```

The Java result demonstrates both sides. One-source partitions reduced the
changed-source median from 508.577 ms to 398.477 ms (1.28x faster), but one
fresh `javac` VM per source increased clean build time from 513.421 ms to
21,200.950 ms (41.3x slower). So micro-partitioning worked for the leaf edit
and decisively failed as a fixed clean-build policy. Until a language has a
persistent worker, ABI graph or cheap compiler invocation, Frost defaults to
module/batch boundaries and uses fine partitions only where measured reuse
repays their cost.

## Execution order

The next comparative suites should land in this order:

1. Rust multi-crate workspace: Cargo metadata, dependencies, features, build
   scripts and test artifacts after the direct single-crate win.
2. Go multi-package DAG: `go list -deps`, imports/modules, constraints, cgo,
   embed and equal binary/test artifacts after the direct one-package win.
3. TypeScript project references, watch/hot reload, a separate equal-output
   esbuild bundle contract and one monorepo task runner, after the direct
   single-project no-op win.
4. Python PEP 517 metadata/dependencies/extensions and affected pytest
   selection, after the minimal pure-wheel win.
5. Windows/macOS replicas of the C++, Java, Rust and Python gates.

This matrix is the stop condition for a broad claim. Until those rows carry
checked reports, Frost should say exactly which workload it won.
