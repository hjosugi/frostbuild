# Language adapter study

Native C/C++ remains the reference because depfiles expose TU granularity.

- Rust: `cargo metadata` supplies crate graph; Cargo owns rustc incrementality
  and receives the GNU jobserver. Start at package/profile partitions.
- TypeScript: project references are partitions; `tsc --build` owns
  `.tsbuildinfo`, while Frost tracks projects and outputs.
- Go: `go list -deps -json` supplies package edges; Go's build cache stays
  authoritative to avoid duplicate object caching.

Genrule wrapping is the migration path; native rules win on diagnostics and
pruning. Priority is Rust, TypeScript, Go. A Rust adapter must first benchmark
against direct Cargo; no performance claim is made until its JSON is checked in.
