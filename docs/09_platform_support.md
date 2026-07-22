# Host and target platform support

Linux remains the reference host: inotify via `notify`, hardlink/copy CAS,
Unix-domain daemon sockets and bubblewrap sandboxing. macOS uses the native
`notify` backend and the same Unix-domain daemon transport. Native `clonefile`
and Seatbelt are not implemented; `--sandbox` reports unavailable when
bubblewrap is absent.

Windows is now an experimental host instead of a compile-time non-goal. The
daemon publishes an ephemeral loopback TCP address in the workspace's
`.frost/frostd.endpoint`, test success stamps are executor-owned rather than
POSIX shell snippets, shell actions use `cmd.exe /C`, and cancellation uses
`taskkill /T` for the complete child process tree. The workspace is
cross-checked for `x86_64-pc-windows-gnu`. CI defines native macOS and Windows
gates for all-target compilation, library/binary unit tests, daemon
status/shutdown, and a real command-target build followed by a cached no-op.
Those gates passed on the published v0.3.0 commit, including the Windows CLI's
dedicated-stack entry point. Tagged releases publish host-built macOS and
Windows archives alongside static Linux.

The Windows C/C++ adapter still emits GCC/Clang-style depfile and link flags;
it is suitable for a GNU-like or explicitly wrapped toolchain, not yet a
native MSVC `cl.exe`/`link.exe` contract. Windows genrule command text is
`cmd.exe` syntax, while direct `kind = "command"` and `kind = "test"` argv are
the preferred portable language adapters. Linux-only bubblewrap remains the
only strict sandbox backend.

Target-platform support is distinct from host support:
`[platform.*]` toolchain overlays cross-compile for any device target the
declared toolchain reaches (verified for aarch64-linux via `zig cc`), with
per-platform output trees and cache identities. Genrules and shell tests run
host-side. BSD `ar` on macOS lacks GNU `ar`'s `D` flag, so use an explicit
`arflags` value or `llvm-ar`; `--sandbox` stays Linux-only.
