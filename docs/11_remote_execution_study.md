# Remote execution study

Decision: remote execution is v2, but v1 action/CAS data stays translatable to
the Remote Execution API without invalidating local cache.

REAPI `Command` maps to canonical argv, environment, cwd, outputs and platform
properties. `Action` adds an input-root digest and timeout. Frost's sorted flat
path/digest map must become a Merkle `Directory`; directory/symlink outputs need
explicit representation.

V1 requirements now enforced are immutable digest objects separated from action
results, canonical relative paths/env, toolchain identity, declared outputs,
atomic publication, crash-safe records and sandbox diagnostics. RE requires
hermetic toolchains and sandbox correctness.

A v2 experiment should execute one synthetic graph on Buildbarn and BuildGrid
and compare digest translation, platform properties, missing-blob recovery and
output trees. Frost may retain local BLAKE3 while producing the wire digest
(normally SHA-256) in the adapter.
