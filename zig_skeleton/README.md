# Zig skeleton (historical)

The authoritative implementation is the Rust workspace in `../crates/`. This
directory is retained only as historical design input.

This is a non-runnable design skeleton for a future Zig implementation.

The Python prototype at `../frost.py` is a comparison/reference model only.

Suggested future command:

```bash
zig build run -- plan --workspace ../sample
```

Why Zig:

```text
- low startup overhead
- no garbage collector pause
- simple static binary distribution
- explicit memory and IO control
```

Recommended approach:

```text
1. keep Python prototype for algorithm iteration
2. define a stable frost.json/frost.toml model
3. port planner + cache + scheduler to Zig
4. keep language analyzers as plugins at first
```
