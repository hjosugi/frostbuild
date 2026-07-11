# Contributing

Issues use Background / Scope / Acceptance Criteria / Dependencies. Feature,
correctness and research forms are provided. Research is complete only when a
checked-in decision memo records evidence, adoption/rejection and follow-up.

Labels: `area:*` names ownership; `kind:feature`, `kind:test`, `kind:infra`, and
`kind:research` name work type; `perf` requires harness evidence; `correctness`
requires a regression scenario. Apply both when a speed optimization changes a
correctness boundary.

Before a PR:

```bash
cargo test --workspace --all-targets --locked
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
python3 -m unittest discover -s tests
```

Performance claims must include `frost-bench` JSON, host metadata, medians and
dispersion. Do not use a one-off stopwatch result. Design changes update
`DESIGN.md`; manifest/storage changes add compatibility and corruption tests.
Use conventional commit subjects. M1 covers correctness/table stakes, M2 local
performance/tooling, and M3 daemon/distribution/v2 research.
