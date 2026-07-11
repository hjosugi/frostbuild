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

Current gap:

- Frost uses canonical BLAKE3 descriptors instead of REAPI protobuf messages.
- The local action result is a binary journal record, not REAPI ActionResult.
- There is no ByteStream service, batch update API, compressor negotiation, or
  remote execution platform properties.

Decision:

- Adopt the REAPI separation of ActionCache and ContentAddressableStorage in
  the local data model.
- Defer wire compatibility until the Rust engine freezes its v1 action schema.
- File a v2 requirement before remote work: freeze a protobuf-compatible action
  descriptor and output tree format so local action keys can be translated
  without rebuilding the cache.
