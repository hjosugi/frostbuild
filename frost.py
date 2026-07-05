#!/usr/bin/env python3
"""
FrostBuild POC

A tiny research prototype for a Nix + Bazel + Snowflake-style micro-partition build idea.
It is intentionally small and dependency-free so it can run anywhere with Python 3.11+.

Core features in this POC:
- content hashes, not timestamps
- target dependency graph
- micro-partition affected planning from changed source files
- local action cache + content-addressed artifact store
- parallel execution over an affected DAG
- naive full-rebuild benchmark for comparison
- optional Bazel benchmark if `bazel` is installed and the sample workspace is used
"""
from __future__ import annotations

import argparse
import concurrent.futures
import dataclasses
import hashlib
import json
import os
import pathlib
import shutil
import subprocess
import sys
import time
import tomllib
from collections import defaultdict, deque
from typing import Any

BUILDER_VERSION = "frost-sim-builder-v1"


@dataclasses.dataclass(frozen=True)
class Target:
    name: str
    kind: str
    src: str
    deps: tuple[str, ...]
    out: str
    cost_ms: int = 20


@dataclasses.dataclass
class Plan:
    changed_sources: list[str]
    changed_targets: set[str]
    affected_targets: set[str]
    selected_targets: set[str]
    pruned_targets: set[str]
    reason: str


class Workspace:
    def __init__(self, root: pathlib.Path):
        self.root = root.resolve()
        self.config_path = self.root / "frost.toml"
        if not self.config_path.exists():
            raise FileNotFoundError(f"missing frost.toml at {self.config_path}")
        with self.config_path.open("rb") as f:
            data = tomllib.load(f)
        self.toolchain = data.get("workspace", {}).get("toolchain", "unknown-toolchain")
        self.default_targets = tuple(data.get("workspace", {}).get("default_targets", ["app"]))
        raw_targets = data.get("target", {})
        self.targets: dict[str, Target] = {}
        for name, spec in raw_targets.items():
            self.targets[name] = Target(
                name=name,
                kind=spec.get("kind", "build"),
                src=spec["src"],
                deps=tuple(spec.get("deps", [])),
                out=spec.get("out", f".frost/out/{name}.out"),
                cost_ms=int(spec.get("cost_ms", 20)),
            )
        self._validate()
        self.frost_dir = self.root / ".frost"
        self.out_dir = self.frost_dir / "out"
        self.cas_dir = self.frost_dir / "cas"
        self.action_cache_path = self.frost_dir / "action_cache.json"
        self.state_path = self.frost_dir / "state.json"
        self.metadata_path = self.frost_dir / "metadata.json"

    def _validate(self) -> None:
        for t in self.targets.values():
            if t.name in t.deps:
                raise ValueError(f"target {t.name} depends on itself")
            for dep in t.deps:
                if dep not in self.targets:
                    raise ValueError(f"target {t.name} has unknown dep {dep}")

    def rel(self, p: pathlib.Path) -> str:
        return p.resolve().relative_to(self.root).as_posix()

    def path(self, rel: str) -> pathlib.Path:
        return self.root / rel

    def output_path(self, name: str) -> pathlib.Path:
        return self.root / self.targets[name].out

    def ensure_dirs(self) -> None:
        self.frost_dir.mkdir(exist_ok=True)
        self.out_dir.mkdir(exist_ok=True)
        self.cas_dir.mkdir(exist_ok=True)

    def reverse_deps(self) -> dict[str, set[str]]:
        rev: dict[str, set[str]] = defaultdict(set)
        for name, t in self.targets.items():
            for dep in t.deps:
                rev[dep].add(name)
        return rev

    def target_closure(self, roots: set[str]) -> set[str]:
        seen: set[str] = set()
        def visit(n: str) -> None:
            if n in seen:
                return
            seen.add(n)
            for dep in self.targets[n].deps:
                visit(dep)
        for r in roots:
            visit(r)
        return seen

    def reverse_closure(self, roots: set[str]) -> set[str]:
        rev = self.reverse_deps()
        seen = set(roots)
        q = deque(roots)
        while q:
            n = q.popleft()
            for parent in rev.get(n, set()):
                if parent not in seen:
                    seen.add(parent)
                    q.append(parent)
        return seen


