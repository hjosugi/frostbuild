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
import contextlib
import dataclasses
import heapq
import hashlib
import json
import os
import pathlib
import platform
import re
import shutil
import shlex
import socket
import stat
import struct
import subprocess
import sys
import tempfile
import threading
import time
import tomllib
from collections import defaultdict, deque
from typing import Any

BUILDER_VERSION = "frost-sim-builder-v1"
ACTION_KEY_VERSION = "frost-action-key-v2"
METADATA_CATALOG_SCHEMA = "frost-metadata-catalog-v1"
GRAPH_STORE_SCHEMA = "frost-graph-store-v1"
STAT_CACHE_SCHEMA = "frost-stat-cache-v1"
BUILD_JOURNAL_SCHEMA = "frost-build-journal-v1"
DYNAMIC_DEPS_SCHEMA = "frost-dynamic-deps-v1"
DAEMON_PROTOCOL_VERSION = "frost-daemon-json-v1"
ACTION_ENV_KEYS = ("FROSTBUILD_FLAGS",)
TOKEN_BYTE = b"+"
DEFAULT_CAS_MAX_BYTES = 512 * 1024 * 1024
JOURNAL_LOCK = threading.Lock()


@dataclasses.dataclass(frozen=True)
class Target:
    name: str
    kind: str
    src: str
    deps: tuple[str, ...]
    out: str
    cost_ms: int = 20
    depfile: str | None = None
    declared_inputs: tuple[str, ...] = ()
    sandbox: bool = True
    command: tuple[str, ...] = ()


@dataclasses.dataclass
class Plan:
    changed_sources: list[str]
    changed_targets: set[str]
    affected_targets: set[str]
    selected_targets: set[str]
    pruned_targets: set[str]
    reason: str


@dataclasses.dataclass(frozen=True)
class ActionDescriptor:
    argv: tuple[str, ...]
    cwd: str
    env: tuple[tuple[str, str], ...]
    inputs: tuple[tuple[str, str], ...]


@dataclasses.dataclass
class JobserverLease:
    mode: str
    effective_jobs: int
    read_fd: int | None = None
    write_fd: int | None = None
    borrowed_tokens: int = 0
    fifo_file: Any | None = None
    server_read_fd: int | None = None
    server_write_fd: int | None = None

    @staticmethod
    def disabled(requested_jobs: int) -> "JobserverLease":
        return JobserverLease(mode="disabled", effective_jobs=max(1, requested_jobs))

    @staticmethod
    def from_environment(requested_jobs: int, env: dict[str, str] | None = None) -> "JobserverLease":
        requested_jobs = max(1, requested_jobs)
        env = dict(os.environ if env is None else env)
        auth = parse_make_jobserver_auth(env.get("MAKEFLAGS", ""))
        if auth is None:
            return JobserverLease.disabled(requested_jobs)
        if auth.startswith("fifo:"):
            return JobserverLease._from_fifo(requested_jobs, auth.removeprefix("fifo:"))
        try:
            read_s, write_s = auth.split(",", 1)
            read_fd = int(read_s)
            write_fd = int(write_s)
        except ValueError:
            return JobserverLease.disabled(requested_jobs)
        borrowed = borrow_pipe_tokens(read_fd, max(0, requested_jobs - 1))
        return JobserverLease(
            mode="client",
            effective_jobs=max(1, min(requested_jobs, 1 + borrowed)),
            read_fd=read_fd,
            write_fd=write_fd,
            borrowed_tokens=borrowed,
        )

    @staticmethod
    def _from_fifo(requested_jobs: int, fifo_path: str) -> "JobserverLease":
        try:
            fifo = open(fifo_path, "r+b", buffering=0)
        except OSError:
            return JobserverLease.disabled(requested_jobs)
        borrowed = borrow_pipe_tokens(fifo.fileno(), max(0, requested_jobs - 1))
        return JobserverLease(
            mode="client",
            effective_jobs=max(1, min(requested_jobs, 1 + borrowed)),
            write_fd=fifo.fileno(),
            borrowed_tokens=borrowed,
            fifo_file=fifo,
        )

    @staticmethod
    def server(requested_jobs: int) -> "JobserverLease":
        requested_jobs = max(1, requested_jobs)
        read_fd, write_fd = os.pipe()
        os.set_inheritable(read_fd, True)
        os.set_inheritable(write_fd, True)
        if requested_jobs > 1:
            os.write(write_fd, TOKEN_BYTE * (requested_jobs - 1))
        return JobserverLease(
            mode="server",
            effective_jobs=requested_jobs,
            server_read_fd=read_fd,
            server_write_fd=write_fd,
        )

    def child_env(self, base_env: dict[str, str] | None = None) -> dict[str, str]:
        env = dict(os.environ if base_env is None else base_env)
        if self.mode == "server" and self.server_read_fd is not None and self.server_write_fd is not None:
            flags = env.get("MAKEFLAGS", "")
            auth = f"--jobserver-auth={self.server_read_fd},{self.server_write_fd} -j"
            env["MAKEFLAGS"] = f"{flags} {auth}".strip()
        return env

    def close(self) -> None:
        if self.borrowed_tokens and self.write_fd is not None:
            with contextlib.suppress(OSError):
                os.write(self.write_fd, TOKEN_BYTE * self.borrowed_tokens)
        if self.fifo_file is not None:
            self.fifo_file.close()
        for fd in (self.server_read_fd, self.server_write_fd):
            if fd is not None:
                with contextlib.suppress(OSError):
                    os.close(fd)

    def __enter__(self) -> "JobserverLease":
        return self

    def __exit__(self, exc_type: object, exc: object, tb: object) -> None:
        self.close()


class Workspace:
    def __init__(self, root: pathlib.Path):
        self.root = root.resolve()
        self.config_path = self.root / "frost.toml"
        if not self.config_path.exists():
            raise FileNotFoundError(f"missing frost.toml at {self.config_path}")
        with self.config_path.open("rb") as f:
            data = tomllib.load(f)
        workspace_spec = data.get("workspace", {})
        toolchain_spec = data.get("toolchain", {})
        self.toolchain = workspace_spec.get("toolchain", "unknown-toolchain")
        self.toolchain_compiler_path = toolchain_spec.get("compiler_path", "")
        self.toolchain_sysroot = toolchain_spec.get("sysroot", "")
        self.env_whitelist = tuple(workspace_spec.get("env_whitelist", ACTION_ENV_KEYS))
        self.default_targets = tuple(workspace_spec.get("default_targets", ["app"]))
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
                depfile=spec.get("depfile"),
                declared_inputs=tuple(spec.get("inputs", [])),
                sandbox=bool(spec.get("sandbox", True)),
                command=tuple(spec.get("command", [])),
            )
        self._validate()
        self.frost_dir = self.root / ".frost"
        self.out_dir = self.frost_dir / "out"
        self.cas_dir = self.frost_dir / "cas"
        self.action_cache_path = self.frost_dir / "action_cache.json"
        self.state_path = self.frost_dir / "state.json"
        self.metadata_path = self.frost_dir / "metadata.json"
        self.graph_store_path = self.frost_dir / "graph.json"
        self.stat_cache_path = self.frost_dir / "stat_cache.json"
        self.journal_path = self.frost_dir / "journal.ndjson"
        self.journal_compact_path = self.frost_dir / "journal.json"
        self.dynamic_deps_path = self.frost_dir / "dynamic_deps.json"
        self.daemon_socket_path = self.frost_dir / "daemon.sock"
        self.daemon_pid_path = self.frost_dir / "daemon.pid"
        self.daemon_dirty_path = self.frost_dir / "dirty.json"
        self.lock_path = self.frost_dir / "workspace.lock"

    def _validate(self) -> None:
        for target in self.default_targets:
            if target not in self.targets:
                raise ValueError(f"default target {target} is not defined")
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


