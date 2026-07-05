# Zig implementation plan

## Is Zig a good choice?

Yes, Zig is a reasonable choice for the engine core if the goal is a small, fast, predictable binary.

Good parts:

```text
- no GC
- fast startup
- explicit memory management
- easy single-binary distribution
- good C interop
- good control over IO and hashing
- good fit for a scheduler/cache/indexer engine
```

Risks:

```text
- smaller ecosystem than Rust/Go
- fewer mature libraries for remote execution protocols
- team hiring may be harder
- async/runtime ecosystem is less standard than Go/Rust
```

## Recommended split

Do not implement everything in Zig from day 1.

```text
Zig:
  core engine
  graph planner
  hash/CAS code
  local scheduler
  file scanner
  CLI binary

Python/TypeScript/Rust plugins:
  language analysis
  import graph extraction
  test discovery
  framework-specific integrations

Nix:
  toolchain environment layer

REAPI/gRPC sidecar:
  remote execution protocol support if Zig gRPC becomes painful
```

## Why not pure Zig immediately?

The hard part is not raw speed. The hard part is correctness:

```text
- dependency inference
- dynamic deps
- test selection safety
- rule ecosystem
- remote cache compatibility
- sandbox behavior
```

First prove the algorithm in Python. Then port hot paths.

## Data structures

Core structs:

```zig
const Partition = struct {
    id: []const u8,
    kind: Kind,
    src: []const u8,
    deps: []const []const u8,
    reverse_deps: []const []const u8,
    output: []const u8,
    source_hash: [32]u8,
    toolchain_hash: [32]u8,
    last_duration_ms: u64,
};

const ActionKey = struct {
    digest: [32]u8,
};

const ActionResult = struct {
    action_key: ActionKey,
    output_digest: [32]u8,
    status: Status,
};
```

## Zig MVP milestones

```text
M1:
  parse simple frost.json
  build graph
  detect changed source hashes
  produce affected plan

M2:
  local CAS
  action cache
  parallel scheduler

M3:
  process execution sandbox wrapper
  depfile reader
  JSON event log

M4:
  Nix environment hash integration
  remote cache client

M5:
  REAPI remote execution client
```

## CLI shape

```bash
frost init
frost plan //app
frost build //app --jobs 16
frost test //app --affected
frost bench --baseline bazel
frost explain //app --why-built
frost query 'changed(src/pkg05_mod07) -> affected_tests'
```

## Skeleton

See `zig_skeleton/src/main.zig`.

It is intentionally only a skeleton because this environment does not include the Zig compiler. The runnable POC is `frost.py`.
