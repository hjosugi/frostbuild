# Platform support decision

Linux is the v1 reference platform: inotify via `notify`, hardlink/copy CAS,
Unix sockets and bubblewrap sandboxing. macOS supports standalone builds and
FSEvents/kqueue through the same watcher abstraction. Native `clonefile` and
Seatbelt are deferred; `--sandbox` reports unavailable when bubblewrap is not
installed. Executor/daemon/store crate boundaries keep platform changes out of
action keys. Windows is a v2/non-goal.

Target-platform support is distinct from host support and shipped:
`[platform.*]` toolchain overlays cross-compile for any device target the
declared toolchain reaches (verified for aarch64-linux via `zig cc`), with
per-platform output trees and cache identities. Genrules and shell tests run
host-side. Host portability items that remain for macOS: BSD `ar` lacks the
`D` flag (use `arflags` or `llvm-ar`) and `--sandbox` stays Linux-only.
