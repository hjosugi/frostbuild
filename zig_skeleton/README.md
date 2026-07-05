# Zig skeleton

This is a non-runnable design skeleton for a future Zig implementation.

The runnable POC is the Python prototype at `../frost.py`.

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
