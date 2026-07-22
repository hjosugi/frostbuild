# Open-issue implementation matrix

“Gate” means implementation and a reproducible job exist, but closure needs CI
history, specific hardware, a release tag, or an external service.

| Issues | Implementation / evidence |
|---|---|
| #8, #15, #16, #19, #63 | mmap/versioned postcard graph store, parallel stat/hash cache, append-only journal, torn-tail and kill tests |
| #9, #10, #12, #13 | captured executor, env whitelist, critical-path/fifo scheduler, progress, keep-going, failure summary |
| #18 | structured progress events plus a TTY-only live dashboard with job slots, cache/timing state, critical path, scrollable logs, plain pipe/CI fallback and `--no-tui`; PTY/plain E2E and renderer overhead gate |
| #14, #21, #25, #44 | benchmark runner and PR/nightly artifacts exist. The daemon now validates eligible certificates in-process instead of spawning a second CLI. A checked rotating median-of-31 one-target run measured 0.238 ms socket roundtrip and 1.711 ms end-to-end daemon CLI versus 2.043 ms standalone, meeting #25's local 5-ms target. Retained published CI/hardware history remains the open gate |
| #17, #20, #50, #56 | escaped depfiles, dynamic headers, early cutoff, path policy/E2E, generated order-only edges |
| #22, #23, #24 | framed/versioned socket, recursive watcher, lifecycle commands, serialized builds, correctness-preserving engine validation/fallback |
| #26, #28, #29 | immutable BLAKE3 CAS with GC, cached compiler closure, bubblewrap sandbox and undeclared-header E2E |
| #31, #32, #36, #37 | action pruning stats, affected tests, safe predictive flag, scheduler/estimator flags; ML reduction gated on replay miss rate |
| #34, #35 | REAPI cache decision and Rust Ninja subset importer |
| #39–#43 | DESIGN/manifest/README/contribution docs, six crates, toolchain pin, forms and milestone assignment |
| #45, #46 | four fuzz targets, proptest, nightly fuzz, cargo-deny, Dependabot and SHA-pinned actions |
| #47 | Completed by v0.1.0: SemVer/CHANGELOG/install docs and tag-triggered musl binaries/checksums |
| #48, #49, #51 | process-group cancellation, partial-output cleanup, explain/trace and compilation database |
| #52–#54 | platform/language decisions and authoritative Rust/reference Python/historical Zig roles |
| #57–#61 | profiles, C++, hermetic globs, multi-package labels and cached test runner |
| #62 | deterministic double-run mode and macro/output diagnosis E2E |
| #64 | RE execution gaps, Merkle/output-tree requirements and executor experiment plan. Gate: external v2 experiment |
| #81 | The mutex-contention hypothesis was falsified by counters; the real linear-chain costs were thundering-herd wakeups and per-action checking. Targeted wakeups plus a checksummed whole-closure no-op certificate now avoid that work, with corruption/change fallback tests and a checked 10k standalone median 15.620 ms versus Ninja 42.419 ms. The issue's standalone acceptance evidence is met locally; published-branch evidence remains |
| #82 | Local DeltaCDC core: previous-version positional overlap selector, zstd level 19 + long-distance mode, retain-only-if-smaller-than-level-3-full policy, patch/chunk/blob triple verification and exact→chunk→delta→miss fallback. One-bit residual reconstruction and corrupt/missing/wrong-base failure injection pass. A checked 64 MiB interleaved A/B measured verified delta restore at 40.196 ms parallel vs 75.647 ms serial and retained seven patches in 518 bytes; remote CPU/bandwidth and protocol evidence remain open |
| #83 | Bazel-bit-compatible FastCDC 2020 boundaries/defaults, >2 MiB threshold, SHA-256 chunk store, versioned blob manifests, verified chunk splice fallback and persistent `cache stats` reuse ratios. Local E2E restores without the whole blob, retains >75% of chunks after one byte changes, and bounded parallel chunk work measured 1.41x faster cold publication / 1.89x faster restore in the checked 64 MiB A/B; published CI evidence remains |
| #84 | `UnverifiedBytes`/`VerifiedBlob` publication boundary plus a required CI job injecting bit flips, missing/wrong/truncated/single chunks, ordering changes, parameter mismatch and the Bazel #29544 final-path scenario. Delta-patch-specific injections remain coupled to #82 |
| #87 | direct-argv `command` and `test` targets, named/fingerprinted tools, explicit environment, platform overrides, real Rust/Go/Java/Python/Node E2E and native TypeScript single/solution plus standards-compliant Python wheel harnesses remove the generic execution blocker. `preserve_outputs`, generic `watch`, target-aware `run`/`dev`, GDB/LLDB/jdb/Node/pdb launch and non-overwriting VS Code configs ship. Keep open for npm/PEP 517 discovery, dynamic output trees, comparative affected-pytest evidence, persistent compiler/browser HMR and richer DAP/source-map UX |

Current cache-v2 issues remain open until published and external evidence is
complete: #82 still needs remote CPU/bandwidth/protocol calibration; #83 needs
published CI/release evidence; #84 retains any future transport-specific fault
matrix; #64 requires an external REAPI executor experiment. Local command
adapters or no-op benchmarks do not imply those gates.

Long-running CI-noise records, historical ML replay and a remote executor
experiment are not represented as completed; they require external evidence
rather than more local implementation.