def sha256_bytes(b: bytes) -> str:
    return hashlib.sha256(b).hexdigest()


def sha256_file(path: pathlib.Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def load_json(path: pathlib.Path, default: Any) -> Any:
    if not path.exists():
        return default
    with path.open("r", encoding="utf-8") as f:
        return json.load(f)


def save_json(path: pathlib.Path, data: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(path.suffix + ".tmp")
    with tmp.open("w", encoding="utf-8") as f:
        json.dump(data, f, indent=2, sort_keys=True)
        f.write("\n")
    tmp.replace(path)


def all_source_hashes(ws: Workspace) -> dict[str, str]:
    hashes: dict[str, str] = {}
    for t in ws.targets.values():
        p = ws.path(t.src)
        if p.exists():
            hashes[t.src] = sha256_file(p)
        else:
            hashes[t.src] = "MISSING"
    return hashes


def source_to_targets(ws: Workspace) -> dict[str, set[str]]:
    m: dict[str, set[str]] = defaultdict(set)
    for name, t in ws.targets.items():
        m[t.src].add(name)
    return m


def detect_changed_sources(ws: Workspace, explicit_changed: list[str] | None = None) -> tuple[list[str], set[str], str]:
    src_map = source_to_targets(ws)
    if explicit_changed:
        normalized = []
        for p in explicit_changed:
            rel = pathlib.Path(p).as_posix()
            if rel.startswith(str(ws.root)):
                rel = pathlib.Path(rel).resolve().relative_to(ws.root).as_posix()
            normalized.append(rel)
        changed_targets = set()
        for rel in normalized:
            changed_targets.update(src_map.get(rel, set()))
        return sorted(set(normalized)), changed_targets, "explicit --changed"

    state = load_json(ws.state_path, {})
    previous = state.get("source_hashes")
    current = all_source_hashes(ws)
    if not previous:
        return sorted(current.keys()), set(ws.targets.keys()), "no previous state; cold plan"
    changed = sorted(rel for rel, h in current.items() if previous.get(rel) != h)
    changed_targets = set()
    for rel in changed:
        changed_targets.update(src_map.get(rel, set()))
    return changed, changed_targets, "hash diff from previous state"


def build_plan(ws: Workspace, roots: set[str], kinds: set[str], explicit_changed: list[str] | None, force_all: bool) -> Plan:
    changed_sources, changed_targets, reason = detect_changed_sources(ws, explicit_changed)
    closure = ws.target_closure(roots)
    if force_all or not ws.state_path.exists():
        affected = set(closure)
        reason = "forced/cold full target closure"
    elif not changed_targets:
        affected = set()
    else:
        affected = ws.reverse_closure(changed_targets) & closure

    selected = {n for n in affected if ws.targets[n].kind in kinds}

    # If a selected target needs a dep output that is missing, add that dep recursively.
    # This keeps the POC safe after deleting .frost/out but keeping state/cache.
    changed = True
    while changed:
        changed = False
        for n in list(selected):
            for dep in ws.targets[n].deps:
                if dep not in selected and not ws.output_path(dep).exists():
                    selected.add(dep)
                    changed = True

    pruned = {n for n in closure if ws.targets[n].kind in kinds} - selected
    return Plan(changed_sources, changed_targets, affected, selected, pruned, reason)


def topo_sort(ws: Workspace, subset: set[str]) -> list[str]:
    temp: set[str] = set()
    perm: set[str] = set()
    order: list[str] = []
    def visit(n: str) -> None:
        if n in perm:
            return
        if n in temp:
            raise ValueError(f"cycle detected at {n}")
        temp.add(n)
        for dep in ws.targets[n].deps:
            if dep in subset:
                visit(dep)
        temp.remove(n)
        perm.add(n)
        order.append(n)
    for n in sorted(subset):
        visit(n)
    return order


def action_key(ws: Workspace, t: Target) -> str:
    parts: list[str] = [
        BUILDER_VERSION,
        ws.toolchain,
        t.name,
        t.kind,
        t.src,
        sha256_file(ws.path(t.src)) if ws.path(t.src).exists() else "MISSING_SRC",
    ]
    for dep in sorted(t.deps):
        out = ws.output_path(dep)
        parts.append(dep)
        parts.append(sha256_file(out) if out.exists() else "MISSING_OUT")
    return sha256_bytes("\0".join(parts).encode())


def restore_from_cache(ws: Workspace, out: pathlib.Path, digest: str) -> bool:
    blob = ws.cas_dir / digest
    if not blob.exists():
        return False
    out.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(blob, out)
    return True


def execute_action(ws: Workspace, name: str, action_cache: dict[str, Any], use_cache: bool) -> dict[str, Any]:
    t = ws.targets[name]
    out = ws.output_path(name)
    key = action_key(ws, t)
    cache_entry = action_cache.get(key)
    if use_cache and cache_entry and restore_from_cache(ws, out, cache_entry["digest"]):
        return {"target": name, "status": "cache_hit", "digest": cache_entry["digest"], "key": key}

    # Simulate compiler/test work. This is the cost we try to avoid.
    time.sleep(t.cost_ms / 1000.0)

    src_text = ws.path(t.src).read_text(encoding="utf-8") if ws.path(t.src).exists() else ""
    dep_digests = []
    dep_payload = []
    for dep in sorted(t.deps):
        dep_out = ws.output_path(dep)
        if not dep_out.exists():
            raise RuntimeError(f"{name}: missing dep output {dep_out}")
        digest = sha256_file(dep_out)
        dep_digests.append((dep, digest))
        dep_payload.append(dep_out.read_text(encoding="utf-8", errors="replace")[:256])

    payload = {
        "builder": BUILDER_VERSION,
        "target": name,
        "kind": t.kind,
        "src": t.src,
        "src_hash": sha256_bytes(src_text.encode()),
        "deps": dep_digests,
        "toolchain": ws.toolchain,
        "result": "PASS" if t.kind == "test" else "BUILT",
    }
    output = json.dumps(payload, sort_keys=True, indent=2) + "\n"
    if dep_payload:
        output += "\n# dep preview\n" + "\n".join(dep_payload) + "\n"
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(output, encoding="utf-8")
    digest = sha256_file(out)
    blob = ws.cas_dir / digest
    if not blob.exists():
        shutil.copyfile(out, blob)
    action_cache[key] = {
        "target": name,
        "kind": t.kind,
        "digest": digest,
        "out": t.out,
        "created_at": time.time(),
    }
    return {"target": name, "status": "executed", "digest": digest, "key": key}


def execute_plan(ws: Workspace, selected: set[str], jobs: int, use_cache: bool = True) -> dict[str, Any]:
    ws.ensure_dirs()
    if not selected:
        return {"executed": 0, "cache_hit": 0, "results": [], "duration_s": 0.0}

    action_cache = load_json(ws.action_cache_path, {})
    selected = set(selected)
    deps_in_subset: dict[str, set[str]] = {n: {d for d in ws.targets[n].deps if d in selected} for n in selected}
    reverse: dict[str, set[str]] = defaultdict(set)
    for n, deps in deps_in_subset.items():
        for d in deps:
            reverse[d].add(n)
    ready = deque(sorted(n for n, deps in deps_in_subset.items() if not deps))
    running: dict[concurrent.futures.Future, str] = {}
    completed: set[str] = set()
    results: list[dict[str, Any]] = []
    start = time.perf_counter()

    with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, jobs)) as pool:
        while ready or running:
            while ready and len(running) < max(1, jobs):
                n = ready.popleft()
                fut = pool.submit(execute_action, ws, n, action_cache, use_cache)
                running[fut] = n
            if not running:
                break
            done, _ = concurrent.futures.wait(running.keys(), return_when=concurrent.futures.FIRST_COMPLETED)
            for fut in done:
                n = running.pop(fut)
                res = fut.result()
                results.append(res)
                completed.add(n)
                for parent in sorted(reverse.get(n, set())):
                    deps_in_subset[parent].remove(n)
                    if not deps_in_subset[parent]:
                        ready.append(parent)

    if completed != selected:
        missing = selected - completed
        raise RuntimeError(f"build did not complete; missing={sorted(missing)}")
    save_json(ws.action_cache_path, action_cache)
    save_json(ws.state_path, {"source_hashes": all_source_hashes(ws), "updated_at": time.time()})
    duration = time.perf_counter() - start
    executed = sum(1 for r in results if r["status"] == "executed")
    cache_hit = sum(1 for r in results if r["status"] == "cache_hit")
    return {"executed": executed, "cache_hit": cache_hit, "results": sorted(results, key=lambda x: x["target"]), "duration_s": duration}


