# Java: Frost, Gradle and Maven

This comparison separates three questions that are easy to blur:

1. How fast is the frontend when nothing changed?
2. How much compiler work does one source change trigger?
3. How much ecosystem functionality and configuration convenience is included?

Frost is measured twice. `frost-unit` declares one `javac` action per source,
while `frost-batch` passes the full source set to one `javac` action. Both use
the same JDK and produce the same required `.class` set as Gradle and Maven.

## Reproduce

```bash
cargo build --release --locked -p frostbuild-cli --bin frost

FROST_BIN=target/release/frost \
GRADLE_BIN=/path/to/gradle \
./frost-bench java \
  --tools frost-unit,frost-batch,gradle,maven \
  --size 100 --iterations 3 --jobs 8 \
  --out bench/baselines/2026-07-20-E14-java.json
```

Missing tools are recorded as `skipped`; a tool that starts and then fails is
recorded as `failed`. Neither is silently removed. Each successful run verifies
all 100 expected class names and records a digest over their bytes. The checked
run produced byte-identical class sets across all four frontends.

`clean` removes produced classes but retains the JDK/tool installation, Gradle
daemon and Maven local repository. Gradle uses its daemon and configuration
cache, with the build cache disabled so a clean sample invokes the compiler.
`noop` is warmed before sampling. `incremental_leaf` appends a comment to one
source before every sample. Frontends run round-robin, with the order reversed
on every measured iteration, so no frontend always receives the coolest or
warmest machine state. The main table reports median-of-3 wall time; a focused
Frost/Gradle run below reports median-of-15.

## Checked local result

The checked JSON is
[`bench/baselines/2026-07-20-E14-java.json`](../bench/baselines/2026-07-20-E14-java.json).
It records the exact versions, samples, host, load average, CPU governor and
turbo state. This run began at load average 0.66 / 1.78 / 2.29 on an 8-core E14
with the performance governor and turbo enabled. Results are milliseconds:

| Frontend | Clean | No-op | One source changed | Declared compiler scope |
|---|---:|---:|---:|---|
| Frost, 100 source units | 21,200.950 | 1.941 | 398.477 | 1 source |
| Frost, one batch | 513.421 | 1.936 | 508.577 | 100 sources |
| Gradle 8.14.4 | 603.643 | 548.287 | 565.518 | Gradle incremental analysis |
| Maven 3.9.16 | 1,717.647 | 1,322.760 | 1,723.897 | compiler-plugin analysis |

These numbers support narrow conclusions only:

- Frost's validated no-op boundary is dramatically cheaper on this project.
- Micro-partitioning helps the one-source change: source-unit Frost was 1.28x
  faster than Frost batch, 1.42x faster than Gradle and 4.33x faster than Maven.
- Naively starting 100 `javac` JVMs is disastrous for clean builds. Frost unit
  was 41.3x slower than Frost batch. Micro-partitions need a persistent compiler
  worker or adaptive batching, not unbounded process granularity.
- After making small-output hashing adaptive and publishing distinct CAS
  objects concurrently, Frost batch was 1.18x faster than Gradle clean, 1.11x
  faster for one changed source and 283x faster no-op. It was 3.35x faster than
  Maven clean.
- A focused alternating-order median-of-15 run in
  [`2026-07-20-E14-java-frost-gradle-15.json`](../bench/baselines/2026-07-20-E14-java-frost-gradle-15.json)
  confirms the direction: Frost was 1.13x faster clean, 1.13x faster after one
  changed source and 269x faster no-op.
- This is evidence for this generated 100-source, dependency-free project. It
  is not evidence for large real-world dependency graphs, annotation
  processors, Kotlin, tests or publishing.

The source-unit manifest was 31,466 bytes / 705 lines; the batch manifest was
9,650 bytes / 12 long lines, Gradle was 332 bytes / 15 lines across three files,
and Maven was 789 bytes / 21 lines. `frost init` now removes the hand-authoring
cost for the practical plain-Java batch/JAR case. It deliberately does not
generate the 100-action source-unit layout: that layout loses clean builds
badly and still needs automatic adaptive partitioning before it is a usable
default.

## JAR packaging result

The class-only comparison deliberately isolates compilation. A second checked
report measures a usable module artifact: Frost's one-action `javac` plus
built-in deterministic JAR packer, Gradle's `jar` task and Maven's `package`
lifecycle:

```bash
FROST_BIN=target/release/frost \
GRADLE_BIN=/path/to/gradle \
./frost-bench java \
  --tools frost-jar,gradle-jar,maven-jar \
  --size 100 --iterations 7 --jobs 8 \
  --out bench/baselines/2026-07-20-E14-java-jar.json
```