def semantic_source_text(text: str) -> str:
    return "\n".join(line for line in text.splitlines() if not line.lstrip().startswith("#")) + "\n"


def load_json(path: pathlib.Path, default: Any) -> Any:
    if not path.exists():
        return default
    with path.open("r", encoding="utf-8") as f:
        return json.load(f)


def save_json(path: pathlib.Path, data: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp_name = tempfile.mkstemp(prefix=f".{path.name}.", suffix=".tmp", dir=path.parent)
    tmp = pathlib.Path(tmp_name)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            json.dump(data, f, indent=2, sort_keys=True)
            f.write("\n")
        tmp.replace(path)
    finally:
        with contextlib.suppress(FileNotFoundError):
            tmp.unlink()


def path_stat_record(path: pathlib.Path) -> dict[str, int] | None:
    try:
        st = path.stat()
    except FileNotFoundError:
        return None
    return {
        "mtime_ns": st.st_mtime_ns,
        "size": st.st_size,
        "inode": st.st_ino,
        "mode": stat.S_IFMT(st.st_mode),
    }


def same_stat_record(a: dict[str, Any] | None, b: dict[str, Any] | None) -> bool:
    return bool(a and b) and all(a.get(key) == b.get(key) for key in ("mtime_ns", "size", "inode", "mode"))


def hash_files_cached(ws: Workspace, rel_paths: set[str], *, workers: int | None = None) -> dict[str, str]:
    cache = load_json(ws.stat_cache_path, {})
    if cache.get("schema") != STAT_CACHE_SCHEMA:
        cache = {"schema": STAT_CACHE_SCHEMA, "files": {}}
    file_cache: dict[str, Any] = cache.setdefault("files", {})
    results: dict[str, str] = {}
    to_hash: list[tuple[str, pathlib.Path, dict[str, int] | None]] = []

    for rel in sorted(rel_paths):
        path = ws.path(rel)
        current_stat = path_stat_record(path)
        cached = file_cache.get(rel, {})
        if current_stat is None:
            results[rel] = "MISSING"
            file_cache[rel] = {"stat": None, "hash": "MISSING"}
        elif same_stat_record(cached.get("stat"), current_stat) and cached.get("hash"):
            results[rel] = cached["hash"]
        else:
            to_hash.append((rel, path, current_stat))

    def compute(item: tuple[str, pathlib.Path, dict[str, int] | None]) -> tuple[str, str, dict[str, int] | None]:
        rel, path, current_stat = item
        return rel, sha256_file(path), current_stat

    if to_hash:
        max_workers = workers or min(32, max(1, os.cpu_count() or 1))
        with concurrent.futures.ThreadPoolExecutor(max_workers=max_workers) as pool:
            for rel, digest, current_stat in pool.map(compute, to_hash):
                results[rel] = digest
                file_cache[rel] = {"stat": current_stat, "hash": digest, "updated_at": time.time()}

    save_json(ws.stat_cache_path, cache)
    return results


def all_source_hashes(ws: Workspace) -> dict[str, str]:
    sources = {t.src for t in ws.targets.values()}
    for target in ws.targets.values():
        sources.update(target.declared_inputs)
    dynamic = load_dynamic_deps(ws)
    for deps in dynamic.values():
        sources.update(deps)
    return hash_files_cached(ws, sources)


def source_to_targets(ws: Workspace) -> dict[str, set[str]]:
    m: dict[str, set[str]] = defaultdict(set)
    for name, t in ws.targets.items():
        m[t.src].add(name)
        for rel in t.declared_inputs:
            m[rel].add(name)
    for name, deps in load_dynamic_deps(ws).items():
        for rel in deps:
            m[rel].add(name)
    return m


def workspace_manifest_fingerprint(ws: Workspace) -> str:
    # One canonical JSON encoding is materially faster than hundreds of
    # thousands of tiny hashlib.update calls on a 10k-target graph.
    payload = {
        "toolchain": ws.toolchain,
        "compiler": getattr(ws, "toolchain_compiler_path", ""),
        "sysroot": getattr(ws, "toolchain_sysroot", ""),
        "env": sorted(getattr(ws, "env_whitelist", ACTION_ENV_KEYS)),
        "defaults": sorted(ws.default_targets),
        "targets": [
            (
                name,
                t.kind,
                t.src,
                sorted(t.deps),
                t.out,
                t.cost_ms,
                t.depfile or "",
                t.sandbox,
                sorted(t.declared_inputs),
                list(t.command),
            )
            for name, t in sorted(ws.targets.items())
        ],
    }
    encoded = json.dumps(payload, sort_keys=True, separators=(",", ":")).encode()
    return hashlib.sha256(encoded).hexdigest()


def build_metadata_catalog(ws: Workspace, *, include_source_hashes: bool = False) -> dict[str, Any]:
    rev = ws.reverse_deps()
    partitions = []
    source_to_partitions: dict[str, list[str]] = defaultdict(list)
    source_to_targets_index: dict[str, list[str]] = defaultdict(list)
    partition_to_target: dict[str, str] = {}
    target_to_partition: dict[str, str] = {}

    for name, t in sorted(ws.targets.items()):
        partition_id = name
        partition = {
            "partition_id": partition_id,
            "target": name,
            "kind": t.kind,
            "src": t.src,
            "deps": list(t.deps),
            "reverse_deps": sorted(rev.get(name, [])),
            "out": t.out,
            "cost_ms": t.cost_ms,
            "depfile": t.depfile,
            "declared_inputs": list(t.declared_inputs),
            "sandbox": t.sandbox,
            "command": list(t.command),
        }
        if include_source_hashes:
            partition["source_hash"] = sha256_file(ws.path(t.src)) if ws.path(t.src).exists() else "MISSING"
        partitions.append(partition)
        source_to_partitions[t.src].append(partition_id)
        source_to_targets_index[t.src].append(name)
        partition_to_target[partition_id] = name
        target_to_partition[name] = partition_id

    return {
        "schema": METADATA_CATALOG_SCHEMA,
        "toolchain": ws.toolchain,
        "manifest_fingerprint": workspace_manifest_fingerprint(ws),
        "partitions": partitions,
        "indexes": {
            "source_to_partitions": {k: sorted(v) for k, v in sorted(source_to_partitions.items())},
            "source_to_targets": {k: sorted(v) for k, v in sorted(source_to_targets_index.items())},
            "partition_to_target": dict(sorted(partition_to_target.items())),
            "target_to_partition": dict(sorted(target_to_partition.items())),
            "reverse_deps": {k: sorted(v) for k, v in sorted(rev.items())},
        },
    }


def metadata_catalog_is_stale(ws: Workspace, catalog: dict[str, Any]) -> bool:
    return (
        catalog.get("schema") != METADATA_CATALOG_SCHEMA
        or catalog.get("manifest_fingerprint") != workspace_manifest_fingerprint(ws)
    )


def catalog_changed_targets(catalog: dict[str, Any], sources: list[str] | tuple[str, ...] | set[str]) -> set[str]:
    source_index = catalog.get("indexes", {}).get("source_to_targets", {})
    targets: set[str] = set()
    for source in sources:
        targets.update(source_index.get(source, []))
    return targets


def catalog_reverse_closure(catalog: dict[str, Any], roots: set[str]) -> set[str]:
    rev = catalog.get("indexes", {}).get("reverse_deps", {})
    seen = set(roots)
    q = deque(roots)
    while q:
        n = q.popleft()
        for parent in rev.get(n, []):
            if parent not in seen:
                seen.add(parent)
                q.append(parent)
    return seen


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


def build_plan(
    ws: Workspace,
    roots: set[str],
    kinds: set[str],
    explicit_changed: list[str] | None,
    force_all: bool,
    *,
    use_catalog: bool = True,
) -> Plan:
    changed_sources, changed_targets, reason = detect_changed_sources(ws, explicit_changed)
    closure = ws.target_closure(roots)
    catalog = load_json(ws.metadata_path, {}) if use_catalog else {}
    catalog_ok = bool(catalog) and not metadata_catalog_is_stale(ws, catalog)
    if catalog_ok and changed_sources:
        changed_targets = catalog_changed_targets(catalog, changed_sources)
    if force_all or not ws.state_path.exists():
        affected = set(closure)
        reason = "forced/cold full target closure"
    elif not changed_targets:
        affected = set()
    else:
        affected = (catalog_reverse_closure(catalog, changed_targets) if catalog_ok else ws.reverse_closure(changed_targets)) & closure

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


def test_roots(ws: Workspace, requested: list[str] | None) -> set[str]:
    if requested:
        return set(requested)
    tests = {name for name, target in ws.targets.items() if target.kind == "test"}
    return tests or set(ws.default_targets)


def apply_predictive_test_selection(ws: Workspace, plan: Plan, *, enabled: bool) -> dict[str, Any]:
    if not enabled:
        return {"enabled": False, "selected": sorted(plan.selected_targets), "dropped": []}
    tests = {name for name in plan.selected_targets if ws.targets[name].kind == "test"}
    non_tests = plan.selected_targets - tests
    changed = plan.changed_targets
    scored: list[tuple[int, str]] = []
    for name in tests:
        deps = set(ws.targets[name].deps)
        score = 0
        if deps & changed:
            score += 100
        if deps & plan.affected_targets:
            score += 20
        if name in changed:
            score += 100
        scored.append((score, name))
    selected_tests = {name for score, name in scored if score > 0}
    if not selected_tests and tests:
        selected_tests = tests
    dropped = sorted(tests - selected_tests)
    plan.selected_targets = set(non_tests | selected_tests)
    return {
        "enabled": True,
        "selected": sorted(plan.selected_targets),
        "dropped": dropped,
        "model": "journal-distance-heuristic-int8-placeholder",
    }


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


def parse_make_jobserver_auth(makeflags: str) -> str | None:
    if not makeflags:
        return None
    try:
        parts = shlex.split(makeflags)
    except ValueError:
        parts = makeflags.split()
    for part in parts:
        if part.startswith("--jobserver-auth="):
            return part.split("=", 1)[1]
        if part.startswith("--jobserver-fds="):
            return part.split("=", 1)[1]
    return None


def borrow_pipe_tokens(read_fd: int, limit: int) -> int:
    if limit <= 0:
        return 0
    borrowed = 0
    was_blocking = True
    try:
        was_blocking = os.get_blocking(read_fd)
        os.set_blocking(read_fd, False)
    except OSError:
        return 0
    try:
        while borrowed < limit:
            try:
                token = os.read(read_fd, 1)
            except BlockingIOError:
                break
            except OSError:
                break
            if not token:
                break
            borrowed += 1
    finally:
        with contextlib.suppress(OSError):
            os.set_blocking(read_fd, was_blocking)
    return borrowed


def unescape_dep_token(token: str) -> str:
    return token.replace("\\ ", " ").replace("\\#", "#").replace("\\:", ":").replace("\\\\", "\\")


def parse_depfile_text(text: str) -> dict[str, list[str]]:
    logical = text.replace("\\\r\n", " ").replace("\\\n", " ")
    result: dict[str, list[str]] = {}
    for raw_line in logical.splitlines():
        line = raw_line.strip()
        if not line or ":" not in line:
            continue
        left, right = line.split(":", 1)
        outputs = [unescape_dep_token(token) for token in re.findall(r"(?:\\.|[^\s])+", left)]
        inputs = [unescape_dep_token(token) for token in re.findall(r"(?:\\.|[^\s])+", right)]
        for output in outputs:
            result[output] = inputs
    return result


def parse_depfile(path: pathlib.Path) -> dict[str, list[str]]:
    return parse_depfile_text(path.read_text(encoding="utf-8", errors="replace"))


def load_dynamic_deps(ws: Workspace) -> dict[str, list[str]]:
    data = load_json(ws.dynamic_deps_path, {})
    if data.get("schema") != DYNAMIC_DEPS_SCHEMA:
        return {}
    return {str(k): list(v) for k, v in data.get("targets", {}).items()}


def save_dynamic_deps(ws: Workspace, deps: dict[str, list[str]]) -> None:
    save_json(ws.dynamic_deps_path, {"schema": DYNAMIC_DEPS_SCHEMA, "targets": {k: sorted(set(v)) for k, v in sorted(deps.items())}})


def record_target_depfile(ws: Workspace, target: Target) -> list[str]:
    if not target.depfile:
        return []
    depfile_path = ws.path(target.depfile)
    if not depfile_path.exists():
        return []
    parsed = parse_depfile(depfile_path)
    output_rel = target.out
    deps = parsed.get(output_rel)
    if deps is None:
        deps = next(iter(parsed.values()), [])
    normalized = []
    for dep in deps:
        dep_path = pathlib.Path(dep)
        rel = dep_path.as_posix()
        if dep_path.is_absolute():
            try:
                rel = dep_path.resolve().relative_to(ws.root).as_posix()
            except ValueError:
                continue
        if rel != target.out:
            normalized.append(rel)
    dynamic = load_dynamic_deps(ws)
    dynamic[target.name] = sorted(set(normalized))
    save_dynamic_deps(ws, dynamic)
    return dynamic[target.name]


def compile_graph_store(ws: Workspace) -> dict[str, Any]:
    graph = {
        "schema": GRAPH_STORE_SCHEMA,
        "manifest_fingerprint": workspace_manifest_fingerprint(ws),
        "compiled_at": time.time(),
        "targets": {
            name: {
                "kind": t.kind,
                "src": t.src,
                "deps": list(t.deps),
                "out": t.out,
                "cost_ms": t.cost_ms,
            }
            for name, t in sorted(ws.targets.items())
        },
        "default_targets": list(ws.default_targets),
    }
    save_json(ws.graph_store_path, graph)
    return graph


def load_graph_store(ws: Workspace) -> dict[str, Any]:
    graph = load_json(ws.graph_store_path, {})
    if graph.get("schema") != GRAPH_STORE_SCHEMA or graph.get("manifest_fingerprint") != workspace_manifest_fingerprint(ws):
        return compile_graph_store(ws)
    return graph


def graph_dot(ws: Workspace, roots: set[str] | None = None) -> str:
    subset = ws.target_closure(roots or set(ws.default_targets))
    lines = ["digraph frost {", "  rankdir=LR;"]
    for name in sorted(subset):
        target = ws.targets[name]
        shape = "box" if target.kind == "build" else "ellipse"
        lines.append(f"  {json.dumps(name)} [shape={shape}, label={json.dumps(name)}];")
        for dep in sorted(target.deps):
            if dep in subset:
                lines.append(f"  {json.dumps(dep)} -> {json.dumps(name)};")
    lines.append("}")
    return "\n".join(lines) + "\n"


def relative_cwd(ws: Workspace, cwd: pathlib.Path | None = None) -> str:
    cwd = (cwd or ws.root).resolve()
    try:
        rel = cwd.relative_to(ws.root)
    except ValueError:
        return cwd.as_posix()
    return rel.as_posix() or "."


def action_descriptor(
    ws: Workspace,
    t: Target,
    *,
    env: dict[str, str] | None = None,
    cwd: pathlib.Path | None = None,
) -> ActionDescriptor:
    env = dict(os.environ if env is None else env)
    dynamic_deps = load_dynamic_deps(ws).get(t.name, [])
    inputs: list[tuple[str, str]] = [
        (t.src, sha256_file(ws.path(t.src)) if ws.path(t.src).exists() else "MISSING_SRC"),
    ]
    for rel in sorted(set(t.declared_inputs) | set(dynamic_deps)):
        p = ws.path(rel)
        inputs.append((rel, sha256_file(p) if p.exists() else "MISSING_INPUT"))
    for dep in sorted(t.deps):
        out = ws.output_path(dep)
        inputs.append((ws.rel(out), sha256_file(out) if out.exists() else "MISSING_OUT"))
    return ActionDescriptor(
        argv=t.command or ("frost-sim-build", "--target", t.name, "--kind", t.kind, "--src", t.src),
        cwd=relative_cwd(ws, cwd),
        env=tuple(sorted((key, env[key]) for key in ws.env_whitelist if key in env)),
        inputs=tuple(sorted(inputs)),
    )


def toolchain_closure_hash(ws: Workspace, env: dict[str, str] | None = None) -> str:
    env = dict(os.environ if env is None else env)
    payload: dict[str, Any] = {
        "toolchain": ws.toolchain,
        "compiler_path": ws.toolchain_compiler_path,
        "sysroot": ws.toolchain_sysroot,
        "env": {key: env[key] for key in ws.env_whitelist if key in env},
    }
    if ws.toolchain_compiler_path:
        compiler = ws.path(ws.toolchain_compiler_path)
        if not compiler.exists():
            compiler = pathlib.Path(ws.toolchain_compiler_path)
        if compiler.exists() and compiler.is_file():
            hashes = hash_files_cached(ws, {compiler.resolve().as_posix()} if compiler.is_absolute() else {ws.toolchain_compiler_path})
            key = compiler.resolve().as_posix() if compiler.is_absolute() else ws.toolchain_compiler_path
            payload["compiler_digest"] = hashes.get(key, sha256_file(compiler))
            payload["compiler_stat"] = path_stat_record(compiler)
        else:
            payload["compiler_digest"] = "MISSING"
    return sha256_bytes(json.dumps(payload, sort_keys=True, separators=(",", ":")).encode())


def action_key(
    ws: Workspace,
    t: Target,
    *,
    env: dict[str, str] | None = None,
    cwd: pathlib.Path | None = None,
) -> str:
    descriptor = action_descriptor(ws, t, env=env, cwd=cwd)
    payload = {
        "schema": ACTION_KEY_VERSION,
        "builder": BUILDER_VERSION,
        "platform": platform.system().lower(),
        "target": t.name,
        "toolchain_hash": toolchain_closure_hash(ws, env=env),
        "descriptor": dataclasses.asdict(descriptor),
    }
    return sha256_bytes(json.dumps(payload, sort_keys=True, separators=(",", ":")).encode())


class ActionExecutionError(RuntimeError):
    def __init__(self, target: str, message: str, *, exit_code: int = 1, stdout: str = "", stderr: str = ""):
        super().__init__(message)
        self.target = target
        self.exit_code = exit_code
        self.stdout = stdout
        self.stderr = stderr or message


def materialize_blob(blob: pathlib.Path, out: pathlib.Path) -> str:
    out.parent.mkdir(parents=True, exist_ok=True)
    tmp = out.with_name(f".{out.name}.{os.getpid()}.tmp")
    with contextlib.suppress(FileNotFoundError):
        tmp.unlink()
    method = "copy"
    try:
        os.link(blob, tmp)
        method = "hardlink"
    except OSError:
        shutil.copyfile(blob, tmp)
    os.replace(tmp, out)
    return method


def store_blob_in_cas(ws: Workspace, out: pathlib.Path, digest: str) -> pathlib.Path:
    blob = ws.cas_dir / digest
    if blob.exists():
        return blob
    ws.cas_dir.mkdir(parents=True, exist_ok=True)
    fd, tmp_name = tempfile.mkstemp(prefix=f".{digest}.", suffix=".tmp", dir=ws.cas_dir)
    os.close(fd)
    tmp = pathlib.Path(tmp_name)
    try:
        shutil.copyfile(out, tmp)
        os.replace(tmp, blob)
    finally:
        with contextlib.suppress(FileNotFoundError):
            tmp.unlink()
    return blob


def cas_gc(ws: Workspace, max_bytes: int = DEFAULT_CAS_MAX_BYTES) -> dict[str, Any]:
    if max_bytes <= 0 or not ws.cas_dir.exists():
        return {"removed": 0, "bytes_before": 0, "bytes_after": 0}
    blobs = []
    total = 0
    for path in ws.cas_dir.iterdir():
        if not path.is_file() or path.name.startswith("."):
            continue
        st = path.stat()
        total += st.st_size
        blobs.append((st.st_atime_ns, st.st_mtime_ns, path, st.st_size))
    before = total
    removed = 0
    for _, _, path, size in sorted(blobs):
        if total <= max_bytes:
            break
        with contextlib.suppress(FileNotFoundError):
            path.unlink()
            total -= size
            removed += 1
    return {"removed": removed, "bytes_before": before, "bytes_after": total}


def restore_from_cache(ws: Workspace, out: pathlib.Path, digest: str) -> tuple[bool, str | None]:
    blob = ws.cas_dir / digest
    if not blob.exists():
        return False, None
    method = materialize_blob(blob, out)
    with contextlib.suppress(OSError):
        os.utime(blob, None)
    return True, method


def append_journal_entry(ws: Workspace, entry: dict[str, Any]) -> None:
    ws.frost_dir.mkdir(parents=True, exist_ok=True)
    payload = {
        "schema": BUILD_JOURNAL_SCHEMA,
        "time": time.time(),
        **entry,
    }
    line = json.dumps(payload, sort_keys=True, separators=(",", ":")) + "\n"
    with JOURNAL_LOCK:
        with ws.journal_path.open("a", encoding="utf-8") as file:
            file.write(line)


def load_journal_entries(ws: Workspace) -> list[dict[str, Any]]:
    entries: list[dict[str, Any]] = []
    if not ws.journal_path.exists():
        return entries
    with ws.journal_path.open("r", encoding="utf-8", errors="replace") as file:
        for line in file:
            try:
                entry = json.loads(line)
            except json.JSONDecodeError:
                continue
            if entry.get("schema") == BUILD_JOURNAL_SCHEMA:
                entries.append(entry)
    return entries


def compact_journal(ws: Workspace) -> dict[str, Any]:
    entries = load_journal_entries(ws)
    latest_by_target: dict[str, dict[str, Any]] = {}
    durations: dict[str, list[float]] = defaultdict(list)
    for entry in entries:
        target = entry.get("target")
        if not target:
            continue
        latest_by_target[target] = entry
        if entry.get("status") == "executed" and isinstance(entry.get("duration_ms"), (int, float)):
            durations[target].append(float(entry["duration_ms"]))
    compact = {
        "schema": BUILD_JOURNAL_SCHEMA,
        "entry_count": len(entries),
        "latest_by_target": latest_by_target,
        "duration_ms_by_target": {k: v[-32:] for k, v in sorted(durations.items())},
        "compacted_at": time.time(),
    }
    save_json(ws.journal_compact_path, compact)
    return compact


def journal_duration_stats(ws: Workspace) -> dict[str, list[float]]:
    compact = load_json(ws.journal_compact_path, {})
    if compact.get("schema") != BUILD_JOURNAL_SCHEMA:
        compact = compact_journal(ws)
    return {str(k): [float(x) for x in v] for k, v in compact.get("duration_ms_by_target", {}).items()}


def declared_input_set(ws: Workspace, target: Target) -> set[str]:
    declared = {target.src, *target.declared_inputs}
    declared.update(load_dynamic_deps(ws).get(target.name, []))
    for dep in target.deps:
        declared.add(ws.rel(ws.output_path(dep)))
    return declared


def sandbox_check(ws: Workspace, target: Target, enabled: bool) -> None:
    if not enabled or not target.sandbox:
        return
    source = ws.path(target.src)
    if not source.exists():
        return
    declared = declared_input_set(ws, target)
    for line in source.read_text(encoding="utf-8", errors="replace").splitlines():
        match = re.match(r"\s*READ\s+(.+?)\s*$", line)
        if not match:
            continue
        rel = pathlib.Path(match.group(1)).as_posix()
        if rel not in declared:
            raise ActionExecutionError(
                target.name,
                f"{target.name}: sandbox denied undeclared read {rel}",
                stderr=f"undeclared input: {rel}",
            )


def execute_action(
    ws: Workspace,
    name: str,
    action_cache: dict[str, Any],
    use_cache: bool,
    *,
    sandbox: bool = False,
) -> dict[str, Any]:
    t = ws.targets[name]
    out = ws.output_path(name)
    lookup_start = time.perf_counter()
    key = action_key(ws, t)
    cache_entry = action_cache.get(key)
    if use_cache and cache_entry:
        restored, method = restore_from_cache(ws, out, cache_entry["digest"])
    else:
        restored, method = False, None
    if restored:
        lookup_latency_ms = (time.perf_counter() - lookup_start) * 1000
        result = {
            "target": name,
            "status": "cache_hit",
            "digest": cache_entry["digest"],
            "key": key,
            "lookup_latency_ms": lookup_latency_ms,
            "exit_code": cache_entry.get("exit_code", 0),
            "materialize": method,
        }
        append_journal_entry(ws, result)
        return result
    lookup_latency_ms = (time.perf_counter() - lookup_start) * 1000

    # Simulate compiler/test work. This is the cost we try to avoid.
    action_start = time.perf_counter()
    sandbox_check(ws, t, sandbox)
    if ws.path(t.src).exists():
        source_text_for_fail = ws.path(t.src).read_text(encoding="utf-8", errors="replace")
        if re.search(r"(?m)^\s*FAIL(?:\s|$)", source_text_for_fail):
            raise ActionExecutionError(
                name,
                f"{name}: simulated action failure",
                exit_code=1,
                stdout="",
                stderr=f"{name}: source requested failure",
            )
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
        "src_hash": sha256_bytes(semantic_source_text(src_text).encode()),
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
    store_blob_in_cas(ws, out, digest)
    previous_digest = None
    compact = load_json(ws.journal_compact_path, {})
    latest = compact.get("latest_by_target", {}).get(name, {}) if compact.get("schema") == BUILD_JOURNAL_SCHEMA else {}
    if latest:
        previous_digest = latest.get("digest")
    dynamic_deps = record_target_depfile(ws, t)
    action_cache[key] = {
        "target": name,
        "kind": t.kind,
        "digest": digest,
        "outputs": {t.out: digest},
        "out": t.out,
        "exit_code": 0,
        "duration_ms": (time.perf_counter() - action_start) * 1000,
        "dynamic_deps": dynamic_deps,
        "created_at": time.time(),
    }
    result = {
        "target": name,
        "status": "executed",
        "digest": digest,
        "key": key,
        "lookup_latency_ms": lookup_latency_ms,
        "exit_code": 0,
        "duration_ms": action_cache[key]["duration_ms"],
        "unchanged_output": previous_digest == digest if previous_digest else False,
        "dynamic_deps": dynamic_deps,
    }
    append_journal_entry(ws, result)
    return result


def estimate_action_duration_ms(
    ws: Workspace,
    name: str,
    estimator: str,
    stats: dict[str, list[float]] | None = None,
) -> float:
    if estimator == "static":
        return float(ws.targets[name].cost_ms)
    stats = stats or {}
    samples = stats.get(name, [])
    if samples:
        if estimator in {"journal", "learned"}:
            return sum(samples[-8:]) / len(samples[-8:])
        return samples[-1]
    by_kind = [
        value
        for target_name, values in stats.items()
        if ws.targets.get(target_name) and ws.targets[target_name].kind == ws.targets[name].kind
        for value in values[-4:]
    ]
    if by_kind:
        return sum(by_kind) / len(by_kind)
    return float(ws.targets[name].cost_ms)


def critical_path_priorities(ws: Workspace, subset: set[str], estimator: str = "heuristic") -> dict[str, float]:
    stats = journal_duration_stats(ws) if estimator != "static" else {}
    reverse: dict[str, set[str]] = defaultdict(set)
    for name in subset:
        for dep in ws.targets[name].deps:
            if dep in subset:
                reverse[dep].add(name)
    memo: dict[str, float] = {}

    def score(name: str) -> float:
        if name in memo:
            return memo[name]
        child_scores = [score(parent) for parent in reverse.get(name, set())]
        memo[name] = estimate_action_duration_ms(ws, name, estimator, stats) + (max(child_scores) if child_scores else 0.0)
        return memo[name]

    return {name: score(name) for name in subset}


def estimator_benchmark(ws: Workspace, estimator: str, iterations: int = 10_000) -> dict[str, Any]:
    names = sorted(ws.targets)
    if not names:
        return {"estimator": estimator, "iterations": 0, "us_per_action": 0.0}
    stats = journal_duration_stats(ws) if estimator != "static" else {}
    start = time.perf_counter()
    total = 0.0
    for i in range(iterations):
        total += estimate_action_duration_ms(ws, names[i % len(names)], estimator, stats)
    elapsed = time.perf_counter() - start
    return {
        "estimator": estimator,
        "iterations": iterations,
        "us_per_action": round((elapsed / max(iterations, 1)) * 1_000_000, 3),
        "checksum": round(total, 3),
    }


def execute_plan(
    ws: Workspace,
    selected: set[str],
    jobs: int,
    use_cache: bool = True,
    *,
    keep_going: bool = False,
    scheduler: str = "critical-path",
    estimator: str = "heuristic",
    sandbox: bool = False,
    cas_max_bytes: int = DEFAULT_CAS_MAX_BYTES,
) -> dict[str, Any]:
    ws.ensure_dirs()
    if not selected:
        return {
            "executed": 0,
            "cache_hit": 0,
            "failed": 0,
            "skipped": 0,
            "results": [],
            "duration_s": 0.0,
            "cache_lookup_latency_ms_p50": 0.0,
            "scheduler": scheduler,
            "estimator": estimator,
            "cas_gc": {"removed": 0, "bytes_before": 0, "bytes_after": 0},
        }

    action_cache = load_json(ws.action_cache_path, {})
    selected = set(selected)
    deps_in_subset: dict[str, set[str]] = {n: {d for d in ws.targets[n].deps if d in selected} for n in selected}
    reverse: dict[str, set[str]] = defaultdict(set)
    for n, deps in deps_in_subset.items():
        for d in deps:
            reverse[d].add(n)
    priorities = critical_path_priorities(ws, selected, estimator) if scheduler == "critical-path" else {}
    ready_fifo = deque(sorted(n for n, deps in deps_in_subset.items() if not deps))
    ready_heap = [(-priorities.get(n, 0.0), n) for n in ready_fifo]
    if scheduler == "critical-path":
        heapq.heapify(ready_heap)
    running: dict[concurrent.futures.Future, str] = {}
    completed: set[str] = set()
    failed: set[str] = set()
    skipped: set[str] = set()
    results: list[dict[str, Any]] = []
    start = time.perf_counter()

    def has_ready() -> bool:
        return bool(ready_heap if scheduler == "critical-path" else ready_fifo)

    def pop_ready() -> str:
        if scheduler == "critical-path":
            return heapq.heappop(ready_heap)[1]
        return ready_fifo.popleft()

    def push_ready(name: str) -> None:
        if scheduler == "critical-path":
            heapq.heappush(ready_heap, (-priorities.get(name, 0.0), name))
        else:
            ready_fifo.append(name)

    with JobserverLease.from_environment(jobs) as jobserver:
        with concurrent.futures.ThreadPoolExecutor(max_workers=jobserver.effective_jobs) as pool:
            while has_ready() or running:
                while has_ready() and len(running) < jobserver.effective_jobs:
                    n = pop_ready()
                    fut = pool.submit(execute_action, ws, n, action_cache, use_cache, sandbox=sandbox)
                    running[fut] = n
                if not running:
                    break
                done, _ = concurrent.futures.wait(running.keys(), return_when=concurrent.futures.FIRST_COMPLETED)
                for fut in done:
                    n = running.pop(fut)
                    try:
                        res = fut.result()
                    except ActionExecutionError as error:
                        res = {
                            "target": error.target,
                            "status": "failed",
                            "exit_code": error.exit_code,
                            "stdout": error.stdout,
                            "stderr": error.stderr,
                        }
                        append_journal_entry(ws, res)
                        failed.add(n)
                        results.append(res)
                        if not keep_going:
                            raise
                        for parent in reverse.get(n, set()):
                            skipped.add(parent)
                        continue
                    results.append(res)
                    completed.add(n)
                    for parent in sorted(reverse.get(n, set())):
                        if parent in skipped:
                            continue
                        deps_in_subset[parent].remove(n)
                        if not deps_in_subset[parent]:
                            push_ready(parent)

    if failed:
        blocked = set()
        q = deque(failed)
        while q:
            node = q.popleft()
            for parent in reverse.get(node, set()):
                if parent not in blocked:
                    blocked.add(parent)
                    q.append(parent)
        skipped.update(blocked - completed - failed)
    if completed | failed | skipped != selected:
        missing = selected - completed - failed - skipped
        raise RuntimeError(f"build did not complete; missing={sorted(missing)}")
    save_json(ws.action_cache_path, action_cache)
    save_json(ws.state_path, {"source_hashes": all_source_hashes(ws), "updated_at": time.time()})
    compact_journal(ws)
    gc_result = cas_gc(ws, cas_max_bytes)
    duration = time.perf_counter() - start
    executed = sum(1 for r in results if r["status"] == "executed")
    cache_hit = sum(1 for r in results if r["status"] == "cache_hit")
    failure_count = sum(1 for r in results if r["status"] == "failed")
    lookup_latencies = sorted(r.get("lookup_latency_ms", 0.0) for r in results)
    p50 = lookup_latencies[len(lookup_latencies) // 2] if lookup_latencies else 0.0
    return {
        "executed": executed,
        "cache_hit": cache_hit,
        "failed": failure_count,
        "skipped": len(skipped),
        "results": sorted(results, key=lambda x: x["target"]),
        "duration_s": duration,
        "cache_lookup_latency_ms_p50": p50,
        "scheduler": scheduler,
        "estimator": estimator,
        "cas_gc": gc_result,
    }


def write_metadata(ws: Workspace) -> None:
    save_json(ws.metadata_path, build_metadata_catalog(ws, include_source_hashes=True))


@contextlib.contextmanager
def workspace_lock(ws: Workspace) -> Any:
    ws.frost_dir.mkdir(parents=True, exist_ok=True)
    fd: int | None = None
    try:
        fd = os.open(ws.lock_path, os.O_CREAT | os.O_EXCL | os.O_WRONLY)
        os.write(fd, str(os.getpid()).encode())
        yield
    except FileExistsError as error:
        raise RuntimeError(f"workspace is already building: {ws.lock_path}") from error
    finally:
        if fd is not None:
            os.close(fd)
            with contextlib.suppress(FileNotFoundError):
                ws.lock_path.unlink()


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
    roots = test_roots(ws, args.target) if "test" in kinds and getattr(args, "test_mode", False) else set(args.target or ws.default_targets)
    load_graph_store(ws)
    write_metadata(ws)
    force_all = args.force or getattr(args, "all", False)
    plan = build_plan(ws, roots, kinds=kinds, explicit_changed=args.changed, force_all=force_all)
    predictive = apply_predictive_test_selection(ws, plan, enabled=getattr(args, "predictive", False))
    if args.dry_run:
        print_plan(ws, plan)
        return
    with workspace_lock(ws):
        result = execute_plan(
            ws,
            plan.selected_targets,
            jobs=args.jobs,
            use_cache=not args.no_cache,
            keep_going=getattr(args, "keep_going", False),
            scheduler=getattr(args, "scheduler", "critical-path"),
            estimator=getattr(args, "estimator", "heuristic"),
            sandbox=getattr(args, "sandbox", False),
            cas_max_bytes=getattr(args, "cas_max_bytes", DEFAULT_CAS_MAX_BYTES),
        )
    print(json.dumps({
        "plan_reason": plan.reason,
        "changed_sources": plan.changed_sources,
        "selected_count": len(plan.selected_targets),
        "pruned_count": len(plan.pruned_targets),
        "executed": result["executed"],
        "cache_hit": result["cache_hit"],
        "failed": result["failed"],
        "skipped": result.get("skipped", 0),
        "duration_s": round(result["duration_s"], 4),
        "cache_lookup_latency_ms_p50": round(result["cache_lookup_latency_ms_p50"], 4),
        "scheduler": result["scheduler"],
        "estimator": result["estimator"],
        "predictive": predictive,
        "selected_targets": sorted(plan.selected_targets),
        "failures": [
            {
                "target": r["target"],
                "exit_code": r.get("exit_code", 1),
                "stderr": r.get("stderr", ""),
            }
            for r in result["results"]
            if r["status"] == "failed"
        ],
    }, indent=2, sort_keys=True))
    if result["failed"]:
        raise SystemExit(1)


def clean(args: argparse.Namespace) -> None:
    ws = Workspace(pathlib.Path(args.workspace))
    if getattr(args, "cache", False):
        if ws.frost_dir.exists():
            shutil.rmtree(ws.frost_dir)
        print(f"removed {ws.frost_dir}")
        return
    removed: list[pathlib.Path] = []
    for path in (ws.out_dir, ws.state_path, ws.metadata_path, ws.graph_store_path, ws.stat_cache_path, ws.journal_compact_path):
        if path.is_dir():
            shutil.rmtree(path)
            removed.append(path)
        elif path.exists():
            path.unlink()
            removed.append(path)
    if removed:
        print("removed " + ", ".join(str(path) for path in removed))
    else:
        print(f"nothing to remove under {ws.frost_dir}")


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
        "cache_lookup_latency_ms_p50": round(micro["cache_lookup_latency_ms_p50"], 4),
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


def send_frame(sock: socket.socket, payload: dict[str, Any]) -> None:
    data = json.dumps(payload, sort_keys=True, separators=(",", ":")).encode()
    sock.sendall(struct.pack("!I", len(data)) + data)


def recv_exact(sock: socket.socket, size: int) -> bytes:
    chunks = []
    remaining = size
    while remaining:
        chunk = sock.recv(remaining)
        if not chunk:
            raise ConnectionError("socket closed")
        chunks.append(chunk)
        remaining -= len(chunk)
    return b"".join(chunks)


def recv_frame(sock: socket.socket) -> dict[str, Any]:
    header = recv_exact(sock, 4)
    size = struct.unpack("!I", header)[0]
    return json.loads(recv_exact(sock, size).decode())


def daemon_request(ws: Workspace, request: dict[str, Any], *, timeout: float = 2.0) -> dict[str, Any]:
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
        sock.settimeout(timeout)
        sock.connect(str(ws.daemon_socket_path))
        send_frame(sock, {"protocol": DAEMON_PROTOCOL_VERSION, **request})
        return recv_frame(sock)


def daemon_is_running(ws: Workspace) -> bool:
    if not ws.daemon_pid_path.exists() or not ws.daemon_socket_path.exists():
        return False
    try:
        pid = int(ws.daemon_pid_path.read_text(encoding="utf-8").strip())
        os.kill(pid, 0)
        return True
    except (OSError, ValueError):
        return False


def start_daemon(ws: Workspace) -> dict[str, Any]:
    ws.ensure_dirs()
    if daemon_is_running(ws):
        return daemon_request(ws, {"op": "status"})
    with contextlib.suppress(FileNotFoundError):
        ws.daemon_socket_path.unlink()
    subprocess.Popen(
        [sys.executable, pathlib.Path(__file__).resolve().as_posix(), "daemon", "serve", "--workspace", ws.root.as_posix()],
        cwd=ws.root,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        start_new_session=True,
    )
    deadline = time.time() + 2.0
    while time.time() < deadline:
        if daemon_is_running(ws):
            try:
                return daemon_request(ws, {"op": "status"})
            except OSError:
                pass
        time.sleep(0.025)
    raise RuntimeError("daemon did not start")


def daemon_dirty_snapshot(ws: Workspace) -> dict[str, Any]:
    changed, targets, reason = detect_changed_sources(ws)
    snapshot = {
        "schema": "frost-daemon-dirty-v1",
        "updated_at": time.time(),
        "reason": reason,
        "changed_sources": changed,
        "changed_targets": sorted(targets),
    }
    save_json(ws.daemon_dirty_path, snapshot)
    return snapshot


def handle_daemon_request(ws: Workspace, request: dict[str, Any]) -> tuple[dict[str, Any], bool]:
    op = request.get("op")
    if op == "status":
        return {
            "ok": True,
            "protocol": DAEMON_PROTOCOL_VERSION,
            "pid": os.getpid(),
            "workspace": ws.root.as_posix(),
            "dirty": daemon_dirty_snapshot(ws),
        }, False
    if op == "shutdown":
        return {"ok": True, "pid": os.getpid(), "shutdown": True}, True
    if op == "build":
        roots = set(request.get("targets") or ws.default_targets)
        kinds = set(request.get("kinds") or ["build"])
        write_metadata(ws)
        plan = build_plan(ws, roots, kinds, request.get("changed"), bool(request.get("force", False)))
        with workspace_lock(ws):
            result = execute_plan(
                ws,
                plan.selected_targets,
                int(request.get("jobs") or 1),
                use_cache=bool(request.get("use_cache", True)),
                keep_going=bool(request.get("keep_going", False)),
                scheduler=str(request.get("scheduler", "critical-path")),
                estimator=str(request.get("estimator", "heuristic")),
                sandbox=bool(request.get("sandbox", False)),
            )
        return {
            "ok": result["failed"] == 0,
            "plan": dataclasses.asdict(plan),
            "result": result,
        }, False
    return {"ok": False, "error": f"unknown daemon op: {op}"}, False


def serve_daemon(args: argparse.Namespace) -> None:
    ws = Workspace(pathlib.Path(args.workspace))
    ws.ensure_dirs()
    with contextlib.suppress(FileNotFoundError):
        ws.daemon_socket_path.unlink()
    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    server.bind(str(ws.daemon_socket_path))
    server.listen(16)
    ws.daemon_pid_path.write_text(str(os.getpid()) + "\n", encoding="utf-8")
    stop = False
    try:
        while not stop:
            conn, _ = server.accept()
            with conn:
                try:
                    request = recv_frame(conn)
                    if request.get("protocol") != DAEMON_PROTOCOL_VERSION:
                        response = {"ok": False, "error": "protocol mismatch"}
                    else:
                        response, stop = handle_daemon_request(ws, request)
                except Exception as error:
                    response = {"ok": False, "error": str(error)}
                send_frame(conn, response)
    finally:
        server.close()
        with contextlib.suppress(FileNotFoundError):
            ws.daemon_socket_path.unlink()
        with contextlib.suppress(FileNotFoundError):
            ws.daemon_pid_path.unlink()


def daemon_command(args: argparse.Namespace) -> None:
    ws = Workspace(pathlib.Path(args.workspace))
    if args.daemon_cmd == "serve":
        serve_daemon(args)
        return
    if args.daemon_cmd == "start":
        print(json.dumps(start_daemon(ws), indent=2, sort_keys=True))
        return
    if args.daemon_cmd == "status":
        if daemon_is_running(ws):
            print(json.dumps(daemon_request(ws, {"op": "status"}), indent=2, sort_keys=True))
        else:
            print(json.dumps({"ok": False, "running": False}, indent=2, sort_keys=True))
        return
    if args.daemon_cmd == "stop":
        if daemon_is_running(ws):
            print(json.dumps(daemon_request(ws, {"op": "shutdown"}), indent=2, sort_keys=True))
        else:
            print(json.dumps({"ok": True, "running": False}, indent=2, sort_keys=True))
        return
    if args.daemon_cmd == "restart":
        if daemon_is_running(ws):
            daemon_request(ws, {"op": "shutdown"})
            time.sleep(0.05)
        print(json.dumps(start_daemon(ws), indent=2, sort_keys=True))
        return
    raise SystemExit(f"unknown daemon command: {args.daemon_cmd}")


def run_daemon_build(args: argparse.Namespace, kinds: set[str]) -> bool:
    if not getattr(args, "daemon", False):
        return False
    ws = Workspace(pathlib.Path(args.workspace))
    try:
        if not daemon_is_running(ws):
            start_daemon(ws)
        response = daemon_request(
            ws,
            {
                "op": "build",
                "targets": args.target or list(ws.default_targets),
                "kinds": sorted(kinds),
                "changed": args.changed,
                "force": args.force or getattr(args, "all", False),
                "jobs": args.jobs,
                "use_cache": not args.no_cache,
                "keep_going": getattr(args, "keep_going", False),
                "scheduler": getattr(args, "scheduler", "critical-path"),
                "estimator": getattr(args, "estimator", "heuristic"),
                "sandbox": getattr(args, "sandbox", False),
            },
        )
        print(json.dumps(response, indent=2, sort_keys=True))
        if not response.get("ok"):
            raise SystemExit(1)
        return True
    except Exception as error:
        print(json.dumps({"daemon": "fallback", "reason": str(error)}, sort_keys=True), file=sys.stderr)
        return False


def expand_ninja_vars(value: str, variables: dict[str, str]) -> str:
    pattern = re.compile(r"\$(\w+)|\$\{([^}]+)\}")

    def repl(match: re.Match[str]) -> str:
        key = match.group(1) or match.group(2)
        return variables.get(key, "")

    previous = None
    current = value
    while previous != current:
        previous = current
        current = pattern.sub(repl, current)
    return current


def sanitize_target_name(value: str) -> str:
    cleaned = re.sub(r"[^A-Za-z0-9_]+", "_", value).strip("_")
    return cleaned or "target"


def parse_ninja_subset(path: pathlib.Path) -> dict[str, Any]:
    variables: dict[str, str] = {}
    rules: dict[str, dict[str, str]] = {}
    builds: list[dict[str, Any]] = []
    defaults: list[str] = []
    lines = path.read_text(encoding="utf-8").splitlines()
    i = 0
    current_rule: str | None = None
    while i < len(lines):
        raw = lines[i]
        line = raw.strip()
        i += 1
        if not line or line.startswith("#"):
            continue
        if raw.startswith("  ") and current_rule and "=" in line:
            key, value = line.split("=", 1)
            rules[current_rule][key.strip()] = expand_ninja_vars(value.strip(), variables)
            continue
        current_rule = None
        if "=" in line and not line.startswith(("rule ", "build ", "default ")):
            key, value = line.split("=", 1)
            variables[key.strip()] = expand_ninja_vars(value.strip(), variables)
            continue
        if line.startswith("rule "):
            current_rule = line.split(None, 1)[1]
            rules[current_rule] = {}
            continue
        if line.startswith("build "):
            rest = line.removeprefix("build ").strip()
            if ":" not in rest:
                raise ValueError(f"unsupported ninja build line: {line}")
            outputs_text, rhs = rest.split(":", 1)
            parts = shlex.split(expand_ninja_vars(rhs.strip(), variables))
            if not parts:
                raise ValueError(f"missing ninja rule: {line}")
            rule = parts[0]
            inputs = [part for part in parts[1:] if part not in {"|", "||"}]
            builds.append({
                "outputs": shlex.split(expand_ninja_vars(outputs_text, variables)),
                "rule": rule,
                "inputs": inputs,
                "command": rules.get(rule, {}).get("command", ""),
            })
            continue
        if line.startswith("default "):
            defaults.extend(shlex.split(expand_ninja_vars(line.removeprefix("default ").strip(), variables)))
            continue
        raise ValueError(f"unsupported ninja syntax: {line}")
    return {"variables": variables, "rules": rules, "builds": builds, "defaults": defaults}


def ninja_subset_to_frost_toml(ninja: dict[str, Any]) -> str:
    output_to_name: dict[str, str] = {}
    for build in ninja["builds"]:
        for output in build["outputs"]:
            output_to_name[output] = sanitize_target_name(output)
    targets: dict[str, dict[str, Any]] = {}
    for build in ninja["builds"]:
        if build["rule"] == "phony":
            continue
        primary = build["outputs"][0]
        name = output_to_name[primary]
        deps = [output_to_name[input_] for input_ in build["inputs"] if input_ in output_to_name]
        sources = [input_ for input_ in build["inputs"] if input_ not in output_to_name]
        src = sources[0] if sources else primary
        targets[name] = {
            "kind": "build",
            "src": src,
            "deps": deps,
            "out": primary,
            "cost_ms": 1,
            "command": shlex.split(build["command"]) if build["command"] else [],
        }
    defaults = [output_to_name[item] for item in ninja.get("defaults", []) if item in output_to_name]
    if not defaults and targets:
        defaults = [next(reversed(targets))]
    lines = [
        "[workspace]",
        'toolchain = "ninja-imported-toolchain"',
        "default_targets = [" + ", ".join(json.dumps(item) for item in defaults) + "]",
        "",
    ]
    for name in sorted(targets):
        spec = targets[name]
        lines.append(f"[target.{name}]")
        lines.append(f"kind = {json.dumps(spec['kind'])}")
        lines.append(f"src = {json.dumps(spec['src'])}")
        lines.append("deps = [" + ", ".join(json.dumps(dep) for dep in spec["deps"]) + "]")
        lines.append(f"out = {json.dumps(spec['out'])}")
        lines.append(f"cost_ms = {spec['cost_ms']}")
        if spec["command"]:
            lines.append("command = [" + ", ".join(json.dumps(part) for part in spec["command"]) + "]")
        lines.append("")
    return "\n".join(lines)


def import_ninja(args: argparse.Namespace) -> None:
    ninja_path = pathlib.Path(args.ninja).resolve()
    data = parse_ninja_subset(ninja_path)
    toml_text = ninja_subset_to_frost_toml(data)
    if args.out:
        pathlib.Path(args.out).write_text(toml_text, encoding="utf-8")
    else:
        print(toml_text)


def graph_command(args: argparse.Namespace) -> None:
    ws = Workspace(pathlib.Path(args.workspace))
    load_graph_store(ws)
    roots = set(args.target or ws.default_targets)
    dot = graph_dot(ws, roots)
    if args.out:
        pathlib.Path(args.out).write_text(dot, encoding="utf-8")
    else:
        print(dot, end="")


def estimator_command(args: argparse.Namespace) -> None:
    ws = Workspace(pathlib.Path(args.workspace))
    print(json.dumps(estimator_benchmark(ws, args.estimator, args.iterations), indent=2, sort_keys=True))


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
        p.add_argument("--keep-going", action="store_true", help="continue independent work after failures")
        p.add_argument("--sandbox", action="store_true", help="deny undeclared READ lines in simulated actions")
        p.add_argument("--daemon", action="store_true", help="try frostd and fall back to standalone execution")
        p.add_argument("--scheduler", choices=("critical-path", "fifo"), default="critical-path")
        p.add_argument("--estimator", choices=("heuristic", "journal", "static", "learned"), default="heuristic")
        p.add_argument("--cas-max-bytes", type=int, default=DEFAULT_CAS_MAX_BYTES)

    p = sub.add_parser("plan", help="show affected build plan")
    add_common(p)
    p.set_defaults(func=lambda args: run_build(args, {"build"}))

    p = sub.add_parser("build", help="build affected targets")
    add_common(p)
    p.set_defaults(func=lambda args: None if run_daemon_build(args, {"build"}) else run_build(args, {"build"}))

    p = sub.add_parser("test", help="run affected tests")
    add_common(p)
    p.add_argument("--affected", action="store_true", help="run only tests affected by the dirty set")
    p.add_argument("--all", action="store_true", help="run the full test closure")
    p.add_argument("--explain", action="store_true", help="include selection explanation in JSON output")
    p.add_argument("--predictive", action="store_true", help="opt in to journal-distance predictive test selection")
    p.set_defaults(test_mode=True, func=lambda args: None if run_daemon_build(args, {"build", "test"}) else run_build(args, {"build", "test"}))

    p = sub.add_parser("graph", help="write the target graph as DOT")
    p.add_argument("--workspace", default="sample")
    p.add_argument("--target", action="append")
    p.add_argument("--out")
    p.set_defaults(func=graph_command)

    p = sub.add_parser("clean")
    p.add_argument("--workspace", default="sample")
    p.add_argument("--cache", action="store_true", help="also remove the local action cache and CAS")
    p.set_defaults(func=clean)

    p = sub.add_parser("bench", help="compare micro-partition incremental build vs naive full rebuild")
    p.add_argument("--workspace", default="sample")
    p.add_argument("--change", default="src/pkg05_mod07.fb")
    p.add_argument("--jobs", type=int, default=max(1, (os.cpu_count() or 4) // 2))
    p.add_argument("--with-bazel", action="store_true", help="also try Bazel if installed")
    p.set_defaults(func=bench)

    p = sub.add_parser("daemon", help="manage frostd")
    daemon_sub = p.add_subparsers(dest="daemon_cmd", required=True)
    for name in ("serve", "start", "status", "stop", "restart"):
        dp = daemon_sub.add_parser(name)
        dp.add_argument("--workspace", default="sample")
        dp.set_defaults(func=daemon_command)

    p = sub.add_parser("import-ninja", help="convert a supported build.ninja subset to frost.toml")
    p.add_argument("--ninja", default="build.ninja")
    p.add_argument("--out")
    p.set_defaults(func=import_ninja)

    p = sub.add_parser("estimator-bench", help="measure duration-estimator hot-path overhead")
    p.add_argument("--workspace", default="sample")
    p.add_argument("--estimator", choices=("heuristic", "journal", "static", "learned"), default="heuristic")
    p.add_argument("--iterations", type=int, default=10_000)
    p.set_defaults(func=estimator_command)

    args = parser.parse_args(argv)
    args.func(args)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
