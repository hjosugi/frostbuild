#!/usr/bin/env python3
"""Median benchmark harness for generated Ninja/Make workspaces."""

from __future__ import annotations

import argparse
import dataclasses
import datetime as dt
import hashlib
import json
import os
import pathlib
import platform
import shutil
import statistics
import subprocess
import tempfile
import time
from typing import Any

SCHEMA = "frost-bench-standard-v1"
STANDARD_SCENARIOS = ("clean", "noop", "incremental_leaf", "hot_header", "cache_hit_rebuild")
SUPPORTED_TOOLS = ("ninja", "make", "frost")


@dataclasses.dataclass(frozen=True)
class ToolSpec:
    name: str
    argv: tuple[str, ...]


def parse_csv(value: str, *, valid: tuple[str, ...] | None = None) -> list[str]:
    items = [item.strip() for item in value.split(",") if item.strip()]
    if valid is not None:
        unknown = sorted(set(items) - set(valid))
        if unknown:
            raise argparse.ArgumentTypeError(f"unsupported value(s): {', '.join(unknown)}")
    return items


def parse_sizes(value: str) -> list[int]:
    sizes: list[int] = []
    for item in parse_csv(value):
        try:
            size = int(item)
        except ValueError as error:
            raise argparse.ArgumentTypeError(f"invalid size: {item}") from error
        if size <= 0:
            raise argparse.ArgumentTypeError("sizes must be positive")
        sizes.append(size)
    return sizes


def utc_now() -> str:
    return dt.datetime.now(dt.UTC).replace(microsecond=0).isoformat().replace("+00:00", "Z")


def read_text(path: pathlib.Path) -> str | None:
    try:
        return path.read_text(encoding="utf-8").strip()
    except OSError:
        return None


def detect_cpu_governor() -> str | None:
    return read_text(pathlib.Path("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor"))


def detect_turbo_state() -> str | None:
    intel_no_turbo = read_text(pathlib.Path("/sys/devices/system/cpu/intel_pstate/no_turbo"))
    if intel_no_turbo is not None:
        return "disabled" if intel_no_turbo == "1" else "enabled"
    amd_boost = read_text(pathlib.Path("/sys/devices/system/cpu/cpufreq/boost"))
    if amd_boost is not None:
        return "enabled" if amd_boost == "1" else "disabled"
    return None


def environment_snapshot() -> dict[str, Any]:
    load_avg = os.getloadavg() if hasattr(os, "getloadavg") else None
    return {
        "captured_at": utc_now(),
        "hostname": platform.node(),
        "platform": platform.platform(),
        "machine": platform.machine(),
        "processor": platform.processor(),
        "python": platform.python_version(),
        "cpu_count": os.cpu_count(),
        "load_avg": list(load_avg) if load_avg else None,
        "cpu_governor": detect_cpu_governor(),
        "turbo": detect_turbo_state(),
    }


def write_stamp_tool(root: pathlib.Path) -> None:
    tools = root / "tools"
    tools.mkdir(parents=True, exist_ok=True)
    stamp = tools / "stamp.py"
    stamp.write_text(
        """#!/usr/bin/env python3
from __future__ import annotations

import hashlib
import pathlib
import sys

out = pathlib.Path(sys.argv[1])
digest = hashlib.sha256()
for name in sys.argv[2:]:
    path = pathlib.Path(name)
    digest.update(name.encode())
    digest.update(b"\\0")
    if path.exists():
        digest.update(path.read_bytes())
out.parent.mkdir(parents=True, exist_ok=True)
out.write_text(digest.hexdigest() + "\\n", encoding="utf-8")
""",
        encoding="utf-8",
    )
    stamp.chmod(0o755)


def target_name(index: int) -> str:
    return f"node{index:05d}"


def generate_workspace(root: pathlib.Path, size: int) -> None:
    if root.exists():
        shutil.rmtree(root)
    (root / "src").mkdir(parents=True)
    (root / "include").mkdir()
    (root / "out").mkdir()
    write_stamp_tool(root)
    (root / "include/hot.h").write_text("HOT=1\n", encoding="utf-8")
    for index in range(size):
        (root / "src" / f"{target_name(index)}.txt").write_text(f"value={index}\n", encoding="utf-8")
    write_ninja(root, size)
    write_makefile(root, size)
    write_frost_toml(root, size)


def write_ninja(root: pathlib.Path, size: int) -> None:
    lines = [
        "rule stamp",
        "  command = printf '%s\\n' $out > $out",
        "  description = STAMP $out",
        "",
    ]
    for index in range(size):
        name = target_name(index)
        inputs = [f"src/{name}.txt", "include/hot.h"]
        if index > 0:
            inputs.insert(1, f"out/{target_name(index - 1)}.out")
        lines.append(f"build out/{name}.out: stamp {' '.join(inputs)}")
    lines.extend(["", f"build all: phony out/{target_name(size - 1)}.out", "default all", ""])
    (root / "build.ninja").write_text("\n".join(lines), encoding="utf-8")