def write_metadata(ws: Workspace) -> None:
    rev = ws.reverse_deps()
    meta = []
    for name, t in sorted(ws.targets.items()):
        meta.append({
            "partition_id": name,
            "kind": t.kind,
            "src": t.src,
            "deps": list(t.deps),
            "reverse_deps": sorted(rev.get(name, [])),
            "out": t.out,
            "source_hash": sha256_file(ws.path(t.src)) if ws.path(t.src).exists() else "MISSING",
            "cost_ms": t.cost_ms,
        })
    save_json(ws.metadata_path, {"toolchain": ws.toolchain, "partitions": meta})


def print_plan(ws: Workspace, plan: Plan) -> None:
    total_build = sum(1 for n in ws.target_closure(set(ws.default_targets)) if ws.targets[n].kind == "build")
    print(json.dumps({
        "reason": plan.reason,
        "changed_sources": plan.changed_sources,
        "changed_targets": sorted(plan.changed_targets),
        "affected_targets": sorted(plan.affected_targets),
        "selected_targets": sorted(plan.selected_targets),
        "selected_count": len(plan.selected_targets),
        "pruned_count": len(plan.pruned_targets),
        "total_build_targets_in_default_closure": total_build,
    }, indent=2, sort_keys=True))


def run_build(args: argparse.Namespace, kinds: set[str]) -> None:
    ws = Workspace(pathlib.Path(args.workspace))
    roots = set(args.target or ws.default_targets)
    write_metadata(ws)
    plan = build_plan(ws, roots, kinds=kinds, explicit_changed=args.changed, force_all=args.force)
    if args.dry_run:
        print_plan(ws, plan)
        return
    result = execute_plan(ws, plan.selected_targets, jobs=args.jobs, use_cache=not args.no_cache)
    print(json.dumps({
        "plan_reason": plan.reason,
        "changed_sources": plan.changed_sources,
        "selected_count": len(plan.selected_targets),
        "pruned_count": len(plan.pruned_targets),
        "executed": result["executed"],
        "cache_hit": result["cache_hit"],
        "duration_s": round(result["duration_s"], 4),
        "selected_targets": sorted(plan.selected_targets),
    }, indent=2, sort_keys=True))


