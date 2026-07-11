# Fuzzing

Nightly CI runs the manifest, depfile, journal and graph-store targets. Reproduce
with `cargo fuzz run TARGET`. On a crash, run `cargo fuzz tmin TARGET artifact`,
add the minimized bytes/scenario as a deterministic regression test, fix the
decoder, then retain the case in `fuzz/corpus/TARGET/`. Corrupt persistent state
must safely miss/recompile; it must never synthesize a cache hit.