def write_makefile(root: pathlib.Path, size: int) -> None:
    lines = [
        ".PHONY: all",
        f"all: out/{target_name(size - 1)}.out",
        "",
    ]
    for index in range(size):
        name = target_name(index)
        deps = [f"src/{name}.txt", "include/hot.h", "tools/stamp.py"]
        if index > 0:
            deps.insert(1, f"out/{target_name(index - 1)}.out")
        lines.append(f"out/{name}.out: {' '.join(deps)}")
        lines.append("\tprintf '%s\\n' $@ > $@")
        lines.append("")
    (root / "Makefile").write_text("\n".join(lines), encoding="utf-8")


def write_frost_toml(root: pathlib.Path, size: int) -> None:
    lines = [
        "[workspace]",
        f"default_targets = [{json.dumps(target_name(size - 1))}]",
        "",
    ]
    for index in range(size):
        name = target_name(index)
        deps = [target_name(index - 1)] if index > 0 else []
        lines.append(f"[target.{name}]")
        lines.append('kind = "genrule"')
        lines.append('cmd = "cat ${in} > ${out}"')
        lines.append("deps = [" + ", ".join(json.dumps(dep) for dep in deps) + "]")
        lines.append(
            "inputs = ["
            + ", ".join(json.dumps(path) for path in [f"src/{name}.txt", "include/hot.h"])
            + "]"
        )
        lines.append(f"outputs = [{json.dumps(f'.frost/out/{name}.out')}]")
        lines.append("")
    (root / "frost.toml").write_text("\n".join(lines), encoding="utf-8")


def clean_outputs(root: pathlib.Path) -> None:
    out = root / "out"
    if out.exists():
        shutil.rmtree(out)
    out.mkdir()


def clean_tool_outputs(root: pathlib.Path, spec: ToolSpec, *, cache: bool = False) -> None:
    if spec.name == "frost":
        frost_dir = root / ".frost"
        if cache and frost_dir.exists():
            shutil.rmtree(frost_dir)
        else:
            out = frost_dir / "out"
            if out.exists():
                shutil.rmtree(out)
        return
    clean_outputs(root)


def append_marker(path: pathlib.Path, marker: str) -> None:
    with path.open("a", encoding="utf-8") as file:
        file.write(f"# {marker} {time.time_ns()}\n")


def tool_specs(names: list[str]) -> list[ToolSpec]:
    specs = []
    for name in names:
        if name == "frost":
            repo = pathlib.Path(__file__).resolve().parent
            configured = os.environ.get("FROST_BIN")
            candidates = [
                pathlib.Path(configured) if configured else None,
                repo / "target/release/frost",
                repo / "target/debug/frost",
            ]
            executable = next((path for path in candidates if path and path.is_file()), None)
            if executable is None and shutil.which("cargo"):
                try:
                    metadata = json.loads(
                        subprocess.check_output(
                            ["cargo", "metadata", "--format-version=1", "--no-deps"],
                            cwd=repo,
                            text=True,
                            stderr=subprocess.DEVNULL,
                        )
                    )
                    target = pathlib.Path(metadata["target_directory"])
                    executable = next(
                        (path for path in [target / "release/frost", target / "debug/frost"] if path.is_file()),
                        None,
                    )
                except (OSError, subprocess.SubprocessError, KeyError, json.JSONDecodeError):
                    executable = None
            specs.append(
                ToolSpec(
                    name=name,
                    argv=(executable.as_posix(), "build", "--workspace", ".") if executable else (),
                )
            )
            continue
        executable = shutil.which(name)
        if executable is None:
            specs.append(ToolSpec(name=name, argv=()))
        elif name == "ninja":
            specs.append(ToolSpec(name=name, argv=(executable, "-f", "build.ninja")))
        elif name == "make":
            specs.append(ToolSpec(name=name, argv=(executable, "-f", "Makefile")))
    return specs


