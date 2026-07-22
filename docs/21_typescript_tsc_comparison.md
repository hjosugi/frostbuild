# TypeScript / native `tsc` comparison

This comparison asks one narrow question: for a dependency-free TypeScript
project whose full type checking, incremental emit and JavaScript files are
owned by native TypeScript 7, what does adding Frost around that compiler cost
or save?

It is deliberately not an `esbuild` comparison. `esbuild` transpiles and
bundles but does not implement TypeScript type checking, so timing it against a
strict `tsc` build would compare different products. Bundling is a separate
artifact contract.

## Equal contract

Both frontends compile the same `src/main.ts` plus 100 imported modules with
TypeScript 7.0.2 and these output-affecting settings:

- `strict`, `noEmitOnError` and incremental compilation enabled;
- `NodeNext` module and resolution modes, target ES2022;
- no declarations or source maps;
- an isolated output directory and `.tsbuildinfo` for each frontend.

After every timed build, the harness checks the exact set and bytes of all 101
JavaScript files, executes `main.js` with Node 26.5.0, and requires exit 0,
empty stderr and exact expected stdout. Tool order reverses on every measured
iteration. The checked outputs are byte-identical.

Native `tsc` also reads the standard-library declarations beside its executable.
The harness copies that compiler closure into each generated workspace. Frost
fingerprints the native compiler binary and declares all 108 `lib*.d.ts` files
as action inputs; it does not pretend that hashing only `tsc` is hermetic.

## Parallel checker sweep

TypeScript 7 exposes `--checkers`. A fixed worker count was not guessed: clean
builds were run in forward and reverse checker-count order, seven samples per
direction. The combined median therefore contains 14 samples per cell:

| Checkers | Frost clean | direct `tsc` clean |
|---:|---:|---:|
| 1 | 269.498 ms | 239.276 ms |
| 2 | 258.100 ms | **224.907 ms** |
| 4 | **256.764 ms** | 226.514 ms |
| 8 | 276.041 ms | 238.382 ms |
| 16 | 273.724 ms | 241.959 ms |

Parallel checking helps, but oversubscription does not. On this 8-logical-CPU
host, Frost's best checked point is four checkers and direct `tsc`'s is two.
The optimized comparison uses those frontend-specific values; `--checkers`
still supplies one shared value when controlled identical parallelism is the
question.

## Optimized median-of-7 result

Report: `bench/baselines/2026-07-21-E14-typescript-optimized.json`

| Scenario | Frost + native `tsc` | direct native `tsc` | Result |
|---|---:|---:|---|
| clean | 259.409 ms | **228.080 ms** | direct `tsc` 1.14x faster |
| warmed no-op | **2.468 ms** | 41.318 ms | Frost 16.7x faster |
| one module changed | 49.391 ms | **42.467 ms** | direct `tsc` 1.16x faster |

Frost wins only the no-op in this single-project boundary. Once the compiler
must run, Frost's graph/action/input verification is additional work around the
same `tsc`; it does not make TypeScript's internal semantic graph faster. This
is useful for a larger mixed-language graph where an unchanged TypeScript
project can be pruned before process startup, but it is not a general
TypeScript speed win.

The controlled shared-eight-checker report is
`bench/baselines/2026-07-21-E14-typescript.json`. The forward and reverse sweep
reports are named `2026-07-21-E14-typescript-checkers-N.json` and
`...-checkers-N-reverse.json`.

## Incremental output correctness found by the benchmark

The first implementation exposed a real incompatibility. Frost removed every
declared output before rerunning an action, while incremental `tsc` deliberately
emits only affected files. The build then lost unchanged JavaScript outputs.

Command targets now have an explicit `preserve_outputs = true` mode. It is in
the action key, keeps prior outputs during a successful incremental rerun and
still content-verifies every declared file before journaling. `.tsbuildinfo` is
also declared as an output, so failure cleanup cannot leave invisible compiler
state claiming that deleted files are current. An E2E regression test covers a
compiler that updates only one member of a retained output set.

## Reproduce

```bash
npm install --prefix /tmp/frost-ts-tools --no-audit --no-fund \
  typescript@7.0.2

TSC_BIN=/tmp/frost-ts-tools/node_modules/@typescript/typescript-linux-x64/lib/tsc \
NODE_BIN="$(command -v node)" \
./frost-bench typescript --size 100 --iterations 7 --jobs 8 \
  --checkers 1 --frost-checkers 4 --tsc-checkers 2 \
  --out bench/baselines/<date>-<host>-typescript-optimized.json
```

## Independent project-reference parallelism

The `typescript-projects` suite covers eight independent composite projects,
25 modules plus an entrypoint in each (208 TypeScript sources). Direct `tsc`
owns one solution with eight references. Frost owns eight independent command
actions. Both emit the same 208 JavaScript and 208 declaration files; all 416
names and bytes match, and all eight entrypoints run after every timed build.

A worker-budget preflight found `-j8 × 1 checker` best for Frost: distributing
eight workers across project processes was faster than giving multiple checker
workers to fewer processes. Direct `tsc --build` was best at two checkers.

Checked median-of-7 report:
`bench/baselines/2026-07-21-E14-typescript-projects.json`.

| Scenario | Frost, 8×1 workers | `tsc --build`, 2 checkers | Result |
|---|---:|---:|---|
| clean | 940.313 ms | **656.893 ms** | `tsc --build` 1.43x faster |
| warmed no-op | **3.200 ms** | 6.556 ms | Frost 2.05x faster |
| one project changed | 50.792 ms | **44.386 ms** | `tsc --build` 1.14x faster |

Project action parallelism is real—it cut Frost's exploratory clean median by
more than half between one and eight outer jobs—but process/toolchain startup
still costs more than TypeScript 7 sharing one native solution process. The
replacement report began at a recorded 2.94 one-minute load average; another
host is still required before treating the exact ratios as universal.

Generic `frost watch` and success-only process restart are now shipped and E2E
checked. The remaining TypeScript gates are a persistent compiler/watch-process
comparison, diagnostics and source-map/debugger UX, an equal-output bundling
comparison, browser-protocol HMR, and Nx/Turborepo/Bazel task boundaries. The
honest claim remains “TypeScript no-op boundaries won,” not “TypeScript won.”
