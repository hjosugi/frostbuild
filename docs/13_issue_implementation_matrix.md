# Open-issue implementation matrix

“Gate” means implementation and a reproducible job exist, but closure needs CI
history, specific hardware, a release tag, or an external service.

| Issues | Implementation / evidence |
|---|---|
| #8, #15, #16, #19, #63 | mmap/versioned postcard graph store, parallel stat/hash cache, append-only journal, torn-tail and kill tests |
| #9, #10, #12, #13 | captured executor, env whitelist, critical-path/fifo scheduler, progress, keep-going, failure summary |
| #18 | structured progress events plus a TTY-only live dashboard with job slots, cache/timing state, critical path, scrollable logs, plain pipe/CI fallback and `--no-tui`; PTY/plain E2E and renderer overhead gate |
| #14, #21, #25, #44 | benchmark runner and PR/nightly artifacts exist. Gate: fresh standalone/daemon 1k/10k measurements and retained CI history after correctness-path changes |
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

Long-running CI-noise records, historical ML replay and a remote executor
experiment are not represented as completed; they require external evidence
rather than more local implementation.