def run_tool(root: pathlib.Path, spec: ToolSpec, jobs: int) -> float:
    if not spec.argv:
        raise FileNotFoundError(spec.name)
    if spec.name == "frost":
        cmd = [*spec.argv, "--jobs", str(max(1, jobs))]
    else:
        cmd = [*spec.argv, "-j", str(max(1, jobs))]
    start = time.perf_counter()
    subprocess.run(cmd, cwd=root, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    return (time.perf_counter() - start) * 1000


def summarize(samples: list[float]) -> dict[str, Any]:
    rounded = [round(value, 3) for value in samples]
    return {
        "samples_ms": rounded,
        "median_ms": round(statistics.median(samples), 3),
        "min_ms": round(min(samples), 3),
        "max_ms": round(max(samples), 3),
    }


def scenario_not_applicable(reason: str) -> dict[str, Any]:
    return {"applicable": False, "reason": reason}


def measure_tool(root: pathlib.Path, spec: ToolSpec, size: int, iterations: int, jobs: int) -> dict[str, Any]:
    if not spec.argv:
        return {
            "tool": spec.name,
            "size": size,
            "status": "skipped",
            "reason": f"{spec.name} executable was not found",
            "scenarios": {},
        }

    scenarios: dict[str, Any] = {}

    clean_samples = []
    for _ in range(iterations):
        clean_tool_outputs(root, spec, cache=True)
        clean_samples.append(run_tool(root, spec, jobs))
    scenarios["clean"] = summarize(clean_samples)

    noop_samples = []
    run_tool(root, spec, jobs)
    for _ in range(iterations):
        noop_samples.append(run_tool(root, spec, jobs))
    scenarios["noop"] = summarize(noop_samples)

    leaf = root / "src" / f"{target_name(size - 1)}.txt"
    incremental_samples = []
    run_tool(root, spec, jobs)
    for _ in range(iterations):
        append_marker(leaf, "leaf")
        incremental_samples.append(run_tool(root, spec, jobs))
    scenarios["incremental_leaf"] = summarize(incremental_samples)

    header = root / "include/hot.h"
    header_samples = []
    run_tool(root, spec, jobs)
    for _ in range(iterations):
        append_marker(header, "hot-header")
        header_samples.append(run_tool(root, spec, jobs))
    scenarios["hot_header"] = summarize(header_samples)
    if spec.name == "frost":
        cache_hit_samples = []
        run_tool(root, spec, jobs)
        for _ in range(iterations):
            clean_tool_outputs(root, spec, cache=False)
            cache_hit_samples.append(run_tool(root, spec, jobs))
        scenarios["cache_hit_rebuild"] = summarize(cache_hit_samples)
    else:
        scenarios["cache_hit_rebuild"] = scenario_not_applicable(
            f"{spec.name} has no content-addressed action cache in this harness",
        )

    return {
        "tool": spec.name,
        "size": size,
        "status": "ok",
        "iterations": iterations,
        "jobs": jobs,
        "target_count": size,
        "scenarios": scenarios,
    }


def run_standard(args: argparse.Namespace) -> dict[str, Any]:
    tools = parse_csv(args.tools, valid=SUPPORTED_TOOLS)
    sizes = parse_sizes(args.sizes)
    if args.iterations <= 0:
        raise SystemExit("--iterations must be positive")
    if args.jobs <= 0:
        raise SystemExit("--jobs must be positive")

    if args.workdir:
        base_workdir = pathlib.Path(args.workdir).resolve()
        base_workdir.mkdir(parents=True, exist_ok=True)
        temp_context = None
    else:
        temp_context = tempfile.TemporaryDirectory(prefix="frost-bench-")
        base_workdir = pathlib.Path(temp_context.name)

    results = []
    try:
        for size in sizes:
            for spec in tool_specs(tools):
                root = base_workdir / f"{args.suite}-{spec.name}-{size}"
                generate_workspace(root, size)
                results.append(measure_tool(root, spec, size, args.iterations, args.jobs))
    finally:
        if temp_context is not None and not args.keep_workdir:
            temp_context.cleanup()

    report = {
        "schema": SCHEMA,
        "suite": args.suite,
        "generated_at": utc_now(),
        "environment": environment_snapshot(),
        "config": {
            "tools": tools,
            "sizes": sizes,
            "iterations": args.iterations,
            "jobs": args.jobs,
            "scenarios": list(STANDARD_SCENARIOS),
        },
        "results": results,
    }
    return report


def report_digest(report: dict[str, Any]) -> str:
    payload = json.dumps(report, sort_keys=True, separators=(",", ":")).encode()
    return hashlib.sha256(payload).hexdigest()


def write_report(path: pathlib.Path, report: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def run(args: argparse.Namespace) -> int:
    if args.suite != "standard":
        raise SystemExit("only --suite standard is supported")
    report = run_standard(args)
    report["digest"] = report_digest(report)
    if args.out:
        write_report(pathlib.Path(args.out), report)
    print(json.dumps(report, indent=2, sort_keys=True))
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="FrostBuild benchmark harness")
    sub = parser.add_subparsers(dest="cmd", required=True)

    run_parser = sub.add_parser("run", help="run a benchmark suite")
    run_parser.add_argument("--suite", default="standard")
    run_parser.add_argument("--tools", default="ninja,make")
    run_parser.add_argument("--sizes", default="1000,10000")
    run_parser.add_argument("--iterations", type=int, default=5)
    run_parser.add_argument("--jobs", type=int, default=max(1, (os.cpu_count() or 4) // 2))
    run_parser.add_argument("--workdir", help="directory for generated benchmark workspaces")
    run_parser.add_argument("--keep-workdir", action="store_true")
    run_parser.add_argument("--out", help="write JSON report to this path as well as stdout")
    run_parser.set_defaults(func=run)

    args = parser.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    raise SystemExit(main())
