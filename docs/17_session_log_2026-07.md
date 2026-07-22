# What was learned on 20 July 2026

A record of one working session on frost, kept because most of what it
produced is knowledge rather than code: seven bugs of one shape, two
performance hypotheses that were wrong, and one benchmark that was not
measuring what it claimed. The code is in the history. This is the part that
would otherwise have to be rediscovered.

This is a historical snapshot of the repository at commit `d72a86d`, not a
description of the current release. Later v0.3.0 results are recorded below so
that measurements from different implementations and workloads are not
silently conflated.

## The bugs were all the same shape

Seven defects were found, and six share a single description: **frost reported
success while leaving a state the source could not reproduce.** That is worse
than being slow, because nothing surfaces it until someone deletes `.frost`.

| What | How it presented |
|---|---|
| A corrupt CAS object was restored unverified | `up to date`, and a binary differing from a correct build by one byte |
| `CPATH` and friends were not in the action key | Two builds against different headers, identical command line, second one cached |
| A file's mode was not in its digest | `chmod -x` on a script: `up to date`, while a clean build failed |
| The shell frost picks was not in the toolchain fingerprint | The one tool frost chooses and did not account for |
| `--profile` accepted any name | A typo built with no profile flags into its own tree, reported success |
| A glob matching no files was accepted | An empty archive, then a link failure pointing nowhere near the glob |
| `--estimator` was accepted and ignored | A flag that did nothing, for as long as it had existed |

Two more were not silent but were failures of the same kind — a feature that
did not do what it said. The daemon could not start from a workspace more than
a few directories deep (its socket lived inside the workspace, and a Unix
socket address is capped near 100 bytes), and the critical-path scheduler
degraded after the first wave because actions unlocked later were prioritised
by a different, cruder key than the one that built the initial queue.

**None of these were found by reading code.** Every one came from trying to
make the cache produce a wrong answer, or from measuring something and not
believing the number. That is the method worth keeping.

`docs/16_action_key_audit.md` exists so this class stops being found one at a
time: it enumerates every input that can change what an action produces,
whether it reaches the key, and the argument for each deliberate exclusion.
Three gaps are named there rather than left to be rediscovered.

## Two performance hypotheses, both wrong

The no-op build went from 285 ms to 176 ms at 10k targets, closing the gap to
Ninja from 6.0x to 3.9x. The order in which that happened matters more than
the number.

| Change | Effect |
|---|---|
| Removing lock contention — the hypothesis the issue was filed on | **-1.3%** |
| Waking one worker per newly runnable action instead of all of them | **-29%** |
| Toolchain fingerprint no longer loading the workspace-wide cache; intra-build stat dedup | -13.5% |
| Pre-stating every file in parallel | **+55%, reverted** |
| Halving action-key allocations (1.39M → 1.18M) | no measurable change |

The first row was wrong because the benchmark graph is a linear chain: there
was no concurrency to serialize, so there was no contention to remove. Looking
at the shape of the graph would have said so in a minute.

The real cause was **50,925 condvar wakeups for 10,000 actions** — every
completion called `notify_all`, so on a chain seven of eight workers woke,
found an empty queue and slept again. Atomic counters in the worker loop found
it in half an hour after two failed guesses.

The fourth row was wrong for an instructive reason. Per-file cost was about
3 µs, of which the stat syscall is roughly 0.5 µs; the rest is allocation.
Parallelising does not reduce allocation, and the change added 20,000 string
clones plus a sort. Measuring the allocator directly (1,387,105 allocations,
165.7 MB for one no-op) then showed that halving the largest source changed
nothing measurable — 210,000 allocations are worth about 5 ms here.

**Conclusion, recorded in #25 and #81: the remaining 3.9x cannot be closed by
accumulation.** The profile is diffuse — syscalls, allocation, hashing, map
operations — with no dominant cost. Getting under Ninja requires not doing the
check at all, which means a daemon holding warm state and a watcher-driven
dirty set. That was always the design in #23; the measurements just removed
the alternative.

## The benchmark was not measuring what it claimed

The published baseline showed frost 2.4-4x slower than Ninja on clean builds.
The harness generated **different commands for different tools**: `printf` for
Ninja and Make, which is a shell builtin, and `cat` for frost, which is an
external binary. frost was spawning two processes per action against their
one.

Matching the commands changed clean-build ratio from **4.06x to 1.61x** on the
same machine and workspace. The instrumented breakdown says the same thing
from the inside: spawn and wait is 7.7 ms per action while frost's own
bookkeeping — hashing outputs, writing the CAS, appending the journal —
totals 2%.

The harness has since been fixed on main. The baselines generated before that
still carry the old conditions and are being regenerated.

Two lessons, both general:

- A comparison is only as good as the thing being held constant. Nobody wrote
  an unfair benchmark on purpose; the two generators simply drifted.
- The internal profile and the external comparison have to agree. They did
  here, which is what made the harness the suspect rather than the engine.

## Where the numbers stood at the end of the session

- no-op, 10k targets: frost ~176 ms, Ninja ~45 ms. **frost loses, 3.9x.**
- clean, 2k targets, matched commands: frost 7.6 s, Ninja 4.7 s. **1.61x.**
- incremental, 10k: frost beats Make by roughly 10x.
- `--daemon` gives **no speedup**: it re-execs the frost binary, so every
  build reloads the graph, journal and hash cache. It was measured at 0.99x
  standalone, and before this session's fixes it was slower than not using it.

None of the above should be quoted without its conditions. The claim frost can
support at the end of this session was "correct, and faster than Make on
incremental work"; "fastest" was not supported by any measurement in this
repository.

## Subsequent v0.3.0 update

The release after this session changed the daemon and the measured no-op path.
The checked 10k standalone graph reached 15.620 ms. A separate one-target,
warm-certificate harness measured 1.711 ms end-to-end through the daemon CLI
and 0.238 ms for the direct daemon socket roundtrip. Those one-target results
supersede the statement above that `--daemon` gave no speedup, but they do not
establish the 10k-target sub-5-ms and greater-than-2x-Ninja gates in #25.

v0.3.0 also shipped the local verified DeltaCDC core and language-neutral
direct-argv command/test targets used by the Rust, Go, Java, Python and
TypeScript harnesses. The remote-cache calibration in #82 and npm/Vite
monorepo completion evidence in #87 intentionally remain open.

## Method notes

Things that repeatedly turned out to matter:

- **Instrument before hypothesising.** Two of two guesses were wrong; two of
  two measurements were right.
- **Interleave A/B runs.** Each needs a cleared cache, so sequential blocks
  compare different machine states. A background research job silently doubled
  every number once before it was noticed.
- **Verify a regression test against the unfixed code.** Several tests here
  were written, passed immediately, and only proved they tested nothing when
  run against the previous engine.
- **Name a test after what it checks.** One was called
  `replacing_the_shell_invalidates_every_genrule` while actually swapping a C
  driver, because swapping the machine's `/bin/sh` is not something a test
  should do. Renamed, with the shell covered directly by a unit test instead.
- **Say what was not established.** Every result document here has a section
  for it. The DeltaCDC harness had to be fixed mid-session because it printed
  oracle totals from a 2% sample next to full-corpus totals, inviting a
  comparison the numbers could not support — the same failure mode as the
  benchmark above, in the research code.