The checked alternating-order median-of-7 report is
[`2026-07-20-E14-java-jar.json`](../bench/baselines/2026-07-20-E14-java-jar.json).
It verifies the same 100 class binary names and byte-identical class payloads
inside every JAR. ZIP metadata and compression bytes are excluded from that
equivalence digest.

| Frontend | Clean JAR | No-op | One source changed | Configuration |
|---|---:|---:|---:|---:|
| Frost 0.2.0 | 879.884 | 3.646 | 867.721 | 528 bytes / 16 lines |
| Gradle 9.3.1 | 985.571 | 899.332 | 932.748 | 332 bytes / 15 lines |
| Maven 3.9.16 | 3,414.296 | 2,634.849 | 3,441.713 | 789 bytes / 21 lines |

On this contract Frost was 1.12x faster than Gradle clean, 1.07x faster after
one changed source and 247x faster no-op. Against Maven it was 3.88x, 3.97x and
723x faster respectively. The Frost manifest is one file and roughly the same
line count as Gradle; it uses one source glob, one stable JAR output and no
shell wrapper. This still does not cover dependency resolution, annotation
processors, tests, publishing or IDE import.

## Feature and usability comparison

| Area | Frost command adapter | Gradle | Maven |
|---|---|---|---|
| No-op startup | Whole-closure certificate; very small local frontend | Long-lived daemon, VFS and up-to-date checks | New Maven process in this comparison |
| Java incrementality | Explicit partition graph; no Java ABI analysis yet | Incremental `JavaCompile` and compile avoidance | Maven Compiler Plugin owns analysis |
| Clean compilation | Direct `javac`; batching is author-controlled | Mature compiler daemon/task integration | Mature compiler lifecycle/plugin |
| Dependency management | None; delegate to an ecosystem tool | Rich repositories/configurations/plugins | Rich repositories/scopes/plugins |
| Tests and packaging | Direct-argv multi-step actions plus built-in deterministic compressed JAR packing; no native Java test model | Native Java test suites, jars and publishing | Standard lifecycle, Surefire, jars and publishing |
| Local output cache | Built-in digest-verified CAS per declared file | Build cache, opt-in by CLI/config | No equivalent core default in this run |
| Remote cache/execution | Protocol work remains v2 | Local/remote build cache; not a general REAPI executor | Extensions/plugins; not core REAPI execution |
| Multi-platform matrix | `--platform` / `--all-platforms` | Attributes, variants and toolchains | Profiles and toolchains |
| Hermetic environment | Cleared environment plus explicit `env`/`pass_env`; optional Linux sandbox | Task/plugin modelling; build-defined | Plugin/build-defined |
| IDE/ecosystem | No Java-specific model yet | Excellent IDE/plugin ecosystem | Excellent IDE/plugin ecosystem |
| Initial configuration | `frost init` generates a plain-Java batch plus deterministic JAR; explicit artifacts remain verbose at unit granularity | Concise convention + programmable DSL | Concise standard layout + declarative POM |

The scaffold is intentionally narrower than Gradle or Maven. It detects a
package-qualified main class, emits an executable JAR when present, and is
covered by a real `javac` → `java -jar` → `frost run` E2E. Auto-detection
refuses Gradle/Maven project markers instead of pretending their dependency,
plugin or test semantics do not exist; `--language java` is an explicit escape
hatch for users who really want a direct batch build.

Gradle documents that incremental builds derive from declared task
inputs/outputs and that its daemon retains in-memory state and filesystem
watching. Its build and configuration caches are separate choices:

- <https://docs.gradle.org/current/userguide/incremental_build.html>
- <https://docs.gradle.org/current/userguide/gradle_daemon.html>
- <https://docs.gradle.org/current/userguide/command_line_interface.html>

Maven's project conventions, toolchains and reproducibility guidance remain
valuable even where its invocation overhead is higher:

- <https://maven.apache.org/guides/mini/guide-configuring-maven.html>
- <https://maven.apache.org/guides/mini/guide-reproducible-builds.html>

## Product decision

Today the most practical Java integration is adaptive and hybrid:

```text
Frost target/module graph
|-- one direct javac batch for small/simple modules (fastest in this test)
|-- small javac/codegen partitions only where incremental pruning repays JVM startup
|-- Gradle project boundary for rich Java/Kotlin/plugin builds
`-- Maven reactor/module boundary for convention-heavy projects
```

Frost can skip an unchanged Gradle/Maven boundary cheaply and combine it with
C++, Rust, Go and TypeScript targets. Inside a Java boundary, Gradle or Maven
remains authoritative until Frost has native Java dependency discovery,
persistent javac workers, automatic adaptive batching, test discovery and IDE
metadata.