def clean(args: argparse.Namespace) -> None:
    ws = Workspace(pathlib.Path(args.workspace))
    if ws.frost_dir.exists():
        shutil.rmtree(ws.frost_dir)
    print(f"removed {ws.frost_dir}")


def touch_source(path: pathlib.Path) -> None:
    old = path.read_text(encoding="utf-8")
    marker = f"\n# bench-change {time.time_ns()}\n"
    path.write_text(old + marker, encoding="utf-8")


def run_optional_bazel(ws: Workspace, change_rel: str) -> dict[str, Any] | None:
    bazel = shutil.which("bazel")
    if not bazel:
        return None
    def timed(cmd: list[str]) -> float:
        s = time.perf_counter()
        subprocess.run(cmd, cwd=ws.root, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        return time.perf_counter() - s
    try:
        timed([bazel, "clean"])
        timed([bazel, "build", "//:app"])
        touch_source(ws.path(change_rel))
        inc = timed([bazel, "build", "//:app"])
        return {"bazel_incremental_s": inc}
    except Exception as e:
        return {"bazel_error": str(e)}


def bench(args: argparse.Namespace) -> None:
    ws = Workspace(pathlib.Path(args.workspace))
    change_rel = args.change
    if not ws.path(change_rel).exists():
        raise FileNotFoundError(f"change file not found: {change_rel}")

    if ws.frost_dir.exists():
        shutil.rmtree(ws.frost_dir)
    write_metadata(ws)

    # Warm full build: creates baseline state and cache.
    full_plan = build_plan(ws, set(ws.default_targets), kinds={"build"}, explicit_changed=None, force_all=True)
    warm = execute_plan(ws, full_plan.selected_targets, jobs=args.jobs, use_cache=True)

    # Single localized change.
    touch_source(ws.path(change_rel))

    micro_plan = build_plan(ws, set(ws.default_targets), kinds={"build"}, explicit_changed=None, force_all=False)
    micro = execute_plan(ws, micro_plan.selected_targets, jobs=args.jobs, use_cache=True)

    # Naive rebuild executes every build target in the default closure, ignoring cache.
    naive_set = {n for n in ws.target_closure(set(ws.default_targets)) if ws.targets[n].kind == "build"}
    naive = execute_plan(ws, naive_set, jobs=args.jobs, use_cache=False)

    speedup = naive["duration_s"] / max(micro["duration_s"], 1e-9)
    report = {
        "workspace": str(ws.root),
        "jobs": args.jobs,
        "changed_file": change_rel,
        "warm_full_build_s": round(warm["duration_s"], 4),
        "micro_partition_incremental_s": round(micro["duration_s"], 4),
        "naive_full_rebuild_s": round(naive["duration_s"], 4),
        "speedup_naive_over_micro": round(speedup, 2),
        "micro_selected_targets": sorted(micro_plan.selected_targets),
        "micro_selected_count": len(micro_plan.selected_targets),
        "micro_pruned_count": len(micro_plan.pruned_targets),
        "naive_target_count": len(naive_set),
        "note": "This is a deterministic simulation benchmark. Use scripts/compare_bazel.sh for real Bazel if installed.",
    }
    if args.with_bazel:
        report["bazel_optional"] = run_optional_bazel(ws, change_rel)
    print(json.dumps(report, indent=2, sort_keys=True))
    save_json(ws.frost_dir / "last_benchmark.json", report)


def gen_sample(args: argparse.Namespace) -> None:
    out = pathlib.Path(args.out).resolve()
    if out.exists() and any(out.iterdir()) and not args.force:
        raise RuntimeError(f"{out} is not empty; use --force")
    if out.exists():
        shutil.rmtree(out)
    (out / "src").mkdir(parents=True)
    (out / "tests").mkdir()
    (out / "tools").mkdir()

    groups = args.groups
    mods = args.modules_per_group
    cost = args.cost_ms
    targets: dict[str, dict[str, Any]] = {}

    for g in range(groups):
        for i in range(mods):
            name = f"pkg{g:02d}_mod{i:02d}"
            src = f"src/{name}.fb"
            deps = []
            if i + 1 < mods:
                deps.append(f"pkg{g:02d}_mod{i+1:02d}")
            if i + 2 < mods and i % 2 == 0:
                deps.append(f"pkg{g:02d}_mod{i+2:02d}")
            (out / src).write_text(
                f"EXPORT {name}\n" + "\n".join(f"IMPORT {d}" for d in deps) + f"\nVALUE {g}-{i}\n",
                encoding="utf-8",
            )
            targets[name] = {"kind": "build", "src": src, "deps": deps, "out": f".frost/out/{name}.out", "cost_ms": cost}

    app_deps = [f"pkg{g:02d}_mod00" for g in range(groups)]
    (out / "src/app.fb").write_text("EXPORT app\n" + "\n".join(f"IMPORT {d}" for d in app_deps) + "\n", encoding="utf-8")
    targets["app"] = {"kind": "build", "src": "src/app.fb", "deps": app_deps, "out": ".frost/out/app.out", "cost_ms": cost}

    for g in range(groups):
        test_name = f"test_pkg{g:02d}"
        src = f"tests/{test_name}.fbtest"
        dep = f"pkg{g:02d}_mod00"
        (out / src).write_text(f"TEST {test_name}\nIMPORT {dep}\n", encoding="utf-8")
        targets[test_name] = {"kind": "test", "src": src, "deps": [dep], "out": f".frost/out/{test_name}.ok", "cost_ms": max(5, cost // 2)}

    # frost.toml
    with (out / "frost.toml").open("w", encoding="utf-8") as f:
        f.write("[workspace]\n")
        f.write('toolchain = "python-simulated-toolchain-v1"\n')
        f.write('default_targets = ["app"]\n\n')
        for name in sorted(targets):
            spec = targets[name]
            f.write(f"[target.{name}]\n")
            f.write(f"kind = {json.dumps(spec['kind'])}\n")
            f.write(f"src = {json.dumps(spec['src'])}\n")
            f.write("deps = [" + ", ".join(json.dumps(d) for d in spec["deps"]) + "]\n")
            f.write(f"out = {json.dumps(spec['out'])}\n")
            f.write(f"cost_ms = {spec['cost_ms']}\n\n")

    # Bazel workspace for optional comparison.
    (out / "MODULE.bazel").write_text('module(name = "frostbuild_sample")\n', encoding="utf-8")
    (out / "tools/BUILD.bazel").write_text('exports_files(["gen.py"])\n', encoding="utf-8")
    (out / "tools/gen.py").write_text(
        "#!/usr/bin/env python3\n"
        "import hashlib, os, sys, time\n"
        "time.sleep(int(os.environ.get('FROST_BAZEL_SLEEP_MS', '20')) / 1000)\n"
        "h = hashlib.sha256()\n"
        "for p in sys.argv[1:]:\n"
        "    with open(p, 'rb') as f: h.update(f.read())\n"
        "print(h.hexdigest())\n",
        encoding="utf-8",
    )
    os.chmod(out / "tools/gen.py", 0o755)
    with (out / "BUILD.bazel").open("w", encoding="utf-8") as f:
        f.write('# Generated Bazel workspace for optional comparison.\n')
        f.write('package(default_visibility = ["//visibility:public"])\n\n')
        for name in sorted(n for n in targets if targets[n]["kind"] == "build"):
            spec = targets[name]
            srcs = [spec["src"]] + [f":{d}" for d in spec["deps"]]
            f.write("genrule(\n")
            f.write(f"    name = \"{name}\",\n")
            f.write("    srcs = [" + ", ".join(json.dumps(s) for s in srcs) + "],\n")
            f.write(f"    outs = [\"bazel_out/{name}.out\"],\n")
            f.write("    tools = [\"//tools:gen.py\"],\n")
            f.write(f"    cmd = \"FROST_BAZEL_SLEEP_MS={cost} python3 $(location //tools:gen.py) $(SRCS) > $@\",\n")
            f.write(")\n\n")

    print(f"generated sample workspace at {out}")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="FrostBuild micro-partition build POC")
    sub = parser.add_subparsers(dest="cmd", required=True)

    p = sub.add_parser("init-sample", help="generate synthetic polyglot-like sample workspace")
    p.add_argument("--out", default="sample")
    p.add_argument("--groups", type=int, default=10)
    p.add_argument("--modules-per-group", type=int, default=8)
    p.add_argument("--cost-ms", type=int, default=30)
    p.add_argument("--force", action="store_true")
    p.set_defaults(func=gen_sample)

    def add_common(p: argparse.ArgumentParser) -> None:
        p.add_argument("--workspace", default="sample")
        p.add_argument("--target", action="append", help="root target; default from frost.toml")
        p.add_argument("--changed", action="append", help="explicit changed source path")
        p.add_argument("--jobs", type=int, default=max(1, (os.cpu_count() or 4) // 2))
        p.add_argument("--force", action="store_true", help="force full target closure")
        p.add_argument("--no-cache", action="store_true")
        p.add_argument("--dry-run", action="store_true")

    p = sub.add_parser("plan", help="show affected build plan")
    add_common(p)
    p.set_defaults(func=lambda args: run_build(args, {"build"}))

    p = sub.add_parser("build", help="build affected targets")
    add_common(p)
    p.set_defaults(func=lambda args: run_build(args, {"build"}))

    p = sub.add_parser("test", help="run affected tests")
    add_common(p)
    p.set_defaults(func=lambda args: run_build(args, {"build", "test"}))

    p = sub.add_parser("clean")
    p.add_argument("--workspace", default="sample")
    p.set_defaults(func=clean)

    p = sub.add_parser("bench", help="compare micro-partition incremental build vs naive full rebuild")
    p.add_argument("--workspace", default="sample")
    p.add_argument("--change", default="src/pkg05_mod07.fb")
    p.add_argument("--jobs", type=int, default=max(1, (os.cpu_count() or 4) // 2))
    p.add_argument("--with-bazel", action="store_true", help="also try Bazel if installed")
    p.set_defaults(func=bench)

    args = parser.parse_args(argv)
    args.func(args)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
