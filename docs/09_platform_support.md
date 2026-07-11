# Platform support decision

Linux is the v1 reference platform: inotify via `notify`, hardlink/copy CAS,
Unix sockets and bubblewrap sandboxing. macOS supports standalone builds and
FSEvents/kqueue through the same watcher abstraction. Native `clonefile` and
Seatbelt are deferred; `--sandbox` reports unavailable when bubblewrap is not
installed. Executor/daemon/store crate boundaries keep platform changes out of
action keys. Windows is a v2/non-goal.
