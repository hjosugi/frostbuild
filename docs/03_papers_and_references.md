# Papers and references

## Build Systems à la Carte

Authors:

```text
Andrey Mokhov, Neil Mitchell, Simon Peyton Jones
```

Why it matters:

```text
- gives a framework for comparing build systems
- separates scheduler, rebuilder, dependency model, store model
- shows build systems as recombinable components
- discusses dynamic dependencies and cloud builds
```

Use for FrostBuild:

```text
Use this as the theory base for combining Nix-like correctness, Bazel-like cloud builds, and dynamic dependency support.
```

References:

- https://www.microsoft.com/en-us/research/wp-content/uploads/2018/03/build-systems.pdf
- https://dl.acm.org/doi/10.1145/3236774
- https://simon.peytonjones.org/assets/pdfs/build-systems-jfp.pdf

## Pluto: A Sound and Optimal Incremental Build System with Dynamic Dependencies

Why it matters:

```text
- focuses on sound and optimal incremental rebuilds
- supports dynamic dependencies
- records build summaries with requirements and products
```

Use for FrostBuild:

```text
Use dynamic dependency recording and validation to keep micro-partition pruning safe.
```

Reference:

- https://www.pl.informatik.uni-mainz.de/files/2019/04/pluto-incremental-build.pdf

## Regression test selection research

Key idea:

```text
Run only tests affected by code changes while trying not to miss failures.
```

Important approaches:

```text
static RTS:
  infer dependencies by static program analysis

dynamic RTS:
  collect dependencies from previous test executions

hybrid RTS:
  combine both
```

Use for FrostBuild:

```text
Build-system-aware multi-language RTS is directly relevant because FrostBuild targets polyglot monorepos.
```

References:

- Build System Aware Multi-language Regression Test Selection in CI: https://mediatum.ub.tum.de/doc/1656311/1656311.pdf
- Static RTS study: https://www.cs.cornell.edu/~legunsen/pubs/LegunsenETAL16StaticRTSStudy.pdf
- RTS in CI: https://mir.cs.illinois.edu/marinov/publications/ShiETAL19RTSinCI.pdf
- STARTS static RTS demo: https://mir.cs.illinois.edu/awshi2/publications/ASEDEMO2017.pdf
- Hybrid RTS ASE 2024: https://zbchen.github.io/files/ase2024.pdf

## Predictive test selection

Meta has published work around predictive test selection.

Use for FrostBuild:

```text
Use probabilistic test selection only as an optional fast mode.
Safe mode should stay conservative.
```

Reference:

- https://research.facebook.com/publications/predictive-test-selection/

## Bazel remote execution and cache

Why it matters:

```text
- action cache
- content-addressable storage
- remote execution API
- distributed test/build actions
```

Use for FrostBuild:

```text
Do not invent a remote execution protocol first. Start with Bazel Remote Execution API compatibility if possible.
```

References:

- https://bazel.build/remote/caching
- https://bazel.build/versions/8.2.0/remote/rbe
- https://github.com/bazelbuild/remote-apis
- https://buf.build/bazel/remote-apis/docs/main:build.bazel.remote.execution.v2

## Buck2

Why it matters:

```text
- strong modern baseline
- Rust engine
- single incremental dependency graph
- dynamic dependencies
- up to 2x faster than Buck1 in practice
- deferred materialization
```

Use for FrostBuild:

```text
Buck2 is the most important design competitor.
```

References:

- https://github.com/facebook/buck2
- https://engineering.fb.com/2023/04/06/open-source/buck2-open-source-large-scale-build-system/
- https://buck2.build/docs/about/why/
- https://buck2.build/docs/users/advanced/deferred_materialization/
- https://github.com/facebookincubator/buck2-change-detector

## Nix and reproducible builds

Why it matters:

```text
- exact environment matters for correct cache keys
- build isolation avoids undeclared dependencies
- reproducibility is a stronger property than just caching
```

References:

- https://nixos.org/
- https://nix.dev/manual/nix/2.25/advanced-topics/diff-hook
- https://reproducible-builds.org/docs/definition/

## Snowflake micro-partitions

Why it matters:

```text
- metadata-driven pruning
- skip irrelevant data before expensive work
```

Use for FrostBuild:

```text
Treat build/test/artifact units as partitions with metadata. Use the metadata catalog to prune work before scheduling.
```

Reference:

- https://docs.snowflake.com/en/user-guide/tables-clustering-micropartitions

## Build performance measurement

Why it matters:

```text
You cannot improve what you do not measure.
```

Track:

```text
- graph loading time
- planning time
- action count
- executed action count
- cache hit count
- cache lookup latency
- artifact download size
- output materialization time
- critical path length
- worker queue time
- test selected/skipped ratio
- false negative rate for test selection
```

References:

- https://bazel.build/advanced/performance/build-performance-breakdown
- https://bazel.build/advanced/performance/build-performance-metrics
