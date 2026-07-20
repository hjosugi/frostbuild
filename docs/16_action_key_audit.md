# What the action key covers, and what it does not

frost skips an action when its key matches the journal and its outputs are
intact. Everything that can change what the action produces must therefore
reach the key, or frost will report a build as current that a clean tree
cannot reproduce. That failure is worse than a slow build: it is silent, and
it survives until someone deletes `.frost`.

This is the enumeration. Each row states whether the input reaches the key,
and the reasoning is written down so that a future change to any of them is a
decision rather than an accident. Rows marked **out** are deliberate gaps with
a stated argument; rows marked **gap** are known holes with no defence.

| Input | Reaches the key | How, or why not |
|---|---|---|
| Command line (argv) | yes | Verbatim in the canonical payload. Profile and platform flags arrive this way. |
| Declared input contents | yes | Content digest per path. |
| Discovered input contents | yes | Depfile paths join the key after the first run. |
| **Input file mode** | yes | Mixed into the content digest. `chmod -x` on a script changes no bytes; without this frost reported "up to date" where a clean build failed. |
| Compiler identity | yes | The toolchain fingerprint hashes the resolved `cc`, `cxx` and `ar` binaries, so a PATH switch or a package upgrade invalidates. |
| `CPATH`, `C_INCLUDE_PATH`, `CPLUS_INCLUDE_PATH`, `LIBRARY_PATH`, `SDKROOT`, `MACOSX_DEPLOYMENT_TARGET`, `SystemRoot` | yes | These choose which headers and libraries a compiler finds with an identical command line. |
| Working directory | yes | Recorded relative to the workspace root. |
| Locale | n/a | Forced to `LC_ALL=C` and `LANG=C` for every action, so it cannot vary. |
| Output contents | yes, separately | Checked against the journal after the key matches; a modified output re-runs or restores. |
| `PATH` | **out** | Its effect on the compiler is already covered by hashing the resolved drivers. Keying on it would rebuild everything whenever a shell, direnv or CI step exports a different one. |
| `HOME`, `TMPDIR`, `TMP`, `TEMP` | **out** | Scratch locations. An action whose output depends on where its scratch space lives is not hermetic, which is what `--sandbox` and `--check-determinism` exist to surface. |
| Order-only inputs | **out** | By construction: generated headers must exist before a compile, but only the ones actually included should invalidate it, and the depfile names those. |
| `--sandbox` | **out** | A checking mode, not a build input. It can only remove access to files an action never declared, so a build that differs under it was already unsound. |
| **Interpreters a genrule invokes** (`/bin/sh`, `python3`, ...) | **gap** | frost cannot know which tools a shell command reaches. Declaring the script is not declaring its interpreter. Same class as any undeclared input; `--sandbox` narrows it, nothing closes it. |
| **umask** | **gap** | Affects the permissions of created outputs. Only the executable bit is captured, via the output digest. |
| **Filesystems with coarse mtime** | **gap** | The stat check is (mtime_ns, size ^ mode, inode). Where a filesystem reports whole seconds, a same-size rewrite inside one second can be missed. Not observed on ext4; no cheap defence beyond hashing everything. |

## The rule this table encodes

An input belongs in the key when changing it changes what the action
produces. An input stays out only when one of these holds, and the row must
say which:

1. Something already in the key covers it (PATH, via the driver hashes).
2. Depending on it is already a bug the build model does not admit
   (scratch directories, sandbox visibility).
3. Including it would invalidate constantly for no correctness gain, and the
   risk is documented (there are currently no rows resting only on this).

"It is inconvenient" is not on the list.

## History

Every row marked yes for an environment variable, and the file-mode row, were
gaps found by trying to break the cache rather than by reading the code. The
CPATH case was reproduced end to end: two headers of the same name reachable
only through the environment, an identical command line, and frost handing
back the binary built against the other one. Tests now cover each; they are
named in `crates/frostbuild-cli/tests/e2e.rs` and each one fails against the
engine that preceded its fix.

The three gaps above have no test, because a test would assert the wrong
behaviour. They are written down instead so that the next person to look does
not have to rediscover them, and so that closing one is a deliberate change.
