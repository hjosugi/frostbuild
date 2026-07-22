# Remote Cache Study

Conclusion: keep the v1 Rust local CAS/action-cache layout close to Bazel REAPI,
but defer the remote wire protocol to v2.

REAPI compatibility requirements for v1:

- Action keys must be computed from a canonical command descriptor, environment
  whitelist, platform/toolchain closure, input digests, and declared outputs.
- CAS objects should be content addressed by digest and materialized separately
  from action-cache metadata.
- Action-cache entries should point from an action digest to output digests,
  exit code, timing metadata, and discovered dependency metadata.
- Writes must be temp-and-rename so a future remote uploader never observes a
  partial local object.
- Large local blobs are split with Bazel-bit-compatible FastCDC 2020 defaults
  (512 KiB average, 128 KiB minimum, 2 MiB maximum). Chunk files are SHA-256
  addressed and a blob manifest retains ordered lengths; reconstruction writes
  a private staging file and verifies the final BLAKE3+mode digest before one
  rename. This maps directly to REAPI SplitBlob/SpliceBlob without making the
  local action descriptor protobuf-dependent.
- A new version of the same output path may attach a level-19 zstd dictionary
  patch to a residual chunk, using the byte-range-overlapping chunk in the
  previous version as its positional base. Patches smaller than a normal
  level-3 compressed full chunk are retained. Restore order is exact blob,
  exact chunk, verified delta chunk, then miss/rebuild; base choice can only
  affect cost because patch, reconstructed chunk and final blob are verified.

Current gap:

- Frost uses canonical BLAKE3 descriptors instead of REAPI protobuf messages.
- The local action result is a binary journal record, not REAPI ActionResult.
- There is no ByteStream service, batch update API, compressor negotiation, or
  remote execution platform properties.
- The local positional delta path is not a claim of remote-cache speed: CPU vs
  bandwidth calibration and protocol negotiation still require external
  measurements.

Decision:

- Adopt the REAPI separation of ActionCache and ContentAddressableStorage in
  the local data model.
- Defer wire compatibility until the Rust engine freezes its v1 action schema.
- File a v2 requirement before remote work: freeze a protobuf-compatible action
  descriptor and output tree format so local action keys can be translated
  without rebuilding the cache.
