#!/usr/bin/env python3
"""Median benchmark harness for equivalent generated build graphs."""

from __future__ import annotations

import argparse
import base64
import csv
import dataclasses
import datetime as dt
import hashlib
import io
import json
import os
import pathlib
import platform
import re
import shutil
import statistics
import subprocess
import tempfile
import time
import tomllib
import zipfile
from typing import Any

SCHEMA = "frost-bench-standard-v2"
STANDARD_SCENARIOS = ("clean", "noop", "incremental_leaf", "hot_header", "cache_hit_rebuild")
SUPPORTED_TOOLS = ("ninja", "make", "frost", "bazel")
JAVA_SCHEMA = "frost-bench-java-v1"
JAVA_SCENARIOS = ("clean", "noop", "incremental_leaf")
JAVA_TOOLS = (
    "frost-unit",
    "frost-batch",
    "gradle",
    "maven",
    "frost-jar",
    "gradle-jar",
    "maven-jar",
)
JAVA_DEFAULT_TOOLS = ("frost-unit", "frost-batch", "gradle", "maven")
RUST_SCHEMA = "frost-bench-rust-v1"
RUST_SCENARIOS = ("clean", "noop", "incremental_leaf")
RUST_TOOLS = ("frost", "cargo")
GO_SCHEMA = "frost-bench-go-v1"
GO_SCENARIOS = ("clean", "noop", "incremental_leaf")
GO_TOOLS = ("frost-native", "frost-go", "go")
TYPESCRIPT_SCHEMA = "frost-bench-typescript-v1"
TYPESCRIPT_SCENARIOS = ("clean", "noop", "incremental_leaf")
TYPESCRIPT_TOOLS = ("frost", "tsc")
TYPESCRIPT_PROJECTS_SCHEMA = "frost-bench-typescript-projects-v1"
PYTHON_SCHEMA = "frost-bench-python-wheel-v1"
PYTHON_SCENARIOS = ("clean", "noop", "incremental_leaf")
PYTHON_TOOLS = ("frost", "python-build", "uv")


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


def target_name(index: int) -> str:
    return f"node{index:05d}"


def java_class_name(index: int) -> str:
    return f"Class{index:05d}"


def rust_module_name(index: int) -> str:
    return f"module{index:05d}"


def go_file_stem(index: int) -> str:
    return f"value{index:05d}"


def typescript_module_name(index: int) -> str:
    return f"module{index:05d}"


def typescript_project_name(index: int) -> str:
    return f"project{index:03d}"


def python_module_name(index: int) -> str:
    return f"module{index:05d}"


def generate_workspace(root: pathlib.Path, size: int) -> None:
    if root.exists():
        shutil.rmtree(root)
    (root / "src").mkdir(parents=True)
    (root / "include").mkdir()
    (root / "out").mkdir()
    (root / "include/hot.h").write_text("HOT=1\n", encoding="utf-8")
    for index in range(size):
        (root / "src" / f"{target_name(index)}.txt").write_text(f"value={index}\n", encoding="utf-8")
    write_ninja(root, size)
    write_makefile(root, size)
    write_frost_toml(root, size)
    write_bazel(root, size)
    verify_generated_graphs(root, size)


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
        deps = [f"src/{name}.txt", "include/hot.h"]
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
        lines.append('cmd = "printf \'%s\\\\n\' ${out} > ${out}"')
        lines.append("deps = [" + ", ".join(json.dumps(dep) for dep in deps) + "]")
        lines.append(
            "inputs = ["
            + ", ".join(json.dumps(path) for path in [f"src/{name}.txt", "include/hot.h"])
            + "]"
        )
        lines.append(f"outputs = [{json.dumps(f'.frost/out/{name}.out')}]")
        lines.append("")
    (root / "frost.toml").write_text("\n".join(lines), encoding="utf-8")


def write_bazel(root: pathlib.Path, size: int) -> None:
    (root / "MODULE.bazel").write_text('module(name = "frost_bench")\n', encoding="utf-8")
    lines = ['package(default_visibility = ["//visibility:public"])', ""]
    for index in range(size):
        name = target_name(index)
        srcs = [f"src/{name}.txt", "include/hot.h"]
        if index > 0:
            srcs.insert(1, f":{target_name(index - 1)}")
        lines.extend(
            [
                "genrule(",
                f'    name = "{name}",',
                "    srcs = [" + ", ".join(json.dumps(src) for src in srcs) + "],",
                f'    outs = ["out/{name}.out"],',
                '    cmd = "printf \'%s\\\\n\' $@ > $@",',
                ")",
                "",
            ]
        )
    lines.extend(
        [
            "filegroup(",
            '    name = "all",',
            f'    srcs = [":{target_name(size - 1)}"],',
            ")",
            "",
        ]
    )
    (root / "BUILD.bazel").write_text("\n".join(lines), encoding="utf-8")


def verify_generated_graphs(root: pathlib.Path, size: int) -> None:
    with (root / "frost.toml").open("rb") as file:
        frost = tomllib.load(file)
    frost_targets = frost.get("target", {})
    frost_edges = sorted(
        (name, dependency)
        for name, target in frost_targets.items()
        for dependency in target.get("deps", [])
    )

    bazel_text = (root / "BUILD.bazel").read_text(encoding="utf-8")
    bazel_edges: list[tuple[str, str]] = []
    bazel_names: list[str] = []
    for block in re.findall(r"genrule\(\n(.*?)\n\)", bazel_text, flags=re.DOTALL):
        name_match = re.search(r'^\s*name = "([^"]+)",$', block, flags=re.MULTILINE)
        srcs_match = re.search(r"^\s*srcs = \[(.*)\],$", block, flags=re.MULTILINE)
        if name_match is None or srcs_match is None:
            raise RuntimeError("generated Bazel genrule is malformed")
        name = name_match.group(1)
        bazel_names.append(name)
        for dependency in re.findall(r'":(node[0-9]+)"', srcs_match.group(1)):
            bazel_edges.append((name, dependency))

    expected_edges = [
        (target_name(index), target_name(index - 1)) for index in range(1, size)
    ]
    expected_names = [target_name(index) for index in range(size)]
    if sorted(frost_targets) != expected_names:
        raise RuntimeError("generated Frost target set differs from the graph contract")
    if sorted(bazel_names) != expected_names:
        raise RuntimeError("generated Bazel target set differs from the graph contract")
    if frost_edges != expected_edges or sorted(bazel_edges) != expected_edges:
        raise RuntimeError("Frost and Bazel dependency edges are not equivalent")


def generate_java_workspace(root: pathlib.Path, size: int, tool: str) -> None:
    """Generate the same independent Java source set for each build frontend."""
    if root.exists():
        shutil.rmtree(root)
    source_root = root / "src/main/java/bench"
    source_root.mkdir(parents=True)
    for index in range(size):
        name = java_class_name(index)
        (source_root / f"{name}.java").write_text(
            "\n".join(
                [
                    "package bench;",
                    "",
                    f"public final class {name} {{",
                    f"    private {name}() {{}}",
                    f"    public static int value() {{ return {index}; }}",
                    "}",
                    "",
                ]
            ),
            encoding="utf-8",
        )

    if tool == "frost-unit":
        write_java_frost(root, size, unit_partitioned=True)
    elif tool == "frost-batch":
        write_java_frost(root, size, unit_partitioned=False)
    elif tool == "frost-jar":
        write_java_frost_jar(root)
    elif tool in ("gradle", "gradle-jar"):
        write_java_gradle(root)
    elif tool in ("maven", "maven-jar"):
        write_java_maven(root)
    else:
        raise ValueError(f"unsupported Java benchmark tool: {tool}")
    verify_java_workspace(root, size, tool)


def java_sources(size: int) -> list[str]:
    return [
        f"src/main/java/bench/{java_class_name(index)}.java"
        for index in range(size)
    ]


def java_outputs(size: int) -> list[str]:
    return [
        f".frost/out/${{config}}/classes/bench/{java_class_name(index)}.class"
        for index in range(size)
    ]


def write_java_frost(root: pathlib.Path, size: int, *, unit_partitioned: bool) -> None:
    names = [java_class_name(index).lower() for index in range(size)]
    lines = [
        "[workspace]",
        "default_targets = [" + ", ".join(json.dumps(name) for name in names) + "]"
        if unit_partitioned
        else 'default_targets = ["classes"]',
        "",
        "[toolchain.tools]",
        'javac = "javac"',
        "",
    ]
    if unit_partitioned:
        for index, name in enumerate(names):
            source = java_sources(size)[index]
            output = java_outputs(size)[index]
            lines.extend(
                [
                    f"[target.{name}]",
                    'kind = "command"',
                    'tool = "javac"',
                    f"inputs = [{json.dumps(source)}]",
                    f"outputs = [{json.dumps(output)}]",
                    (
                        'args = ["--release", "21", "-encoding", "UTF-8", "-g", '
                        f'"-d", ".frost/out/${{config}}/classes", {json.dumps(source)}]'
                    ),
                    "",
                ]
            )
    else:
        lines.extend(
            [
                "[target.classes]",
                'kind = "command"',
                'tool = "javac"',
                "inputs = [" + ", ".join(json.dumps(path) for path in java_sources(size)) + "]",
                "outputs = [" + ", ".join(json.dumps(path) for path in java_outputs(size)) + "]",
                (
                    'args = ["--release", "21", "-encoding", "UTF-8", "-g", '
                    '"-d", ".frost/out/${config}/classes", "${in}"]'
                ),
                "",
            ]
        )
    (root / "frost.toml").write_text("\n".join(lines), encoding="utf-8")


def write_java_frost_jar(root: pathlib.Path) -> None:
    frost = tool_specs(["frost"])[0]
    # Workspace generation is also used to inspect the benchmark contract in
    # environments where the Rust binary has not been built yet (notably the
    # independent Python CI job).  The measurement layer already records a
    # missing executable as skipped; keep generation hermetic by emitting the
    # normal PATH-resolved command in that case.
    pack_jar = frost.argv[0] if frost.argv else "frost"
    (root / "frost.toml").write_text(
        "\n".join(
            [
                "[workspace]",
                'default_targets = ["archive"]',
                "",
                "[toolchain.tools]",
                'javac = "javac"',
                f"pack_jar = {json.dumps(pack_jar)}",
                "",
                "[target.archive]",
                'kind = "command"',
                'tool = "javac"',
                'inputs = ["src/main/java/**/*.java"]',
                'outputs = [".frost/out/${config}/java-bench.jar"]',
                'clean_dirs = [".frost/tmp/${config}/java-classes"]',
                (
                    'args = ["--release", "21", "-encoding", "UTF-8", "-g", '
                    '"-d", "${clean_dir}", "${in}"]'
                ),
                (
                    'steps = [{ tool = "pack_jar", args = ["pack-jar", "--input", '
                    '"${clean_dir}", "--output", "${out}"] }]'
                ),
                "sandbox = false",
                "",
            ]
        ),
        encoding="utf-8",
    )


def write_java_gradle(root: pathlib.Path) -> None:
    (root / "settings.gradle").write_text(
        "rootProject.name = 'frost-java-bench'\n",
        encoding="utf-8",
    )
    (root / "build.gradle").write_text(
        "\n".join(
            [
                "plugins {",
                "    id 'java'",
                "}",
                "",
                "tasks.withType(JavaCompile).configureEach {",
                "    options.release = 21",
                "    options.encoding = 'UTF-8'",
                "    options.debug = true",
                "    options.incremental = true",
                "}",
                "",
            ]
        ),
        encoding="utf-8",
    )
    # Keep the normal long-lived daemon and explicitly exercise Gradle's
    # configuration cache. The build cache stays off so a clean sample really
    # invokes javac instead of restoring classes from a separate cache.
    (root / "gradle.properties").write_text(
        "\n".join(
            [
                "org.gradle.daemon=true",
                "org.gradle.configuration-cache=true",
                "org.gradle.caching=false",
                "org.gradle.console=plain",
                "",
            ]
        ),
        encoding="utf-8",
    )


def write_java_maven(root: pathlib.Path) -> None:
    (root / "pom.xml").write_text(
        "\n".join(
            [
                '<?xml version="1.0" encoding="UTF-8"?>',
                (
                    '<project xmlns="http://maven.apache.org/POM/4.0.0" '
                    'xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"'
                ),
                (
                    '         xsi:schemaLocation="http://maven.apache.org/POM/4.0.0 '
                    'https://maven.apache.org/xsd/maven-4.0.0.xsd">'
                ),
                "  <modelVersion>4.0.0</modelVersion>",
                "  <groupId>dev.frostbuild.bench</groupId>",
                "  <artifactId>java-bench</artifactId>",
                "  <version>1.0.0</version>",
                "  <properties>",
                "    <project.build.sourceEncoding>UTF-8</project.build.sourceEncoding>",
                "    <maven.compiler.release>21</maven.compiler.release>",
                "  </properties>",
                "  <build>",
                "    <plugins>",
                "      <plugin>",
                "        <groupId>org.apache.maven.plugins</groupId>",
                "        <artifactId>maven-compiler-plugin</artifactId>",
                "        <version>3.13.0</version>",
                "      </plugin>",
                "    </plugins>",
                "  </build>",
                "</project>",
                "",
            ]
        ),
        encoding="utf-8",
    )


def verify_java_workspace(root: pathlib.Path, size: int, tool: str) -> None:
    actual_sources = sorted(
        path.relative_to(root).as_posix()
        for path in root.glob("src/main/java/bench/Class*.java")
    )
    if actual_sources != java_sources(size):
        raise RuntimeError(f"{tool} Java source set differs from the benchmark contract")
    if tool.startswith("frost-"):
        with (root / "frost.toml").open("rb") as file:
            manifest = tomllib.load(file)
        targets = manifest.get("target", {})
        expected = size if tool == "frost-unit" else 1
        if len(targets) != expected:
            raise RuntimeError(f"{tool} has {len(targets)} actions, expected {expected}")
        declared_outputs = sorted(
            output
            for target in targets.values()
            for output in target.get("outputs", [])
        )
        expected_outputs = (
            [".frost/out/${config}/java-bench.jar"]
            if tool == "frost-jar"
            else sorted(java_outputs(size))
        )
        if declared_outputs != expected_outputs:
            raise RuntimeError(f"{tool} output set differs from the benchmark contract")


def rust_sources(size: int) -> list[str]:
    return ["src/main.rs", *[f"src/{rust_module_name(index)}.rs" for index in range(size)]]


def rust_binary_name() -> str:
    return "frost-rust-bench.exe" if os.name == "nt" else "frost-rust-bench"


def write_rust_module(path: pathlib.Path, value: int) -> None:
    path.write_text(
        f"pub fn value() -> u64 {{ {value} }}\n",
        encoding="utf-8",
    )


def write_rust_main(root: pathlib.Path, size: int) -> None:
    declarations = [f"mod {rust_module_name(index)};" for index in range(size)]
    values = [f"        {rust_module_name(index)}::value()," for index in range(size)]
    (root / "src/main.rs").write_text(
        "\n".join(
            [
                *declarations,
                "",
                "fn main() {",
                "    let total: u64 = [",
                *values,
                "    ]",
                "    .into_iter()",
                "    .sum();",
                '    println!("{total}");',
                "}",
                "",
            ]
        ),
        encoding="utf-8",
    )


def write_rust_frost(root: pathlib.Path, rustc: str) -> None:
    output = f".frost/out/${{config}}/{rust_binary_name()}"
    (root / "frost.toml").write_text(
        "\n".join(
            [
                "[workspace]",
                'default_targets = ["binary"]',
                "",
                "[toolchain.tools]",
                f"rustc = {json.dumps(rustc)}",
                "",
                "[target.binary]",
                'kind = "command"',
                'tool = "rustc"',
                'inputs = ["src/**/*.rs"]',
                f"outputs = [{json.dumps(output)}]",
                (
                    'args = ["--crate-name", "frost_rust_bench", "--edition", "2021", '
                    '"-C", "opt-level=0", "-C", "debuginfo=0", '
                    '"-C", "strip=none", "-C", "debug-assertions=on", '
                    '"-C", "overflow-checks=on", "-C", "codegen-units=256", '
                    '"-C", "embed-bitcode=no", "-C", "panic=unwind", '
                    '"-C", "lto=off", '
                    '"-C", "incremental=.frost/rust/${config}/incremental", '
                    '"src/main.rs", "-o", "${out}"]'
                ),
                "sandbox = false",
                "",
            ]
        ),
        encoding="utf-8",
    )


def write_rust_cargo(root: pathlib.Path) -> None:
    (root / "Cargo.toml").write_text(
        "\n".join(
            [
                "[package]",
                'name = "frost-rust-bench"',
                'version = "0.0.0"',
                'edition = "2021"',
                "",
                "[profile.dev]",
                "opt-level = 0",
                "debug = 0",
                'strip = "none"',
                "debug-assertions = true",
                "overflow-checks = true",
                "lto = false",
                'panic = "unwind"',
                "incremental = true",
                "codegen-units = 256",
                "",
            ]
        ),
        encoding="utf-8",
    )
    (root / "Cargo.lock").write_text(
        "\n".join(
            [
                "# This file is automatically @generated by Cargo.",
                "# It is not intended for manual editing.",
                "version = 4",
                "",
                "[[package]]",
                'name = "frost-rust-bench"',
                'version = "0.0.0"',
                "",
            ]
        ),
        encoding="utf-8",
    )


def generate_rust_workspace(
    root: pathlib.Path,
    size: int,
    tool: str,
    rustc: str = "rustc",
) -> None:
    """Generate the same single-crate Rust source graph for both frontends."""
    if root.exists():
        shutil.rmtree(root)
    (root / "src").mkdir(parents=True)
    for index in range(size):
        write_rust_module(root / "src" / f"{rust_module_name(index)}.rs", index)
    write_rust_main(root, size)
    if tool == "frost":
        write_rust_frost(root, rustc)
    elif tool == "cargo":
        write_rust_cargo(root)
    else:
        raise ValueError(f"unsupported Rust benchmark tool: {tool}")
    verify_rust_workspace(root, size, tool)


def verify_rust_workspace(root: pathlib.Path, size: int, tool: str) -> None:
    actual_sources = sorted(
        path.relative_to(root).as_posix()
        for path in (root / "src").glob("*.rs")
    )
    if actual_sources != sorted(rust_sources(size)):
        raise RuntimeError(f"{tool} Rust source set differs from the benchmark contract")
    if tool == "frost":
        with (root / "frost.toml").open("rb") as file:
            manifest = tomllib.load(file)
        targets = manifest.get("target", {})
        if list(targets) != ["binary"]:
            raise RuntimeError("Frost Rust benchmark must have exactly one crate action")
        target = targets["binary"]
        if target.get("inputs") != ["src/**/*.rs"]:
            raise RuntimeError("Frost Rust benchmark input glob differs from the crate contract")
        expected = [f".frost/out/${{config}}/{rust_binary_name()}"]
        if target.get("outputs") != expected:
            raise RuntimeError("Frost Rust benchmark output differs from the crate contract")


def go_sources(size: int) -> list[str]:
    return ["main.go", *[f"{go_file_stem(index)}.go" for index in range(size)]]


def go_binary_name() -> str:
    return "frost-go-bench.exe" if os.name == "nt" else "frost-go-bench"


def go_language_version(go_info: dict[str, str]) -> str:
    version = go_info.get("GOVERSION", "go1.26").removeprefix("go")
    parts = version.split(".")
    return ".".join(parts[:2])


def write_go_value(path: pathlib.Path, name: str, value: int) -> None:
    path.write_text(
        "\n".join(
            [
                "package main",
                "",
                f"func {name}() uint64 {{ return {value} }}",
                "",
            ]
        ),
        encoding="utf-8",
    )


def write_go_main(root: pathlib.Path, size: int) -> None:
    additions = [
        f"\ttotal += {go_file_stem(index)}()"
        for index in range(size)
    ]
    (root / "main.go").write_text(
        "\n".join(
            [
                "package main",
                "",
                "func main() {",
                "\ttotal := uint64(0)",
                *additions,
                "\tprintln(total)",
                "}",
                "",
            ]
        ),
        encoding="utf-8",
    )


def go_process_environment(
    root: pathlib.Path,
    go_info: dict[str, str],
    *,
    cache_name: str = ".go-cache",
) -> dict[str, str]:
    environment = os.environ.copy()
    environment.pop("GOFLAGS", None)
    environment.update(
        {
            "CGO_ENABLED": "0",
            "GOARCH": go_info.get("GOARCH", platform.machine()),
            "GOAMD64": go_info.get("GOAMD64", "v1"),
            "GOCACHE": (root / cache_name).resolve().as_posix(),
            "GOFLAGS": "",
            "GOOS": go_info.get("GOOS", platform.system().lower()),
            "GOROOT": go_info.get("GOROOT", ""),
            "GOTOOLCHAIN": "local",
        }
    )
    return environment


def prepare_go_runtime_seed(
    root: pathlib.Path,
    go_executable: str,
    go_info: dict[str, str],
) -> list[tuple[str, pathlib.Path]]:
    seed = root / ".go-cache-seed"
    seed.mkdir(parents=True, exist_ok=True)
    environment = go_process_environment(
        root,
        go_info,
        cache_name=".go-cache-seed",
    )
    completed = subprocess.run(
        [
            go_executable,
            "list",
            "-export",
            "-deps",
            "-f",
            "{{if .Export}}{{.ImportPath}}\t{{.Export}}{{end}}",
            "runtime",
        ],
        cwd=root,
        env=environment,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        check=False,
    )
    if completed.returncode != 0:
        tail = "\n".join(completed.stdout.splitlines()[-30:])
        raise RuntimeError(f"failed to prepare isolated Go runtime cache:\n{tail}")
    exports: list[tuple[str, pathlib.Path]] = []
    for line in completed.stdout.splitlines():
        if not line.strip():
            continue
        import_path, separator, export = line.partition("\t")
        if not separator or not pathlib.Path(export).is_file():
            raise RuntimeError(f"malformed Go export record: {line!r}")
        exports.append((import_path, pathlib.Path(export)))
    if not any(import_path == "runtime" for import_path, _ in exports):
        raise RuntimeError("Go runtime export closure did not contain runtime")
    return exports


def reset_go_project_cache(root: pathlib.Path) -> None:
    seed = root / ".go-cache-seed"
    active = root / ".go-cache"
    if active.exists():
        shutil.rmtree(active)
    shutil.copytree(seed, active, copy_function=os.link)


def write_go_sdk(
    root: pathlib.Path,
    exports: list[tuple[str, pathlib.Path]],
    go_info: dict[str, str],
) -> None:
    sdk = root / ".go-sdk"
    packages = sdk / "pkg"
    packages.mkdir(parents=True)
    importcfg = []
    for index, (import_path, export) in enumerate(exports):
        relative = pathlib.Path(".go-sdk/pkg") / f"{index:04d}.a"
        shutil.copy2(export, root / relative)
        importcfg.append(f"packagefile {import_path}={relative.as_posix()}")
    (sdk / "compile.importcfg").write_text("", encoding="utf-8")
    importcfg.append(f"modinfo {go_quoted_bytes(go_module_info(go_info))}")
    (sdk / "link.importcfg").write_text(
        "\n".join(importcfg) + "\n",
        encoding="utf-8",
    )


def go_module_info(go_info: dict[str, str]) -> bytes:
    text = "\n".join(
        [
            "path\texample.com/frost-go-bench",
            "mod\texample.com/frost-go-bench\t(devel)\t",
            "build\t-buildmode=exe",
            "build\t-compiler=gc",
            "build\t-ldflags=-buildid=",
            "build\tCGO_ENABLED=0",
            f"build\tGOARCH={go_info['GOARCH']}",
            f"build\tGOOS={go_info['GOOS']}",
            f"build\tGOAMD64={go_info.get('GOAMD64', 'v1')}",
            "",
        ]
    )
    return (
        bytes.fromhex("3077af0c9274080241e1c107e6d618e6")
        + text.encode()
        + bytes.fromhex("f932433186182072008242104116d8f2")
    )


def go_quoted_bytes(value: bytes) -> str:
    pieces = ['"']
    escapes = {
        0x09: r"\t",
        0x0A: r"\n",
        0x0D: r"\r",
        0x22: r"\"",
        0x5C: r"\\",
    }
    for byte in value:
        if byte in escapes:
            pieces.append(escapes[byte])
        elif 0x20 <= byte <= 0x7E:
            pieces.append(chr(byte))
        else:
            pieces.append(f"\\x{byte:02x}")
    pieces.append('"')
    return "".join(pieces)


def go_static_environment(root: pathlib.Path, go_info: dict[str, str]) -> str:
    values = {
        "CGO_ENABLED": "0",
        "GOARCH": go_info.get("GOARCH", platform.machine()),
        "GOAMD64": go_info.get("GOAMD64", "v1"),
        "GOCACHE": (root / ".go-cache").resolve().as_posix(),
        "GOFLAGS": "",
        "GOOS": go_info.get("GOOS", platform.system().lower()),
        "GOROOT": go_info.get("GOROOT", ""),
        "GOTOOLCHAIN": "local",
    }
    return "{ " + ", ".join(
        f"{name} = {json.dumps(value)}"
        for name, value in sorted(values.items())
    ) + " }"


def write_go_frost_wrapper(
    root: pathlib.Path,
    go_executable: str,
    go_info: dict[str, str],
    jobs: int,
) -> None:
    output = f".frost/out/${{config}}/{go_binary_name()}"
    (root / "frost.toml").write_text(
        "\n".join(
            [
                "[workspace]",
                'default_targets = ["binary"]',
                "",
                "[toolchain.tools]",
                f"go = {json.dumps(go_executable)}",
                "",
                "[target.binary]",
                'kind = "command"',
                'tool = "go"',
                'inputs = ["go.mod", "*.go"]',
                f"outputs = [{json.dumps(output)}]",
                (
                    'args = ["build", "-buildvcs=false", "-pgo=off", '
                    f'"-p", "{max(1, jobs)}", "-ldflags=-buildid=", '
                    '"-o", "${out}", "."]'
                ),
                f"env = {go_static_environment(root, go_info)}",
                "sandbox = false",
                "",
            ]
        ),
        encoding="utf-8",
    )


def write_go_frost_native(
    root: pathlib.Path,
    go_info: dict[str, str],
    jobs: int,
) -> None:
    sources = go_sources_from_root(root)
    output = f".frost/out/${{config}}/{go_binary_name()}"
    compile_tool = pathlib.Path(go_info["GOTOOLDIR"]) / (
        "compile.exe" if os.name == "nt" else "compile"
    )
    link_tool = pathlib.Path(go_info["GOTOOLDIR"]) / (
        "link.exe" if os.name == "nt" else "link"
    )
    compile_args = [
        "-o",
        "${clean_dir}/main.a",
        "-p",
        "main",
        "-lang",
        f"go{go_language_version(go_info)}",
        "-complete",
        "-goversion",
        go_info["GOVERSION"],
        "-c",
        str(max(1, jobs)),
        "-nolocalimports",
        "-importcfg",
        ".go-sdk/compile.importcfg",
        "-pack",
        *sources,
    ]
    link_args = [
        "-o",
        "${out}",
        "-importcfg",
        ".go-sdk/link.importcfg",
        "-buildmode=exe",
        "-buildid=",
        "${clean_dir}/main.a",
    ]
    environment = {
        "CGO_ENABLED": "0",
        "GOARCH": go_info["GOARCH"],
        "GOAMD64": go_info.get("GOAMD64", "v1"),
        "GOOS": go_info["GOOS"],
        "GOROOT": go_info["GOROOT"],
    }
    (root / "frost.toml").write_text(
        "\n".join(
            [
                "[workspace]",
                'default_targets = ["binary"]',
                "",
                "[toolchain.tools]",
                f"compile = {json.dumps(compile_tool.as_posix())}",
                f"link = {json.dumps(link_tool.as_posix())}",
                "",
                "[target.binary]",
                'kind = "command"',
                'tool = "compile"',
                'inputs = ["go.mod", "*.go", ".go-sdk/**/*"]',
                f"outputs = [{json.dumps(output)}]",
                'clean_dirs = [".frost/tmp/${config}/go"]',
                f"args = {json.dumps(compile_args)}",
                (
                    "steps = [{ tool = \"link\", args = "
                    f"{json.dumps(link_args)} }}]"
                ),
                (
                    "env = { "
                    + ", ".join(
                        f"{name} = {json.dumps(value)}"
                        for name, value in sorted(environment.items())
                    )
                    + " }"
                ),
                "sandbox = false",
                "",
            ]
        ),
        encoding="utf-8",
    )


def write_go_module(root: pathlib.Path, go_info: dict[str, str]) -> None:
    (root / "go.mod").write_text(
        "\n".join(
            [
                "module example.com/frost-go-bench",
                "",
                f"go {go_language_version(go_info)}",
                "",
            ]
        ),
        encoding="utf-8",
    )


def go_sources_from_root(root: pathlib.Path) -> list[str]:
    return sorted(path.name for path in root.glob("*.go"))


def generate_go_workspace(
    root: pathlib.Path,
    size: int,
    tool: str,
    jobs: int,
    go_executable: str | None,
    go_info: dict[str, str] | None,
) -> None:
    """Generate one equal, dependency-free Go main package per frontend."""
    if root.exists():
        shutil.rmtree(root)
    root.mkdir(parents=True)
    info = go_info or {
        "GOARCH": platform.machine(),
        "GOAMD64": "v1",
        "GOOS": platform.system().lower(),
        "GOROOT": "",
        "GOTOOLDIR": "",
        "GOVERSION": "go1.26",
    }
    write_go_module(root, info)
    write_go_main(root, size)
    for index in range(size):
        write_go_value(
            root / f"{go_file_stem(index)}.go",
            go_file_stem(index),
            index,
        )
    (root / ".frostignore").write_text(
        ".go-cache/\n.go-cache-seed/\n",
        encoding="utf-8",
    )

    exports: list[tuple[str, pathlib.Path]] = []
    if go_executable and go_info:
        exports = prepare_go_runtime_seed(root, go_executable, go_info)
    if tool == "frost-native":
        if exports:
            write_go_sdk(root, exports, info)
            shutil.rmtree(root / ".go-cache-seed")
        else:
            (root / ".go-sdk/pkg").mkdir(parents=True)
            (root / ".go-sdk/compile.importcfg").write_text("", encoding="utf-8")
            (root / ".go-sdk/link.importcfg").write_text("", encoding="utf-8")
        write_go_frost_native(root, info, jobs)
    elif tool == "frost-go":
        if exports:
            reset_go_project_cache(root)
        write_go_frost_wrapper(root, go_executable or "go", info, jobs)
    elif tool == "go":
        if exports:
            reset_go_project_cache(root)
    else:
        raise ValueError(f"unsupported Go benchmark tool: {tool}")
    verify_go_workspace(root, size, tool)


def verify_go_workspace(root: pathlib.Path, size: int, tool: str) -> None:
    actual_sources = sorted(path.name for path in root.glob("*.go"))
    if actual_sources != sorted(go_sources(size)):
        raise RuntimeError(f"{tool} Go source set differs from the benchmark contract")
    if tool.startswith("frost-"):
        with (root / "frost.toml").open("rb") as file:
            manifest = tomllib.load(file)
        targets = manifest.get("target", {})
        if list(targets) != ["binary"]:
            raise RuntimeError(f"{tool} must have exactly one Go package action")
        expected = [f".frost/out/${{config}}/{go_binary_name()}"]
        if targets["binary"].get("outputs") != expected:
            raise RuntimeError(f"{tool} output differs from the Go binary contract")


def typescript_sources(size: int) -> list[str]:
    return [
        "src/main.ts",
        *[
            f"src/{typescript_module_name(index)}.ts"
            for index in range(size)
        ],
    ]


def typescript_outputs(size: int, prefix: str) -> list[str]:
    return [
        f"{prefix}/main.js",
        *[
            f"{prefix}/{typescript_module_name(index)}.js"
            for index in range(size)
        ],
    ]


def typescript_frost_outputs(size: int) -> list[str]:
    return [
        *typescript_outputs(size, ".frost/out/${config}/js"),
        ".frost/typescript/${config}/project.tsbuildinfo",
    ]


def typescript_solution_outputs(modules: int, prefix: str) -> list[str]:
    javascript = typescript_outputs(modules, prefix)
    declarations = [str(pathlib.PurePosixPath(path).with_suffix(".d.ts")) for path in javascript]
    return [*javascript, *declarations]


def write_typescript_module(path: pathlib.Path, value: int) -> None:
    path.write_text(
        f"export function value(): number {{ return {value}; }}\n",
        encoding="utf-8",
    )


def write_typescript_main(root: pathlib.Path, size: int) -> None:
    imports = [
        (
            f'import {{ value as {typescript_module_name(index)}Value }} '
            f'from "./{typescript_module_name(index)}.js";'
        )
        for index in range(size)
    ]
    additions = [
        f"total += {typescript_module_name(index)}Value();"
        for index in range(size)
    ]
    (root / "src/main.ts").write_text(
        "\n".join(
            [
                *imports,
                "",
                "let total: number = 0;",
                *additions,
                "console.log(total);",
                "",
            ]
        ),
        encoding="utf-8",
    )


def write_typescript_project(root: pathlib.Path, tool: str) -> None:
    if tool == "frost":
        out_dir = ".frost/out/debug/js"
        build_info = ".frost/typescript/debug/project.tsbuildinfo"
    elif tool == "tsc":
        out_dir = "target/js"
        build_info = "target/project.tsbuildinfo"
    else:
        raise ValueError(tool)
    config = {
        "compilerOptions": {
            "declaration": False,
            "incremental": True,
            "module": "NodeNext",
            "moduleResolution": "NodeNext",
            "newLine": "lf",
            "noEmitOnError": True,
            "outDir": out_dir,
            "rootDir": "src",
            "skipLibCheck": False,
            "sourceMap": False,
            "strict": True,
            "target": "ES2022",
            "tsBuildInfoFile": build_info,
            "types": [],
        },
        "include": ["src/**/*.ts"],
    }
    (root / "tsconfig.json").write_text(
        json.dumps(config, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    (root / "package.json").write_text(
        json.dumps(
            {
                "name": "frost-typescript-bench",
                "private": True,
                "type": "module",
                "version": "0.0.0",
            },
            indent=2,
            sort_keys=True,
        )
        + "\n",
        encoding="utf-8",
    )


def write_typescript_frost(
    root: pathlib.Path,
    size: int,
    tsc: str,
    checkers: int,
) -> None:
    outputs = typescript_frost_outputs(size)
    (root / "frost.toml").write_text(
        "\n".join(
            [
                "[workspace]",
                'default_targets = ["javascript"]',
                "",
                "[toolchain.tools]",
                f"tsc = {json.dumps(tsc)}",
                "",
                "[target.javascript]",
                'kind = "command"',
                'tool = "tsc"',
                (
                    'inputs = ["package.json", "tsconfig.json", '
                    '"src/**/*.ts", "typescript-sdk/lib/lib*.d.ts"]'
                ),
                "outputs = [" + ", ".join(json.dumps(path) for path in outputs) + "]",
                (
                    'args = ["--project", "tsconfig.json", "--checkers", '
                    f'"{max(1, checkers)}", "--pretty", "false"]'
                ),
                "preserve_outputs = true",
                "sandbox = false",
                "",
            ]
        ),
        encoding="utf-8",
    )


def bundle_typescript_toolchain(root: pathlib.Path, tsc: str) -> None:
    source = pathlib.Path(tsc).resolve()
    if not source.is_file():
        raise RuntimeError(f"TypeScript compiler does not exist: {source}")
    if source.read_bytes()[:4] != b"\x7fELF":
        raise RuntimeError(
            "the TypeScript benchmark requires the native TypeScript 7 "
            "compiler so its runtime closure can be declared exactly"
        )
    destination = root / "typescript-sdk/lib"
    shutil.copytree(source.parent, destination)
    copied = destination / source.name
    if copied.name != "tsc":
        shutil.copy2(copied, destination / "tsc")


def generate_typescript_workspace(
    root: pathlib.Path,
    size: int,
    tool: str,
    tsc: str | None = None,
    checkers: int = 1,
) -> None:
    if root.exists():
        shutil.rmtree(root)
    (root / "src").mkdir(parents=True)
    for index in range(size):
        write_typescript_module(
            root / "src" / f"{typescript_module_name(index)}.ts",
            index,
        )
    write_typescript_main(root, size)
    write_typescript_project(root, tool)
    bundled_tsc = "typescript-sdk/lib/tsc"
    if tsc is not None:
        bundle_typescript_toolchain(root, tsc)
    if tool == "frost":
        write_typescript_frost(root, size, bundled_tsc, checkers)
    verify_typescript_workspace(root, size, tool)


def verify_typescript_workspace(root: pathlib.Path, size: int, tool: str) -> None:
    sources = sorted(
        path.relative_to(root).as_posix()
        for path in (root / "src").glob("*.ts")
    )
    if sources != sorted(typescript_sources(size)):
        raise RuntimeError(f"{tool} TypeScript source set differs from contract")
    if tool == "frost":
        with (root / "frost.toml").open("rb") as file:
            manifest = tomllib.load(file)
        outputs = manifest["target"]["javascript"]["outputs"]
        if outputs != typescript_frost_outputs(size):
            raise RuntimeError("Frost TypeScript output set differs from contract")


def typescript_project_output_prefix(tool: str, project: str) -> str:
    if tool == "frost":
        return f".frost/out/debug/js/{project}"
    if tool == "tsc":
        return f"target/js/{project}"
    raise ValueError(tool)


def write_typescript_solution_project(
    root: pathlib.Path,
    tool: str,
    project: str,
    modules: int,
) -> None:
    directory = root / "projects" / project
    (directory / "src").mkdir(parents=True)
    for index in range(modules):
        write_typescript_module(
            directory / "src" / f"{typescript_module_name(index)}.ts",
            index,
        )
    write_typescript_main(directory, modules)
    if tool == "frost":
        out_dir = f"../../.frost/out/debug/js/{project}"
        build_info = f"../../.frost/typescript/debug/{project}.tsbuildinfo"
    elif tool == "tsc":
        out_dir = f"../../target/js/{project}"
        build_info = f"../../target/{project}.tsbuildinfo"
    else:
        raise ValueError(tool)
    config = {
        "compilerOptions": {
            "composite": True,
            "declaration": True,
            "incremental": True,
            "module": "NodeNext",
            "moduleResolution": "NodeNext",
            "newLine": "lf",
            "noEmitOnError": True,
            "outDir": out_dir,
            "rootDir": "src",
            "skipLibCheck": False,
            "sourceMap": False,
            "strict": True,
            "target": "ES2022",
            "tsBuildInfoFile": build_info,
            "types": [],
        },
        "include": ["src/**/*.ts"],
    }
    (directory / "tsconfig.json").write_text(
        json.dumps(config, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )


def write_typescript_solution_frost(
    root: pathlib.Path,
    projects: int,
    modules: int,
    checkers: int,
) -> None:
    defaults = [typescript_project_name(index) for index in range(projects)]
    lines = [
        "[workspace]",
        "default_targets = [" + ", ".join(json.dumps(name) for name in defaults) + "]",
        "",
        "[toolchain.tools]",
        'tsc = "typescript-sdk/lib/tsc"',
        "",
    ]
    for project in defaults:
        prefix = f".frost/out/${{config}}/js/{project}"
        outputs = [
            *typescript_solution_outputs(modules, prefix),
            f".frost/typescript/${{config}}/{project}.tsbuildinfo",
        ]
        lines.extend(
            [
                f"[target.{project}]",
                'kind = "command"',
                'tool = "tsc"',
                (
                    f'inputs = ["projects/{project}/tsconfig.json", '
                    f'"projects/{project}/src/**/*.ts", '
                    '"package.json", "typescript-sdk/lib/lib*.d.ts"]'
                ),
                "outputs = [" + ", ".join(json.dumps(path) for path in outputs) + "]",
                (
                    f'args = ["--project", "projects/{project}/tsconfig.json", '
                    f'"--checkers", "{max(1, checkers)}", "--pretty", "false"]'
                ),
                "preserve_outputs = true",
                "sandbox = false",
                "",
            ]
        )
    (root / "frost.toml").write_text("\n".join(lines), encoding="utf-8")


def generate_typescript_solution(
    root: pathlib.Path,
    projects: int,
    modules: int,
    tool: str,
    tsc: str,
    checkers: int,
) -> None:
    if root.exists():
        shutil.rmtree(root)
    root.mkdir(parents=True)
    bundle_typescript_toolchain(root, tsc)
    (root / "package.json").write_text(
        json.dumps(
            {
                "name": "frost-typescript-projects-bench",
                "private": True,
                "type": "module",
                "version": "0.0.0",
            },
            indent=2,
            sort_keys=True,
        )
        + "\n",
        encoding="utf-8",
    )
    names = [typescript_project_name(index) for index in range(projects)]
    for project in names:
        write_typescript_solution_project(root, tool, project, modules)
    (root / "tsconfig.json").write_text(
        json.dumps(
            {
                "files": [],
                "references": [
                    {"path": f"./projects/{project}"} for project in names
                ],
            },
            indent=2,
            sort_keys=True,
        )
        + "\n",
        encoding="utf-8",
    )
    if tool == "frost":
        write_typescript_solution_frost(root, projects, modules, checkers)


def verify_typescript_solution_output(
    root: pathlib.Path,
    spec: ToolSpec,
    projects: int,
    modules: int,
    expected_totals: list[int],
    node: str,
) -> dict[str, Any]:
    hasher = hashlib.sha256()
    semantic = hashlib.sha256()
    artifact_bytes = 0
    file_count = 0
    for project_index in range(projects):
        project = typescript_project_name(project_index)
        directory = root / typescript_project_output_prefix(spec.name, project)
        outputs = sorted(path for path in directory.iterdir() if path.is_file())
        expected_names = sorted(
            pathlib.Path(path).name for path in typescript_solution_outputs(modules, "")
        )
        if [path.name for path in outputs] != expected_names:
            raise RuntimeError(
                f"{spec.name} {project} emitted an unexpected JavaScript set"
            )
        completed = subprocess.run(
            [node, (directory / "main.js").as_posix()],
            cwd=root,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        expected_stdout = f"{expected_totals[project_index]}\n"
        if (
            completed.returncode != 0
            or completed.stdout != expected_stdout
            or completed.stderr
        ):
            raise RuntimeError(
                f"{spec.name} {project} execution mismatch: "
                f"exit={completed.returncode}, stdout={completed.stdout!r}, "
                f"stderr={completed.stderr!r}"
            )
        semantic.update(project.encode())
        semantic.update(b"\0")
        semantic.update(expected_stdout.encode())
        semantic.update(b"\0")
        for path in outputs:
            payload = path.read_bytes()
            artifact_bytes += len(payload)
            file_count += 1
            hasher.update(project.encode())
            hasher.update(b"/")
            hasher.update(path.name.encode())
            hasher.update(b"\0")
            hasher.update(payload)
            hasher.update(b"\0")
    return {
        "artifact_bytes": artifact_bytes,
        "artifact_sha256": hasher.hexdigest(),
        "file_count": file_count,
        "semantic_sha256": semantic.hexdigest(),
    }


PYTHON_DISTRIBUTION = "frost-python-bench"
PYTHON_IMPORT_PACKAGE = "frost_python_bench"
PYTHON_VERSION = "1.0.0"
PYTHON_WHEEL = f"{PYTHON_IMPORT_PACKAGE}-{PYTHON_VERSION}-py3-none-any.whl"


def write_python_module(path: pathlib.Path, value: int) -> None:
    path.write_text(f"VALUE = {value}\n", encoding="utf-8")


def write_python_package(root: pathlib.Path, size: int) -> None:
    package = root / "src" / PYTHON_IMPORT_PACKAGE
    package.mkdir(parents=True)
    for index in range(size):
        write_python_module(package / f"{python_module_name(index)}.py", index)
    imports = [
        f"from .{python_module_name(index)} import VALUE as VALUE_{index}"
        for index in range(size)
    ]
    values = ", ".join(f"VALUE_{index}" for index in range(size))
    (package / "__init__.py").write_text(
        "\n".join([*imports, "", f"def total():\n    return sum(({values},))", ""]),
        encoding="utf-8",
    )


def write_python_pyproject(root: pathlib.Path) -> None:
    (root / "pyproject.toml").write_text(
        "\n".join(
            [
                "[build-system]",
                'requires = ["setuptools>=77", "wheel"]',
                'build-backend = "setuptools.build_meta"',
                "",
                "[project]",
                f"name = {json.dumps(PYTHON_DISTRIBUTION)}",
                f"version = {json.dumps(PYTHON_VERSION)}",
                'requires-python = ">=3.9"',
                'description = "FrostBuild wheel benchmark fixture"',
                "",
                "[tool.setuptools]",
                'package-dir = {"" = "src"}',
                "",
                "[tool.setuptools.packages.find]",
                'where = ["src"]',
                "",
            ]
        ),
        encoding="utf-8",
    )


def write_python_frost(root: pathlib.Path, frost: str) -> None:
    (root / "frost.toml").write_text(
        "\n".join(
            [
                "[workspace]",
                'default_targets = ["wheel"]',
                "",
                "[toolchain.tools]",
                f"pack_wheel = {json.dumps(frost)}",
                "",
                "[target.wheel]",
                'kind = "command"',
                'tool = "pack_wheel"',
                (
                    'args = ["pack-wheel", "--input", "src", "--distribution", '
                    f'{json.dumps(PYTHON_DISTRIBUTION)}, "--version", '
                    f'{json.dumps(PYTHON_VERSION)}, "--output", "${{out}}"]'
                ),
                'inputs = ["pyproject.toml", "src/**/*.py"]',
                f'outputs = [".frost/out/${{config}}/{PYTHON_WHEEL}"]',
                "sandbox = false",
                "",
            ]
        ),
        encoding="utf-8",
    )


def generate_python_workspace(
    root: pathlib.Path,
    size: int,
    tool: str,
    frost: str,
) -> None:
    if root.exists():
        shutil.rmtree(root)
    root.mkdir(parents=True)
    write_python_package(root, size)
    write_python_pyproject(root)
    if tool == "frost":
        write_python_frost(root, frost)
    elif tool not in ("python-build", "uv"):
        raise ValueError(tool)
    expected = [
        f"src/{PYTHON_IMPORT_PACKAGE}/__init__.py",
        *[
            f"src/{PYTHON_IMPORT_PACKAGE}/{python_module_name(index)}.py"
            for index in range(size)
        ],
    ]
    actual = sorted(
        path.relative_to(root).as_posix()
        for path in (root / "src").rglob("*.py")
    )
    if actual != sorted(expected):
        raise RuntimeError(f"{tool} Python source set differs from the contract")


def python_wheel_path(root: pathlib.Path, spec: ToolSpec) -> pathlib.Path:
    if spec.name == "frost":
        return root / ".frost/out/debug" / PYTHON_WHEEL
    candidates = sorted((root / "dist").glob("*.whl"))
    if len(candidates) != 1:
        raise RuntimeError(f"{spec.name} produced {len(candidates)} wheels, expected one")
    if candidates[0].name != PYTHON_WHEEL:
        raise RuntimeError(
            f"{spec.name} produced {candidates[0].name!r}, expected {PYTHON_WHEEL!r}"
        )
    return candidates[0]


def verify_python_wheel(
    root: pathlib.Path,
    spec: ToolSpec,
    size: int,
    expected_total: int,
    python: str,
) -> dict[str, Any]:
    wheel = python_wheel_path(root, spec)
    if not wheel.is_file():
        raise RuntimeError(f"{spec.name} did not produce {wheel}")
    expected_sources = {
        f"{PYTHON_IMPORT_PACKAGE}/__init__.py": (
            root / "src" / PYTHON_IMPORT_PACKAGE / "__init__.py"
        ).read_bytes(),
        **{
            f"{PYTHON_IMPORT_PACKAGE}/{python_module_name(index)}.py": (
                root
                / "src"
                / PYTHON_IMPORT_PACKAGE
                / f"{python_module_name(index)}.py"
            ).read_bytes()
            for index in range(size)
        },
    }
    dist_info = f"{PYTHON_IMPORT_PACKAGE}-{PYTHON_VERSION}.dist-info"
    with zipfile.ZipFile(wheel) as archive:
        names = archive.namelist()
        if len(names) != len(set(names)):
            raise RuntimeError(f"{spec.name} wheel contains duplicate entries")
        for name, expected in expected_sources.items():
            try:
                actual = archive.read(name)
            except KeyError as error:
                raise RuntimeError(f"{spec.name} wheel omitted {name}") from error
            if actual != expected:
                raise RuntimeError(f"{spec.name} wheel changed source bytes for {name}")

        metadata = archive.read(f"{dist_info}/METADATA").decode("utf-8")
        if f"Name: {PYTHON_DISTRIBUTION}\n" not in metadata:
            raise RuntimeError(f"{spec.name} wheel has wrong Name metadata")
        if f"Version: {PYTHON_VERSION}\n" not in metadata:
            raise RuntimeError(f"{spec.name} wheel has wrong Version metadata")
        wheel_metadata = archive.read(f"{dist_info}/WHEEL").decode("utf-8")
        for field in [
            "Wheel-Version: 1.0\n",
            "Root-Is-Purelib: true\n",
            "Tag: py3-none-any\n",
        ]:
            if field not in wheel_metadata:
                raise RuntimeError(f"{spec.name} WHEEL omitted {field.strip()!r}")

        record_name = f"{dist_info}/RECORD"
        rows = list(
            csv.reader(io.StringIO(archive.read(record_name).decode("utf-8")))
        )
        if any(len(row) != 3 for row in rows):
            raise RuntimeError(f"{spec.name} RECORD has a malformed row")
        recorded_names = [row[0] for row in rows]
        if sorted(recorded_names) != sorted(names):
            raise RuntimeError(f"{spec.name} RECORD does not cover every wheel entry exactly")
        for name, encoded_hash, encoded_size in rows:
            if name == record_name:
                if encoded_hash or encoded_size:
                    raise RuntimeError(f"{spec.name} RECORD hashes itself")
                continue
            payload = archive.read(name)
            expected_hash = base64.urlsafe_b64encode(hashlib.sha256(payload).digest()).rstrip(b"=")
            if encoded_hash.encode() != b"sha256=" + expected_hash:
                raise RuntimeError(f"{spec.name} RECORD hash mismatch for {name}")
            if encoded_size != str(len(payload)):
                raise RuntimeError(f"{spec.name} RECORD size mismatch for {name}")

        semantic = hashlib.sha256()
        for name in sorted(expected_sources):
            semantic.update(name.encode())
            semantic.update(b"\0")
            semantic.update(expected_sources[name])
            semantic.update(b"\0")

        with tempfile.TemporaryDirectory(prefix="frost-wheel-install-") as install_dir:
            archive.extractall(install_dir)
            completed = subprocess.run(
                [
                    python,
                    "-I",
                    "-c",
                    (
                        "import sys; sys.path.insert(0, sys.argv[1]); "
                        f"import {PYTHON_IMPORT_PACKAGE} as package; print(package.total())"
                    ),
                    install_dir,
                ],
                cwd=root,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
            )
    if completed.returncode != 0:
        raise RuntimeError(
            f"{spec.name} installed wheel exited {completed.returncode}: "
            f"{completed.stderr.strip()}"
        )
    if completed.stdout != f"{expected_total}\n" or completed.stderr:
        raise RuntimeError(
            f"{spec.name} installed wheel returned stdout={completed.stdout!r}, "
            f"stderr={completed.stderr!r}"
        )
    return {
        "artifact_bytes": wheel.stat().st_size,
        "artifact_sha256": hashlib.sha256(wheel.read_bytes()).hexdigest(),
        "source_file_count": len(expected_sources),
        "semantic_sha256": semantic.hexdigest(),
        "record_entry_count": len(rows),
    }


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
    if spec.name == "bazel":
        output_root = root.parent / f".{root.name}.bazel-user-root"
        if spec.argv:
            subprocess.run(
                [
                    spec.argv[0],
                    f"--output_user_root={output_root}",
                    "clean",
                    "--expunge",
                ],
                cwd=root,
                check=False,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
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
        if name == "bazel":
            configured = os.environ.get("BAZEL_BIN")
            executable = configured or shutil.which("bazel")
            specs.append(
                ToolSpec(
                    name=name,
                    argv=(executable,) if executable else (),
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


def java_tool_specs(names: list[str]) -> list[ToolSpec]:
    specs = []
    frost_spec = tool_specs(["frost"])[0]
    for name in names:
        if name.startswith("frost-"):
            specs.append(ToolSpec(name=name, argv=frost_spec.argv[:1]))
            continue
        family = name.removesuffix("-jar")
        configured = os.environ.get(f"{family.upper()}_BIN")
        executable = configured or shutil.which("mvn" if family == "maven" else family)
        specs.append(ToolSpec(name=name, argv=(executable,) if executable else ()))
    return specs


def rust_tool_specs(names: list[str]) -> tuple[list[ToolSpec], str | None]:
    frost_spec = tool_specs(["frost"])[0]
    configured_cargo = os.environ.get("CARGO_BIN")
    cargo = configured_cargo or shutil.which("cargo")
    configured_rustc = os.environ.get("RUSTC_BIN")
    rustc = configured_rustc or shutil.which("rustc")
    specs = [
        ToolSpec(
            name=name,
            argv=(
                frost_spec.argv[:1]
                if name == "frost"
                else ((cargo,) if cargo else ())
            ),
        )
        for name in names
    ]
    return specs, rustc


def go_tool_specs(
    names: list[str],
) -> tuple[list[ToolSpec], str | None, dict[str, str] | None]:
    frost_spec = tool_specs(["frost"])[0]
    configured = os.environ.get("GO_BIN")
    go_executable = configured or shutil.which("go")
    go_info = None
    if go_executable:
        try:
            go_info = json.loads(
                subprocess.check_output(
                    [
                        go_executable,
                        "env",
                        "-json",
                        "GOARCH",
                        "GOAMD64",
                        "GOOS",
                        "GOROOT",
                        "GOTOOLDIR",
                        "GOVERSION",
                    ],
                    text=True,
                    stderr=subprocess.STDOUT,
                )
            )
        except (OSError, subprocess.SubprocessError, json.JSONDecodeError):
            go_info = None
    specs = [
        ToolSpec(
            name=name,
            argv=(
                frost_spec.argv[:1]
                if name.startswith("frost-")
                else ((go_executable,) if go_executable else ())
            ),
        )
        for name in names
    ]
    return specs, go_executable, go_info


def typescript_tool_specs(
    names: list[str],
) -> tuple[list[ToolSpec], str | None, str | None, str | None]:
    frost_spec = tool_specs(["frost"])[0]
    configured_tsc = os.environ.get("TSC_BIN")
    discovered_tsc = configured_tsc or shutil.which("tsc")
    tsc = None
    compiler_reason = None
    if discovered_tsc:
        candidate = pathlib.Path(discovered_tsc).resolve()
        try:
            if candidate.read_bytes()[:4] == b"\x7fELF":
                tsc = candidate.as_posix()
            else:
                compiler_reason = (
                    f"{candidate} is not the native TypeScript 7 compiler; "
                    "set TSC_BIN to its native tsc executable"
                )
        except OSError as error:
            compiler_reason = f"could not inspect {candidate}: {error}"
    else:
        compiler_reason = (
            "native TypeScript 7 compiler was not found; set TSC_BIN to include it"
        )
    configured_node = os.environ.get("NODE_BIN")
    node = configured_node or shutil.which("node")
    specs = [
        ToolSpec(
            name=name,
            argv=(
                frost_spec.argv[:1]
                if name == "frost"
                else ((tsc,) if tsc else ())
            ),
        )
        for name in names
    ]
    return specs, tsc, node, compiler_reason


def python_tool_specs(
    names: list[str],
) -> tuple[list[ToolSpec], str | None, str | None]:
    frost_spec = tool_specs(["frost"])[0]
    python = os.environ.get("PYTHON_BIN") or shutil.which("python3")
    uv = os.environ.get("UV_BIN") or shutil.which("uv")
    build_reason = None
    build_available = False
    if python:
        try:
            subprocess.run(
                [python, "-m", "build", "--version"],
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                check=True,
            )
            build_available = True
        except (OSError, subprocess.SubprocessError):
            build_reason = (
                f"{python} cannot import the Python 'build' frontend; "
                "install it or set PYTHON_BIN"
            )
    else:
        build_reason = "python3 executable was not found; set PYTHON_BIN"
    specs = []
    for name in names:
        if name == "frost":
            argv = frost_spec.argv[:1]
        elif name == "python-build":
            argv = (python,) if python and build_available else ()
        elif name == "uv":
            argv = (uv,) if uv else ()
        else:
            raise ValueError(name)
        specs.append(ToolSpec(name=name, argv=argv))
    return specs, python, build_reason


def java_command(spec: ToolSpec, jobs: int) -> list[str]:
    if not spec.argv:
        raise FileNotFoundError(spec.name)
    executable = spec.argv[0]
    if spec.name.startswith("frost-"):
        return [
            executable,
            "build",
            "--workspace",
            ".",
            "--jobs",
            str(max(1, jobs)),
            "--no-tui",
        ]
    if spec.name.startswith("gradle"):
        return [
            executable,
            "--daemon",
            "--console=plain",
            "--quiet",
            "--max-workers",
            str(max(1, jobs)),
            "jar" if spec.name == "gradle-jar" else "classes",
        ]
    if spec.name.startswith("maven"):
        return [
            executable,
            "--quiet",
            "--batch-mode",
            "--no-transfer-progress",
            "-Dstyle.color=never",
            "-DskipTests",
            f"-T{max(1, jobs)}",
            "package" if spec.name == "maven-jar" else "compile",
        ]
    raise ValueError(spec.name)


def run_java_tool(root: pathlib.Path, spec: ToolSpec, jobs: int) -> tuple[float, str]:
    command = java_command(spec, jobs)
    start = time.perf_counter()
    completed = subprocess.run(
        command,
        cwd=root,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        check=False,
    )
    elapsed_ms = (time.perf_counter() - start) * 1000
    if completed.returncode != 0:
        tail = "\n".join(completed.stdout.splitlines()[-30:])
        raise RuntimeError(
            f"{spec.name} failed with exit {completed.returncode}: {' '.join(command)}\n{tail}"
        )
    return elapsed_ms, completed.stdout


def clean_java_outputs(root: pathlib.Path, spec: ToolSpec) -> None:
    if spec.name.startswith("frost-"):
        candidates = [root / ".frost"]
    elif spec.name.startswith("gradle"):
        candidates = [root / "build"]
    elif spec.name.startswith("maven"):
        candidates = [root / "target"]
    else:
        raise ValueError(spec.name)
    for path in candidates:
        if path.exists():
            shutil.rmtree(path)


def rust_command(spec: ToolSpec, jobs: int) -> list[str]:
    if not spec.argv:
        raise FileNotFoundError(spec.name)
    if spec.name == "frost":
        return [
            spec.argv[0],
            "build",
            "--workspace",
            ".",
            "--jobs",
            str(max(1, jobs)),
            "--no-tui",
        ]
    if spec.name == "cargo":
        return [
            spec.argv[0],
            "build",
            "--quiet",
            "--jobs",
            str(max(1, jobs)),
        ]
    raise ValueError(spec.name)


def run_rust_tool(
    root: pathlib.Path,
    spec: ToolSpec,
    jobs: int,
    rustc: str,
) -> tuple[float, str]:
    command = rust_command(spec, jobs)
    environment = os.environ.copy()
    if spec.name == "cargo":
        environment["RUSTC"] = rustc
        # Do not inherit a user-global CARGO_TARGET_DIR: every frontend gets
        # an isolated tree whose clean/incremental state this harness controls.
        environment["CARGO_TARGET_DIR"] = (root / "target").as_posix()
    start = time.perf_counter()
    completed = subprocess.run(
        command,
        cwd=root,
        env=environment,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        check=False,
    )
    elapsed_ms = (time.perf_counter() - start) * 1000
    if completed.returncode != 0:
        tail = "\n".join(completed.stdout.splitlines()[-30:])
        raise RuntimeError(
            f"{spec.name} failed with exit {completed.returncode}: "
            f"{' '.join(command)}\n{tail}"
        )
    return elapsed_ms, completed.stdout


def clean_rust_outputs(root: pathlib.Path, spec: ToolSpec) -> None:
    if spec.name == "frost":
        candidate = root / ".frost"
    elif spec.name == "cargo":
        candidate = root / "target"
    else:
        raise ValueError(spec.name)
    if candidate.exists():
        shutil.rmtree(candidate)


def go_command(root: pathlib.Path, spec: ToolSpec, jobs: int) -> list[str]:
    if not spec.argv:
        raise FileNotFoundError(spec.name)
    if spec.name.startswith("frost-"):
        return [
            spec.argv[0],
            "build",
            "--workspace",
            ".",
            "--jobs",
            str(max(1, jobs)),
            "--no-tui",
        ]
    if spec.name == "go":
        return [
            spec.argv[0],
            "build",
            "-buildvcs=false",
            "-pgo=off",
            "-p",
            str(max(1, jobs)),
            "-ldflags=-buildid=",
            "-o",
            (root / "out" / go_binary_name()).as_posix(),
            ".",
        ]
    raise ValueError(spec.name)


def run_go_tool(
    root: pathlib.Path,
    spec: ToolSpec,
    jobs: int,
    go_info: dict[str, str],
) -> tuple[float, str]:
    command = go_command(root, spec, jobs)
    environment = (
        go_process_environment(root, go_info)
        if spec.name == "go"
        else os.environ.copy()
    )
    start = time.perf_counter()
    completed = subprocess.run(
        command,
        cwd=root,
        env=environment,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        check=False,
    )
    elapsed_ms = (time.perf_counter() - start) * 1000
    if completed.returncode != 0:
        tail = "\n".join(completed.stdout.splitlines()[-30:])
        raise RuntimeError(
            f"{spec.name} failed with exit {completed.returncode}: "
            f"{' '.join(command)}\n{tail}"
        )
    return elapsed_ms, completed.stdout


def clean_go_outputs(root: pathlib.Path, spec: ToolSpec) -> None:
    if spec.name.startswith("frost-"):
        candidate = root / ".frost"
        if candidate.exists():
            shutil.rmtree(candidate)
    elif spec.name == "go":
        binary = root / "out" / go_binary_name()
        if binary.exists():
            binary.unlink()
        (root / "out").mkdir(exist_ok=True)
    else:
        raise ValueError(spec.name)
    if spec.name in ("frost-go", "go"):
        reset_go_project_cache(root)


def typescript_command(
    root: pathlib.Path,
    spec: ToolSpec,
    jobs: int,
    checkers: int,
) -> list[str]:
    if not spec.argv:
        raise FileNotFoundError(spec.name)
    if spec.name == "frost":
        return [
            spec.argv[0],
            "build",
            "--workspace",
            ".",
            "--jobs",
            str(max(1, jobs)),
            "--no-tui",
        ]
    if spec.name == "tsc":
        return [
            (root / "typescript-sdk/lib/tsc").as_posix(),
            "--project",
            "tsconfig.json",
            "--checkers",
            str(max(1, checkers)),
            "--pretty",
            "false",
        ]
    raise ValueError(spec.name)


def run_typescript_tool(
    root: pathlib.Path,
    spec: ToolSpec,
    jobs: int,
    checkers: int,
) -> tuple[float, str]:
    command = typescript_command(root, spec, jobs, checkers)
    start = time.perf_counter()
    completed = subprocess.run(
        command,
        cwd=root,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        check=False,
    )
    elapsed_ms = (time.perf_counter() - start) * 1000
    if completed.returncode != 0:
        tail = "\n".join(completed.stdout.splitlines()[-30:])
        raise RuntimeError(
            f"{spec.name} failed with exit {completed.returncode}: "
            f"{' '.join(command)}\n{tail}"
        )
    return elapsed_ms, completed.stdout


def clean_typescript_outputs(root: pathlib.Path, spec: ToolSpec) -> None:
    candidate = root / (".frost" if spec.name == "frost" else "target")
    if candidate.exists():
        shutil.rmtree(candidate)


def python_command(
    root: pathlib.Path,
    spec: ToolSpec,
    jobs: int,
    python: str,
) -> list[str]:
    if not spec.argv:
        raise FileNotFoundError(spec.name)
    if spec.name == "frost":
        return [
            spec.argv[0],
            "build",
            "--workspace",
            ".",
            "--jobs",
            str(max(1, jobs)),
            "--no-tui",
        ]
    if spec.name == "python-build":
        return [
            python,
            "-m",
            "build",
            "--wheel",
            "--no-isolation",
            "--outdir",
            "dist",
            "--quiet",
            ".",
        ]
    if spec.name == "uv":
        return [
            spec.argv[0],
            "build",
            "--wheel",
            "--no-build-isolation",
            "--out-dir",
            "dist",
            "--no-create-gitignore",
            "--offline",
            "--quiet",
            "--python",
            python,
            ".",
        ]
    raise ValueError(spec.name)


def run_python_tool(
    root: pathlib.Path,
    spec: ToolSpec,
    jobs: int,
    python: str,
) -> tuple[float, str]:
    command = python_command(root, spec, jobs, python)
    environment = os.environ.copy()
    environment["PYTHONDONTWRITEBYTECODE"] = "1"
    environment["SOURCE_DATE_EPOCH"] = "946684800"
    start = time.perf_counter()
    completed = subprocess.run(
        command,
        cwd=root,
        env=environment,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        check=False,
    )
    elapsed_ms = (time.perf_counter() - start) * 1000
    if completed.returncode != 0:
        tail = "\n".join(completed.stdout.splitlines()[-30:])
        raise RuntimeError(
            f"{spec.name} failed with exit {completed.returncode}: "
            f"{' '.join(command)}\n{tail}"
        )
    return elapsed_ms, completed.stdout


def clean_python_outputs(root: pathlib.Path, spec: ToolSpec) -> None:
    if spec.name == "frost":
        candidates = [root / ".frost"]
    elif spec.name in ("python-build", "uv"):
        candidates = [root / "dist", root / "build"]
        candidates.extend((root / "src").glob("*.egg-info"))
    else:
        raise ValueError(spec.name)
    for candidate in candidates:
        if candidate.exists():
            if candidate.is_dir():
                shutil.rmtree(candidate)
            else:
                candidate.unlink()


def python_tool_version(spec: ToolSpec, python: str | None) -> str | None:
    if not spec.argv:
        return None
    if spec.name == "python-build":
        command = [spec.argv[0], "-m", "build", "--version"]
    else:
        command = [spec.argv[0], "--version"]
    try:
        completed = subprocess.run(
            command,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=True,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    version = completed.stdout.strip().splitlines()
    runtime = command_version(python) if python else None
    if spec.name == "python-build" and runtime:
        return f"{version[0] if version else 'build unknown'}; {runtime}"
    return version[0] if version else None


def typescript_output_directory(root: pathlib.Path, spec: ToolSpec) -> pathlib.Path:
    if spec.name == "frost":
        return root / ".frost/out/debug/js"
    if spec.name == "tsc":
        return root / "target/js"
    raise ValueError(spec.name)


def verify_typescript_output(
    root: pathlib.Path,
    spec: ToolSpec,
    size: int,
    expected_total: int,
    node: str,
) -> dict[str, Any]:
    directory = typescript_output_directory(root, spec)
    expected_names = [pathlib.Path(path).name for path in typescript_outputs(size, "")]
    outputs = sorted(directory.glob("*.js"))
    if [path.name for path in outputs] != sorted(expected_names):
        raise RuntimeError(
            f"{spec.name} produced {[path.name for path in outputs]!r}, "
            f"expected {sorted(expected_names)!r}"
        )
    entrypoint = directory / "main.js"
    completed = subprocess.run(
        [node, entrypoint.as_posix()],
        cwd=root,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    expected_stdout = f"{expected_total}\n"
    if completed.returncode != 0:
        raise RuntimeError(
            f"{spec.name} JavaScript exited {completed.returncode}: "
            f"{completed.stderr.strip()}"
        )
    if completed.stdout != expected_stdout or completed.stderr:
        raise RuntimeError(
            f"{spec.name} JavaScript returned stdout={completed.stdout!r}, "
            f"stderr={completed.stderr!r}; expected {expected_stdout!r} and empty stderr"
        )
    hasher = hashlib.sha256()
    artifact_bytes = 0
    for path in outputs:
        payload = path.read_bytes()
        artifact_bytes += len(payload)
        hasher.update(path.name.encode())
        hasher.update(b"\0")
        hasher.update(payload)
        hasher.update(b"\0")
    semantic_payload = json.dumps(
        {"exit_code": 0, "stderr": "", "stdout": expected_stdout},
        sort_keys=True,
        separators=(",", ":"),
    ).encode()
    return {
        "artifact_bytes": artifact_bytes,
        "artifact_sha256": hasher.hexdigest(),
        "file_count": len(outputs),
        "semantic_sha256": hashlib.sha256(semantic_payload).hexdigest(),
        "stdout": expected_stdout.rstrip("\n"),
    }


def go_binary(root: pathlib.Path, spec: ToolSpec) -> pathlib.Path:
    if spec.name.startswith("frost-"):
        return root / ".frost/out/debug" / go_binary_name()
    if spec.name == "go":
        return root / "out" / go_binary_name()
    raise ValueError(spec.name)


def verify_go_output(
    root: pathlib.Path,
    spec: ToolSpec,
    expected_total: int,
    go_executable: str,
    go_info: dict[str, str],
) -> dict[str, Any]:
    binary = go_binary(root, spec)
    if not binary.is_file():
        raise RuntimeError(f"{spec.name} did not produce {binary}")
    completed = subprocess.run(
        [binary.as_posix()],
        cwd=root,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    expected_stderr = f"{expected_total}\n"
    if completed.returncode != 0:
        raise RuntimeError(
            f"{spec.name} artifact exited {completed.returncode}: "
            f"{completed.stderr.strip()}"
        )
    if completed.stdout or completed.stderr != expected_stderr:
        raise RuntimeError(
            f"{spec.name} artifact produced stdout={completed.stdout!r}, "
            f"stderr={completed.stderr!r}; expected empty stdout and "
            f"stderr={expected_stderr!r}"
        )
    metadata = subprocess.run(
        [go_executable, "version", "-m", binary.as_posix()],
        cwd=root,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if metadata.returncode != 0:
        raise RuntimeError(
            f"{spec.name} artifact metadata inspection failed: "
            f"{metadata.stderr.strip()}"
        )
    metadata_lines = sorted(
        line.strip()
        for line in metadata.stdout.splitlines()[1:]
        if line.strip()
    )
    expected_metadata = sorted(
        [
            "path\texample.com/frost-go-bench",
            "mod\texample.com/frost-go-bench\t(devel)",
            "build\t-buildmode=exe",
            "build\t-compiler=gc",
            "build\t-ldflags=-buildid=",
            "build\tCGO_ENABLED=0",
            f"build\tGOARCH={go_info['GOARCH']}",
            f"build\tGOOS={go_info['GOOS']}",
            f"build\tGOAMD64={go_info.get('GOAMD64', 'v1')}",
        ]
    )
    if metadata_lines != expected_metadata:
        raise RuntimeError(
            f"{spec.name} artifact build metadata differs: "
            f"{metadata_lines!r}, expected {expected_metadata!r}"
        )
    metadata_payload = json.dumps(
        metadata_lines,
        separators=(",", ":"),
    ).encode()
    metadata_digest = hashlib.sha256(metadata_payload).hexdigest()
    artifact = binary.read_bytes()
    semantic_payload = json.dumps(
        {
            "build_metadata_sha256": metadata_digest,
            "exit_code": 0,
            "stderr": expected_stderr,
            "stdout": "",
        },
        sort_keys=True,
        separators=(",", ":"),
    ).encode()
    return {
        "artifact_bytes": len(artifact),
        "artifact_sha256": hashlib.sha256(artifact).hexdigest(),
        "build_metadata_sha256": metadata_digest,
        "semantic_sha256": hashlib.sha256(semantic_payload).hexdigest(),
        "stderr": expected_stderr.rstrip("\n"),
    }


def rust_binary(root: pathlib.Path, spec: ToolSpec) -> pathlib.Path:
    if spec.name == "frost":
        return root / ".frost/out/debug" / rust_binary_name()
    if spec.name == "cargo":
        return root / "target/debug" / rust_binary_name()
    raise ValueError(spec.name)


def verify_rust_output(
    root: pathlib.Path,
    spec: ToolSpec,
    expected_total: int,
) -> dict[str, Any]:
    binary = rust_binary(root, spec)
    if not binary.is_file():
        raise RuntimeError(f"{spec.name} did not produce {binary}")
    completed = subprocess.run(
        [binary.as_posix()],
        cwd=root,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    expected_stdout = f"{expected_total}\n"
    if completed.returncode != 0:
        raise RuntimeError(
            f"{spec.name} artifact exited {completed.returncode}: "
            f"{completed.stderr.strip()}"
        )
    if completed.stdout != expected_stdout:
        raise RuntimeError(
            f"{spec.name} artifact printed {completed.stdout!r}, "
            f"expected {expected_stdout!r}"
        )
    artifact = binary.read_bytes()
    semantic_payload = json.dumps(
        {
            "exit_code": 0,
            "stdout": expected_stdout,
        },
        sort_keys=True,
        separators=(",", ":"),
    ).encode()
    return {
        "artifact_bytes": len(artifact),
        "artifact_sha256": hashlib.sha256(artifact).hexdigest(),
        "semantic_sha256": hashlib.sha256(semantic_payload).hexdigest(),
        "stdout": expected_stdout.rstrip("\n"),
    }


def java_class_directory(root: pathlib.Path, spec: ToolSpec) -> pathlib.Path:
    if spec.name.startswith("frost-"):
        return root / ".frost/out/debug/classes/bench"
    if spec.name.startswith("gradle"):
        return root / "build/classes/java/main/bench"
    if spec.name.startswith("maven"):
        return root / "target/classes/bench"
    raise ValueError(spec.name)


def java_archive(root: pathlib.Path, spec: ToolSpec) -> pathlib.Path:
    if spec.name == "frost-jar":
        return root / ".frost/out/debug/java-bench.jar"
    if spec.name == "gradle-jar":
        candidates = sorted((root / "build/libs").glob("*.jar"))
    elif spec.name == "maven-jar":
        candidates = sorted((root / "target").glob("*.jar"))
    else:
        raise ValueError(spec.name)
    if len(candidates) != 1:
        raise RuntimeError(f"{spec.name} produced {len(candidates)} jars, expected one")
    return candidates[0]


def verify_java_outputs(root: pathlib.Path, spec: ToolSpec, size: int) -> str:
    if spec.name.endswith("-jar"):
        archive = java_archive(root, spec)
        if not archive.is_file():
            raise RuntimeError(f"{spec.name} did not produce {archive}")
        expected_entries = [
            f"bench/{java_class_name(index)}.class" for index in range(size)
        ]
        with zipfile.ZipFile(archive) as jar:
            entries = sorted(
                name
                for name in jar.namelist()
                if name.startswith("bench/Class") and name.endswith(".class")
            )
            if entries != expected_entries:
                raise RuntimeError(
                    f"{spec.name} jar contains {len(entries)} benchmark classes, "
                    f"expected {size}"
                )
            hasher = hashlib.sha256()
            for entry in entries:
                hasher.update(entry.encode())
                hasher.update(b"\0")
                hasher.update(jar.read(entry))
                hasher.update(b"\0")
        return hasher.hexdigest()

    directory = java_class_directory(root, spec)
    classes = sorted(directory.glob("Class*.class"))
    expected_names = [f"{java_class_name(index)}.class" for index in range(size)]
    if [path.name for path in classes] != expected_names:
        raise RuntimeError(
            f"{spec.name} produced {len(classes)} benchmark classes, expected {size}"
        )
    hasher = hashlib.sha256()
    for path in classes:
        hasher.update(f"bench/{path.name}".encode())
        hasher.update(b"\0")
        hasher.update(path.read_bytes())
        hasher.update(b"\0")
    return hasher.hexdigest()


def java_tool_version(spec: ToolSpec) -> str | None:
    if not spec.argv:
        return None
    if spec.name.startswith("frost-"):
        command = [spec.argv[0], "--version"]
    else:
        command = [spec.argv[0], "--version"]
    try:
        completed = subprocess.run(
            command,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=True,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    clean = re.sub(r"\x1b\[[0-9;]*[A-Za-z]", "", completed.stdout)
    lines = [line.strip() for line in clean.splitlines() if line.strip()]
    preferred = next(
        (
            line
            for line in lines
            if line.startswith(("frost ", "Gradle ", "Apache Maven "))
        ),
        None,
    )
    return preferred or next(iter(lines), None)


def command_version(executable: str | None) -> str | None:
    if not executable:
        return None
    try:
        completed = subprocess.run(
            [executable, "--version"],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=True,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    return next(
        (line.strip() for line in completed.stdout.splitlines() if line.strip()),
        None,
    )


def rust_configuration_metrics(root: pathlib.Path, spec: ToolSpec) -> dict[str, Any]:
    files = ["frost.toml"] if spec.name == "frost" else ["Cargo.toml"]
    texts = [(root / path).read_text(encoding="utf-8") for path in files]
    metrics = {
        "files": files,
        "configuration_lines": sum(len(text.splitlines()) for text in texts),
        "configuration_bytes": sum(len(text.encode()) for text in texts),
        "declared_compilation_partitions": 1,
        "one_source_change_compiler_scope": "rustc incremental query graph for one crate",
    }
    if spec.name == "cargo":
        metrics["generated_files_excluded_from_configuration_size"] = ["Cargo.lock"]
    return metrics


def go_configuration_metrics(root: pathlib.Path, spec: ToolSpec) -> dict[str, Any]:
    files = ["frost.toml"] if spec.name.startswith("frost-") else ["go.mod"]
    texts = [(root / path).read_text(encoding="utf-8") for path in files]
    return {
        "files": files,
        "configuration_lines": sum(len(text.splitlines()) for text in texts),
        "configuration_bytes": sum(len(text.encode()) for text in texts),
        "declared_compilation_partitions": 1,
        "one_source_change_compiler_scope": "one Go package",
        "generated_toolchain_bundle_excluded_from_configuration_size": (
            [".go-sdk"] if spec.name == "frost-native" else []
        ),
    }


def typescript_configuration_metrics(
    root: pathlib.Path,
    spec: ToolSpec,
) -> dict[str, Any]:
    files = ["tsconfig.json", "package.json"]
    if spec.name == "frost":
        files.insert(0, "frost.toml")
    texts = [(root / path).read_text(encoding="utf-8") for path in files]
    return {
        "files": files,
        "configuration_lines": sum(len(text.splitlines()) for text in texts),
        "configuration_bytes": sum(len(text.encode()) for text in texts),
        "declared_compilation_partitions": 1,
        "one_source_change_compiler_scope": (
            "TypeScript incremental semantic graph for one project"
        ),
        "generated_toolchain_bundle_excluded_from_configuration_size": [
            "typescript-sdk/lib"
        ],
    }


def python_configuration_metrics(root: pathlib.Path, spec: ToolSpec) -> dict[str, Any]:
    files = ["pyproject.toml"]
    if spec.name == "frost":
        files.insert(0, "frost.toml")
    texts = [(root / path).read_text(encoding="utf-8") for path in files]
    return {
        "files": files,
        "configuration_lines": sum(len(text.splitlines()) for text in texts),
        "configuration_bytes": sum(len(text.encode()) for text in texts),
        "declared_packaging_partitions": 1,
        "one_source_change_packaging_scope": "one pure-Python wheel",
    }


def java_configuration_metrics(root: pathlib.Path, spec: ToolSpec, size: int) -> dict[str, Any]:
    if spec.name.startswith("frost-"):
        files = ["frost.toml"]
        partitions: int | str = size if spec.name == "frost-unit" else 1
        incremental_scope: int | str = 1 if spec.name == "frost-unit" else size
    elif spec.name.startswith("gradle"):
        files = ["settings.gradle", "build.gradle", "gradle.properties"]
        partitions = 1
        incremental_scope = "Gradle JavaCompile incremental analysis"
    else:
        files = ["pom.xml"]
        partitions = 1
        incremental_scope = "Maven compiler-plugin incremental analysis"
    texts = [(root / path).read_text(encoding="utf-8") for path in files]
    return {
        "files": files,
        "configuration_lines": sum(len(text.splitlines()) for text in texts),
        "configuration_bytes": sum(len(text.encode()) for text in texts),
        "declared_compilation_partitions": partitions,
        "one_source_change_compiler_scope": incremental_scope,
    }


def run_tool(root: pathlib.Path, spec: ToolSpec, jobs: int) -> float:
    if not spec.argv:
        raise FileNotFoundError(spec.name)
    if spec.name == "bazel":
        output_root = root.parent / f".{root.name}.bazel-user-root"
        cmd = [
            spec.argv[0],
            f"--output_user_root={output_root}",
            "build",
            "//:all",
            "--jobs",
            str(max(1, jobs)),
        ]
    elif spec.name == "frost":
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


def graph_contract(size: int) -> dict[str, Any]:
    edges = [(target_name(index), target_name(index - 1)) for index in range(1, size)]
    payload = json.dumps(edges, separators=(",", ":")).encode()
    return {
        "shape": "linear-chain",
        "action_count": size,
        "dependency_edge_count": len(edges),
        "edge_digest_sha256": hashlib.sha256(payload).hexdigest(),
        "per_action_source_inputs": ["src/nodeNNNNN.txt", "include/hot.h"],
        "manifests_verified_equivalent": True,
    }


def tool_version(spec: ToolSpec) -> str | None:
    if not spec.argv:
        return None
    executable = spec.argv[0]
    flag = "--version" if spec.name != "make" else "--version"
    try:
        output = subprocess.check_output(
            [executable, flag],
            text=True,
            stderr=subprocess.STDOUT,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    return output.splitlines()[0].strip() if output else None


def measure_tool(
    root: pathlib.Path,
    spec: ToolSpec,
    size: int,
    iterations: int,
    jobs: int,
    selected_scenarios: tuple[str, ...] = STANDARD_SCENARIOS,
) -> dict[str, Any]:
    if not spec.argv:
        return {
            "tool": spec.name,
            "size": size,
            "status": "skipped",
            "reason": f"{spec.name} executable was not found",
            "scenarios": {},
        }

    scenarios: dict[str, Any] = {}

    if "clean" in selected_scenarios:
        clean_samples = []
        for _ in range(iterations):
            clean_tool_outputs(root, spec, cache=True)
            clean_samples.append(run_tool(root, spec, jobs))
        scenarios["clean"] = summarize(clean_samples)

    if "noop" in selected_scenarios:
        # A no-op-only run starts from a freshly generated workspace, so seed
        # its outputs once before the ordinary warmup/cache-settling run.
        if "clean" not in selected_scenarios:
            run_tool(root, spec, jobs)
        run_tool(root, spec, jobs)
        noop_samples = [run_tool(root, spec, jobs) for _ in range(iterations)]
        scenarios["noop"] = summarize(noop_samples)

    if "incremental_leaf" in selected_scenarios:
        leaf = root / "src" / f"{target_name(size - 1)}.txt"
        incremental_samples = []
        run_tool(root, spec, jobs)
        for _ in range(iterations):
            append_marker(leaf, "leaf")
            incremental_samples.append(run_tool(root, spec, jobs))
        scenarios["incremental_leaf"] = summarize(incremental_samples)

    if "hot_header" in selected_scenarios:
        header = root / "include/hot.h"
        header_samples = []
        run_tool(root, spec, jobs)
        for _ in range(iterations):
            append_marker(header, "hot-header")
            header_samples.append(run_tool(root, spec, jobs))
        scenarios["hot_header"] = summarize(header_samples)

    if "cache_hit_rebuild" in selected_scenarios:
        if spec.name == "frost":
            cache_hit_samples = []
            run_tool(root, spec, jobs)
            for _ in range(iterations):
                clean_tool_outputs(root, spec, cache=False)
                cache_hit_samples.append(run_tool(root, spec, jobs))
            scenarios["cache_hit_rebuild"] = summarize(cache_hit_samples)
        else:
            reason = (
                "Bazel has no external CAS configured in this local harness"
                if spec.name == "bazel"
                else f"{spec.name} has no content-addressed action cache in this harness"
            )
            scenarios["cache_hit_rebuild"] = scenario_not_applicable(reason)

    return {
        "tool": spec.name,
        "version": tool_version(spec),
        "size": size,
        "status": "ok",
        "iterations": iterations,
        "jobs": jobs,
        "target_count": size,
        "graph": graph_contract(size),
        "scenarios": scenarios,
    }


def run_standard(args: argparse.Namespace) -> dict[str, Any]:
    tools = parse_csv(args.tools, valid=SUPPORTED_TOOLS)
    sizes = parse_sizes(args.sizes)
    requested_scenarios = parse_csv(
        getattr(args, "scenarios", ",".join(STANDARD_SCENARIOS)),
        valid=STANDARD_SCENARIOS,
    )
    if not requested_scenarios:
        raise SystemExit("--scenarios must select at least one scenario")
    selected_scenarios = tuple(
        scenario for scenario in STANDARD_SCENARIOS if scenario in requested_scenarios
    )
    if args.iterations <= 0:
        raise SystemExit("--iterations must be positive")
    if args.jobs <= 0:
        raise SystemExit("--jobs must be positive")
    environment = environment_snapshot()

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
                results.append(
                    measure_tool(
                        root,
                        spec,
                        size,
                        args.iterations,
                        args.jobs,
                        selected_scenarios,
                    )
                )
    finally:
        if temp_context is not None and not args.keep_workdir:
            temp_context.cleanup()

    report = {
        "schema": SCHEMA,
        "suite": args.suite,
        "generated_at": utc_now(),
        "environment": environment,
        "config": {
            "tools": tools,
            "sizes": sizes,
            "iterations": args.iterations,
            "jobs": args.jobs,
            "scenarios": list(selected_scenarios),
        },
        "results": results,
    }
    return report


def measure_java_tool(
    root: pathlib.Path,
    spec: ToolSpec,
    size: int,
    iterations: int,
    jobs: int,
    selected_scenarios: tuple[str, ...],
) -> dict[str, Any]:
    if not spec.argv:
        family = spec.name.removesuffix("-jar")
        executable = "mvn" if family == "maven" else family
        return {
            "tool": spec.name,
            "size": size,
            "status": "skipped",
            "reason": (
                f"{executable} executable was not found; set "
                f"{family.upper()}_BIN to include it"
            ),
            "scenarios": {},
        }

    scenarios: dict[str, Any] = {}
    outputs: list[str] = []

    if "clean" in selected_scenarios:
        samples = []
        for _ in range(iterations):
            clean_java_outputs(root, spec)
            elapsed, output = run_java_tool(root, spec, jobs)
            samples.append(elapsed)
            outputs.append(output)
            verify_java_outputs(root, spec, size)
        scenarios["clean"] = summarize(samples)

    if "noop" in selected_scenarios:
        if "clean" not in selected_scenarios:
            run_java_tool(root, spec, jobs)
        # Settle JIT/daemon/configuration caches and, for Frost, create the
        # whole-closure no-op certificate before taking samples.
        run_java_tool(root, spec, jobs)
        samples = []
        for _ in range(iterations):
            elapsed, output = run_java_tool(root, spec, jobs)
            samples.append(elapsed)
            outputs.append(output)
        scenarios["noop"] = summarize(samples)

    if "incremental_leaf" in selected_scenarios:
        run_java_tool(root, spec, jobs)
        source = root / "src/main/java/bench" / f"{java_class_name(size - 1)}.java"
        samples = []
        for iteration in range(iterations):
            with source.open("a", encoding="utf-8") as file:
                file.write(f"// frost-java-bench change {iteration} {time.time_ns()}\n")
            elapsed, output = run_java_tool(root, spec, jobs)
            samples.append(elapsed)
            outputs.append(output)
            verify_java_outputs(root, spec, size)
        scenarios["incremental_leaf"] = summarize(samples)

    digest = verify_java_outputs(root, spec, size)
    summaries = [
        line.strip()
        for output in outputs
        for line in output.splitlines()
        if line.strip().startswith("frost:")
        or "BUILD SUCCESSFUL" in line
        or "BUILD SUCCESS" in line
    ]
    return {
        "tool": spec.name,
        "version": java_tool_version(spec),
        "size": size,
        "status": "ok",
        "iterations": iterations,
        "jobs": jobs,
        "source_count": size,
        "output_class_count": size,
        "output_set_sha256": digest,
        "configuration": java_configuration_metrics(root, spec, size),
        "observed_summaries": summaries[-3:],
        "scenarios": scenarios,
    }


def measure_java_tools_interleaved(
    base_workdir: pathlib.Path,
    specs: list[ToolSpec],
    size: int,
    iterations: int,
    jobs: int,
    selected_scenarios: tuple[str, ...],
) -> list[dict[str, Any]]:
    """Measure tools round-robin, reversing order on every iteration.

    Running every scenario for one frontend before starting the next gives
    thermal state, JIT settling, and background load a tool-order bias. Each
    Java frontend has an independent generated workspace, so alternating the
    order is safe and preserves the same per-tool cache state.
    """
    roots: dict[str, pathlib.Path] = {}
    samples: dict[str, dict[str, list[float]]] = {}
    summaries: dict[str, list[str]] = {}
    failures: dict[str, str] = {}
    skipped: dict[str, dict[str, Any]] = {}
    active: list[ToolSpec] = []

    for spec in specs:
        root = base_workdir / f"java-{spec.name}-{size}"
        generate_java_workspace(root, size, spec.name)
        roots[spec.name] = root
        samples[spec.name] = {scenario: [] for scenario in selected_scenarios}
        summaries[spec.name] = []
        if not spec.argv:
            family = spec.name.removesuffix("-jar")
            executable = "mvn" if family == "maven" else family
            skipped[spec.name] = {
                "tool": spec.name,
                "size": size,
                "status": "skipped",
                "reason": (
                    f"{executable} executable was not found; set "
                    f"{family.upper()}_BIN to include it"
                ),
                "scenarios": {},
            }
        else:
            active.append(spec)

    def ordered(iteration: int) -> list[ToolSpec]:
        available = [spec for spec in active if spec.name not in failures]
        return available if iteration % 2 == 0 else list(reversed(available))

    def remember_summary(name: str, output: str) -> None:
        summaries[name].extend(
            line.strip()
            for line in output.splitlines()
            if line.strip().startswith("frost:")
            or "BUILD SUCCESSFUL" in line
            or "BUILD SUCCESS" in line
        )

    def seed(spec: ToolSpec) -> None:
        if spec.name in failures:
            return
        try:
            _, output = run_java_tool(roots[spec.name], spec, jobs)
            remember_summary(spec.name, output)
            verify_java_outputs(roots[spec.name], spec, size)
        except Exception as error:
            failures[spec.name] = str(error)

    def measure(spec: ToolSpec, scenario: str) -> None:
        if spec.name in failures:
            return
        try:
            elapsed, output = run_java_tool(roots[spec.name], spec, jobs)
            samples[spec.name][scenario].append(elapsed)
            remember_summary(spec.name, output)
            verify_java_outputs(roots[spec.name], spec, size)
        except Exception as error:
            failures[spec.name] = str(error)

    if "clean" in selected_scenarios:
        for iteration in range(iterations):
            for spec in ordered(iteration):
                try:
                    clean_java_outputs(roots[spec.name], spec)
                except Exception as error:
                    failures[spec.name] = str(error)
                    continue
                measure(spec, "clean")

    if "noop" in selected_scenarios:
        # Seed missing outputs, then settle JIT/daemon/configuration caches and
        # Frost's whole-closure no-op certificate before sampling.
        for spec in active:
            if "clean" not in selected_scenarios:
                seed(spec)
            seed(spec)
        for iteration in range(iterations):
            for spec in ordered(iteration):
                measure(spec, "noop")

    if "incremental_leaf" in selected_scenarios:
        for spec in active:
            seed(spec)
        for iteration in range(iterations):
            for spec in ordered(iteration):
                if spec.name in failures:
                    continue
                source = (
                    roots[spec.name]
                    / "src/main/java/bench"
                    / f"{java_class_name(size - 1)}.java"
                )
                with source.open("a", encoding="utf-8") as file:
                    file.write(
                        f"// frost-java-bench change {iteration} {time.time_ns()}\n"
                    )
                measure(spec, "incremental_leaf")

    results = []
    for spec in specs:
        if spec.name in skipped:
            results.append(skipped[spec.name])
            continue
        if spec.name in failures:
            results.append(
                {
                    "tool": spec.name,
                    "size": size,
                    "status": "failed",
                    "reason": failures[spec.name],
                    "scenarios": {},
                }
            )
            continue
        root = roots[spec.name]
        results.append(
            {
                "tool": spec.name,
                "version": java_tool_version(spec),
                "size": size,
                "status": "ok",
                "iterations": iterations,
                "jobs": jobs,
                "source_count": size,
                "output_class_count": size,
                "output_set_sha256": verify_java_outputs(root, spec, size),
                "configuration": java_configuration_metrics(root, spec, size),
                "observed_summaries": summaries[spec.name][-3:],
                "scenarios": {
                    scenario: summarize(samples[spec.name][scenario])
                    for scenario in selected_scenarios
                },
            }
        )
    return results


def java_graph_contract(size: int) -> dict[str, Any]:
    source_paths = java_sources(size)
    class_names = [f"bench/{java_class_name(index)}.class" for index in range(size)]
    payload = json.dumps(
        {"sources": source_paths, "outputs": class_names},
        sort_keys=True,
        separators=(",", ":"),
    ).encode()
    return {
        "language": "Java",
        "shape": "independent-source-set",
        "source_count": size,
        "required_class_count": size,
        "javac_release": 21,
        "contract_sha256": hashlib.sha256(payload).hexdigest(),
        "same_source_and_output_name_sets": True,
        "note": (
            "Frost-unit declares one javac action per source; Frost-batch, "
            "Gradle and Maven receive the same source set at a coarser boundary"
        ),
    }


def run_java_benchmark(args: argparse.Namespace) -> dict[str, Any]:
    tools = parse_csv(args.tools, valid=JAVA_TOOLS)
    requested_scenarios = parse_csv(args.scenarios, valid=JAVA_SCENARIOS)
    if not tools:
        raise SystemExit("--tools must select at least one Java frontend")
    if not requested_scenarios:
        raise SystemExit("--scenarios must select at least one scenario")
    if args.size <= 0 or args.iterations <= 0 or args.jobs <= 0:
        raise SystemExit("--size, --iterations, and --jobs must be positive")
    environment = environment_snapshot()
    selected_scenarios = tuple(
        scenario for scenario in JAVA_SCENARIOS if scenario in requested_scenarios
    )

    if args.workdir:
        base_workdir = pathlib.Path(args.workdir).resolve()
        base_workdir.mkdir(parents=True, exist_ok=True)
        temp_context = None
    else:
        temp_context = tempfile.TemporaryDirectory(prefix="frost-java-bench-")
        base_workdir = pathlib.Path(temp_context.name)

    specs = java_tool_specs(tools)
    try:
        results = measure_java_tools_interleaved(
            base_workdir,
            specs,
            args.size,
            args.iterations,
            args.jobs,
            selected_scenarios,
        )
    finally:
        if temp_context is not None and not args.keep_workdir:
            temp_context.cleanup()

    successful_digests = {
        result["tool"]: result["output_set_sha256"]
        for result in results
        if result["status"] == "ok"
    }
    report = {
        "schema": JAVA_SCHEMA,
        "suite": "java",
        "generated_at": utc_now(),
        "environment": environment,
        "config": {
            "tools": tools,
            "size": args.size,
            "iterations": args.iterations,
            "jobs": args.jobs,
            "scenarios": list(selected_scenarios),
            "gradle_policy": (
                "daemon and configuration cache enabled; build cache disabled"
            ),
            "clean_policy": (
                "delete produced output tree; retain tool installation, "
                "Gradle daemon, Maven repository, and JDK"
            ),
            "artifact_contract": (
                "JAR containing the required class binary names"
                if all(tool.endswith("-jar") for tool in tools)
                else "required class files"
            ),
            "execution_order": (
                "round-robin; reverse frontend order on every measured iteration"
            ),
        },
        "graph": java_graph_contract(args.size),
        "output_equivalence": {
            "comparison_unit": (
                "required class binary names and bytes; "
                "JAR container metadata excluded"
            ),
            "digests": successful_digests,
            "byte_identical": (
                len(successful_digests) > 1
                and len(set(successful_digests.values())) == 1
            ),
        },
        "results": results,
    }
    return report


def measure_rust_tools_interleaved(
    base_workdir: pathlib.Path,
    specs: list[ToolSpec],
    rustc: str | None,
    size: int,
    iterations: int,
    jobs: int,
    selected_scenarios: tuple[str, ...],
) -> list[dict[str, Any]]:
    """Measure one equal Rust crate per frontend in alternating order."""
    compiler = rustc or "rustc"
    initial_total = size * (size - 1) // 2
    roots: dict[str, pathlib.Path] = {}
    expected_totals: dict[str, int] = {}
    samples: dict[str, dict[str, list[float]]] = {}
    summaries: dict[str, list[str]] = {}
    failures: dict[str, str] = {}
    skipped: dict[str, dict[str, Any]] = {}
    active: list[ToolSpec] = []

    for spec in specs:
        root = base_workdir / f"rust-{spec.name}-{size}"
        generate_rust_workspace(root, size, spec.name, compiler)
        roots[spec.name] = root
        expected_totals[spec.name] = initial_total
        samples[spec.name] = {scenario: [] for scenario in selected_scenarios}
        summaries[spec.name] = []
        if rustc is None:
            skipped[spec.name] = {
                "tool": spec.name,
                "size": size,
                "status": "skipped",
                "reason": (
                    "rustc executable was not found; set RUSTC_BIN to include it"
                ),
                "scenarios": {},
            }
        elif not spec.argv:
            executable = "frost" if spec.name == "frost" else "cargo"
            environment_name = "FROST_BIN" if spec.name == "frost" else "CARGO_BIN"
            skipped[spec.name] = {
                "tool": spec.name,
                "size": size,
                "status": "skipped",
                "reason": (
                    f"{executable} executable was not found; set "
                    f"{environment_name} to include it"
                ),
                "scenarios": {},
            }
        else:
            active.append(spec)

    def ordered(iteration: int) -> list[ToolSpec]:
        available = [spec for spec in active if spec.name not in failures]
        return available if iteration % 2 == 0 else list(reversed(available))

    def remember_summary(name: str, output: str) -> None:
        summaries[name].extend(
            line.strip()
            for line in output.splitlines()
            if line.strip().startswith("frost:")
        )

    def build_and_validate(spec: ToolSpec, scenario: str | None) -> None:
        if spec.name in failures:
            return
        try:
            elapsed, output = run_rust_tool(
                roots[spec.name],
                spec,
                jobs,
                rustc or compiler,
            )
            remember_summary(spec.name, output)
            verify_rust_output(
                roots[spec.name],
                spec,
                expected_totals[spec.name],
            )
            if scenario is not None:
                samples[spec.name][scenario].append(elapsed)
        except Exception as error:
            failures[spec.name] = str(error)

    if "clean" in selected_scenarios:
        for iteration in range(iterations):
            for spec in ordered(iteration):
                try:
                    clean_rust_outputs(roots[spec.name], spec)
                except Exception as error:
                    failures[spec.name] = str(error)
                    continue
                build_and_validate(spec, "clean")

    if "noop" in selected_scenarios:
        # Seed missing outputs, then settle Cargo fingerprints and Frost's
        # whole-closure no-op certificate before taking samples.
        for spec in active:
            if "clean" not in selected_scenarios:
                build_and_validate(spec, None)
            build_and_validate(spec, None)
        for iteration in range(iterations):
            for spec in ordered(iteration):
                build_and_validate(spec, "noop")

    if "incremental_leaf" in selected_scenarios:
        for spec in active:
            build_and_validate(spec, None)
        for iteration in range(iterations):
            for spec in ordered(iteration):
                if spec.name in failures:
                    continue
                changed_value = size - 1 + iteration + 1
                write_rust_module(
                    roots[spec.name]
                    / "src"
                    / f"{rust_module_name(size - 1)}.rs",
                    changed_value,
                )
                expected_totals[spec.name] = initial_total + iteration + 1
                build_and_validate(spec, "incremental_leaf")

    results = []
    for spec in specs:
        if spec.name in skipped:
            results.append(skipped[spec.name])
            continue
        if spec.name in failures:
            results.append(
                {
                    "tool": spec.name,
                    "size": size,
                    "status": "failed",
                    "reason": failures[spec.name],
                    "scenarios": {},
                }
            )
            continue
        root = roots[spec.name]
        artifact = verify_rust_output(
            root,
            spec,
            expected_totals[spec.name],
        )
        results.append(
            {
                "tool": spec.name,
                "version": command_version(spec.argv[0]),
                "compiler_version": command_version(rustc),
                "size": size,
                "status": "ok",
                "iterations": iterations,
                "jobs": jobs,
                "source_count": size + 1,
                "module_count": size,
                "artifact": artifact,
                "configuration": rust_configuration_metrics(root, spec),
                "observed_summaries": summaries[spec.name][-3:],
                "scenarios": {
                    scenario: summarize(samples[spec.name][scenario])
                    for scenario in selected_scenarios
                },
            }
        )
    return results


def rust_graph_contract(size: int) -> dict[str, Any]:
    sources = rust_sources(size)
    payload = json.dumps(
        {
            "sources": sources,
            "module_values": list(range(size)),
            "entrypoint": "src/main.rs",
        },
        sort_keys=True,
        separators=(",", ":"),
    ).encode()
    return {
        "language": "Rust",
        "shape": "single-crate-module-set",
        "source_count": size + 1,
        "module_count": size,
        "initial_expected_stdout": str(size * (size - 1) // 2),
        "contract_sha256": hashlib.sha256(payload).hexdigest(),
        "same_source_graph": True,
        "compilation_boundary": "one rustc crate",
    }


def run_rust_benchmark(args: argparse.Namespace) -> dict[str, Any]:
    tools = parse_csv(args.tools, valid=RUST_TOOLS)
    requested_scenarios = parse_csv(args.scenarios, valid=RUST_SCENARIOS)
    if not tools:
        raise SystemExit("--tools must select at least one Rust frontend")
    if not requested_scenarios:
        raise SystemExit("--scenarios must select at least one scenario")
    if args.size <= 0 or args.iterations <= 0 or args.jobs <= 0:
        raise SystemExit("--size, --iterations, and --jobs must be positive")
    selected_scenarios = tuple(
        scenario for scenario in RUST_SCENARIOS if scenario in requested_scenarios
    )
    environment = environment_snapshot()

    if args.workdir:
        base_workdir = pathlib.Path(args.workdir).resolve()
        base_workdir.mkdir(parents=True, exist_ok=True)
        temp_context = None
    else:
        temp_context = tempfile.TemporaryDirectory(prefix="frost-rust-bench-")
        base_workdir = pathlib.Path(temp_context.name)

    specs, rustc = rust_tool_specs(tools)
    try:
        results = measure_rust_tools_interleaved(
            base_workdir,
            specs,
            rustc,
            args.size,
            args.iterations,
            args.jobs,
            selected_scenarios,
        )
    finally:
        if temp_context is not None and not args.keep_workdir:
            temp_context.cleanup()

    successful = [result for result in results if result["status"] == "ok"]
    semantic_digests = {
        result["tool"]: result["artifact"]["semantic_sha256"]
        for result in successful
    }
    artifact_digests = {
        result["tool"]: result["artifact"]["artifact_sha256"]
        for result in successful
    }
    return {
        "schema": RUST_SCHEMA,
        "suite": "rust",
        "generated_at": utc_now(),
        "environment": environment,
        "config": {
            "tools": tools,
            "size": args.size,
            "iterations": args.iterations,
            "jobs": args.jobs,
            "scenarios": list(selected_scenarios),
            "compiler": {
                "path": rustc,
                "version": command_version(rustc),
            },
            "profile": {
                "edition": "2021",
                "opt_level": 0,
                "debuginfo": 0,
                "strip": "none",
                "debug_assertions": True,
                "overflow_checks": True,
                "lto": False,
                "panic": "unwind",
                "incremental": True,
                "codegen_units": 256,
                "embed_bitcode": False,
            },
            "clean_policy": (
                "delete the frontend output tree and its rustc incremental "
                "state; retain the Rust toolchain and Cargo registry"
            ),
            "warm_policy": (
                "retain each frontend's normal rustc incremental state for "
                "no-op and one-module-change samples"
            ),
            "execution_order": (
                "round-robin; reverse frontend order on every measured iteration"
            ),
        },
        "graph": rust_graph_contract(args.size),
        "output_equivalence": {
            "comparison_unit": (
                "successful execution, exit code 0, and exact stdout after "
                "every timed build; frontend-specific binary bytes may differ"
            ),
            "validated_after_every_timed_build": True,
            "semantic_digests": semantic_digests,
            "artifact_digests": artifact_digests,
            "semantic_equal": (
                len(semantic_digests) > 1
                and len(set(semantic_digests.values())) == 1
            ),
            "byte_identical": (
                len(artifact_digests) > 1
                and len(set(artifact_digests.values())) == 1
            ),
        },
        "results": results,
    }


def go_frontend_version(spec: ToolSpec) -> str | None:
    if not spec.argv:
        return None
    command = (
        [spec.argv[0], "version"]
        if spec.name == "go"
        else [spec.argv[0], "--version"]
    )
    try:
        completed = subprocess.run(
            command,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=True,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    return next(
        (line.strip() for line in completed.stdout.splitlines() if line.strip()),
        None,
    )


def measure_go_tools_interleaved(
    base_workdir: pathlib.Path,
    specs: list[ToolSpec],
    go_executable: str | None,
    go_info: dict[str, str] | None,
    size: int,
    iterations: int,
    jobs: int,
    selected_scenarios: tuple[str, ...],
) -> list[dict[str, Any]]:
    """Measure equal Go package artifacts in alternating frontend order."""
    info = go_info or {
        "GOARCH": platform.machine(),
        "GOAMD64": "v1",
        "GOOS": platform.system().lower(),
        "GOROOT": "",
        "GOTOOLDIR": "",
        "GOVERSION": "go1.26",
    }
    initial_total = size * (size - 1) // 2
    roots: dict[str, pathlib.Path] = {}
    expected_totals: dict[str, int] = {}
    samples: dict[str, dict[str, list[float]]] = {}
    summaries: dict[str, list[str]] = {}
    failures: dict[str, str] = {}
    skipped: dict[str, dict[str, Any]] = {}
    active: list[ToolSpec] = []

    for spec in specs:
        root = base_workdir / f"go-{spec.name}-{size}"
        generate_go_workspace(
            root,
            size,
            spec.name,
            jobs,
            go_executable,
            go_info,
        )
        roots[spec.name] = root
        expected_totals[spec.name] = initial_total
        samples[spec.name] = {scenario: [] for scenario in selected_scenarios}
        summaries[spec.name] = []
        if go_executable is None or go_info is None:
            skipped[spec.name] = {
                "tool": spec.name,
                "size": size,
                "status": "skipped",
                "reason": "go executable was not found; set GO_BIN to include it",
                "scenarios": {},
            }
        elif not spec.argv:
            skipped[spec.name] = {
                "tool": spec.name,
                "size": size,
                "status": "skipped",
                "reason": (
                    "frost executable was not found; set FROST_BIN to include it"
                ),
                "scenarios": {},
            }
        else:
            active.append(spec)

    def ordered(iteration: int) -> list[ToolSpec]:
        available = [spec for spec in active if spec.name not in failures]
        return available if iteration % 2 == 0 else list(reversed(available))

    def remember_summary(name: str, output: str) -> None:
        summaries[name].extend(
            line.strip()
            for line in output.splitlines()
            if line.strip().startswith("frost:")
        )

    def build_and_validate(spec: ToolSpec, scenario: str | None) -> None:
        if spec.name in failures:
            return
        try:
            elapsed, output = run_go_tool(
                roots[spec.name],
                spec,
                jobs,
                info,
            )
            remember_summary(spec.name, output)
            verify_go_output(
                roots[spec.name],
                spec,
                expected_totals[spec.name],
                go_executable or "go",
                info,
            )
            if scenario is not None:
                samples[spec.name][scenario].append(elapsed)
        except Exception as error:
            failures[spec.name] = str(error)

    if "clean" in selected_scenarios:
        for iteration in range(iterations):
            for spec in ordered(iteration):
                try:
                    clean_go_outputs(roots[spec.name], spec)
                except Exception as error:
                    failures[spec.name] = str(error)
                    continue
                build_and_validate(spec, "clean")

    if "noop" in selected_scenarios:
        for spec in active:
            if "clean" not in selected_scenarios:
                build_and_validate(spec, None)
            build_and_validate(spec, None)
        for iteration in range(iterations):
            for spec in ordered(iteration):
                build_and_validate(spec, "noop")

    if "incremental_leaf" in selected_scenarios:
        for spec in active:
            build_and_validate(spec, None)
        for iteration in range(iterations):
            for spec in ordered(iteration):
                if spec.name in failures:
                    continue
                changed_value = size - 1 + iteration + 1
                write_go_value(
                    roots[spec.name] / f"{go_file_stem(size - 1)}.go",
                    go_file_stem(size - 1),
                    changed_value,
                )
                expected_totals[spec.name] = initial_total + iteration + 1
                build_and_validate(spec, "incremental_leaf")

    results = []
    for spec in specs:
        if spec.name in skipped:
            results.append(skipped[spec.name])
            continue
        if spec.name in failures:
            results.append(
                {
                    "tool": spec.name,
                    "size": size,
                    "status": "failed",
                    "reason": failures[spec.name],
                    "scenarios": {},
                }
            )
            continue
        root = roots[spec.name]
        artifact = verify_go_output(
            root,
            spec,
            expected_totals[spec.name],
            go_executable or "go",
            info,
        )
        results.append(
            {
                "tool": spec.name,
                "version": go_frontend_version(spec),
                "compiler_version": info.get("GOVERSION"),
                "size": size,
                "status": "ok",
                "iterations": iterations,
                "jobs": jobs,
                "source_count": size + 1,
                "package_count": 1,
                "artifact": artifact,
                "configuration": go_configuration_metrics(root, spec),
                "observed_summaries": summaries[spec.name][-3:],
                "scenarios": {
                    scenario: summarize(samples[spec.name][scenario])
                    for scenario in selected_scenarios
                },
            }
        )
    return results


def go_graph_contract(size: int) -> dict[str, Any]:
    sources = go_sources(size)
    payload = json.dumps(
        {
            "sources": sources,
            "function_values": list(range(size)),
            "entrypoint": "main.go",
            "module": "example.com/frost-go-bench",
        },
        sort_keys=True,
        separators=(",", ":"),
    ).encode()
    return {
        "language": "Go",
        "shape": "single-main-package-file-set",
        "source_count": size + 1,
        "function_file_count": size,
        "package_count": 1,
        "initial_expected_stderr": str(size * (size - 1) // 2),
        "contract_sha256": hashlib.sha256(payload).hexdigest(),
        "same_source_graph": True,
        "compilation_boundary": "one Go package and executable link",
    }


def run_go_benchmark(args: argparse.Namespace) -> dict[str, Any]:
    tools = parse_csv(args.tools, valid=GO_TOOLS)
    requested_scenarios = parse_csv(args.scenarios, valid=GO_SCENARIOS)
    if not tools:
        raise SystemExit("--tools must select at least one Go frontend")
    if not requested_scenarios:
        raise SystemExit("--scenarios must select at least one scenario")
    if args.size <= 0 or args.iterations <= 0 or args.jobs <= 0:
        raise SystemExit("--size, --iterations, and --jobs must be positive")
    selected_scenarios = tuple(
        scenario for scenario in GO_SCENARIOS if scenario in requested_scenarios
    )
    environment = environment_snapshot()

    if args.workdir:
        base_workdir = pathlib.Path(args.workdir).resolve()
        base_workdir.mkdir(parents=True, exist_ok=True)
        temp_context = None
    else:
        temp_context = tempfile.TemporaryDirectory(prefix="frost-go-bench-")
        base_workdir = pathlib.Path(temp_context.name)

    specs, go_executable, go_info = go_tool_specs(tools)
    try:
        results = measure_go_tools_interleaved(
            base_workdir,
            specs,
            go_executable,
            go_info,
            args.size,
            args.iterations,
            args.jobs,
            selected_scenarios,
        )
    finally:
        if temp_context is not None and not args.keep_workdir:
            temp_context.cleanup()

    successful = [result for result in results if result["status"] == "ok"]
    semantic_digests = {
        result["tool"]: result["artifact"]["semantic_sha256"]
        for result in successful
    }
    artifact_digests = {
        result["tool"]: result["artifact"]["artifact_sha256"]
        for result in successful
    }
    metadata_digests = {
        result["tool"]: result["artifact"]["build_metadata_sha256"]
        for result in successful
    }
    return {
        "schema": GO_SCHEMA,
        "suite": "go",
        "generated_at": utc_now(),
        "environment": environment,
        "config": {
            "tools": tools,
            "size": args.size,
            "iterations": args.iterations,
            "jobs": args.jobs,
            "scenarios": list(selected_scenarios),
            "compiler": {
                "path": go_executable,
                "version": go_info.get("GOVERSION") if go_info else None,
                "goos": go_info.get("GOOS") if go_info else None,
                "goarch": go_info.get("GOARCH") if go_info else None,
            },
            "profile": {
                "buildvcs": False,
                "cgo_enabled": False,
                "pgo": "off",
                "trimpath": False,
                "link_buildid": "",
            },
            "clean_policy": (
                "delete the project artifact/frontend state and reset an "
                "isolated Go cache to a prewarmed runtime-only seed"
            ),
            "warm_policy": (
                "retain each frontend's project cache for no-op and "
                "one-file-change samples"
            ),
            "native_policy": (
                "frost-native directly invokes the selected Go distribution's "
                "compile and link tools with a declared copied runtime export closure"
            ),
            "execution_order": (
                "round-robin; reverse frontend order on every measured iteration"
            ),
        },
        "graph": go_graph_contract(args.size),
        "output_equivalence": {
            "comparison_unit": (
                "successful execution, exit code 0, empty stdout, and exact "
                "stderr plus exact normalized module/build metadata after "
                "every timed build; compiler-internal binary bytes may differ"
            ),
            "validated_after_every_timed_build": True,
            "semantic_digests": semantic_digests,
            "artifact_digests": artifact_digests,
            "build_metadata_digests": metadata_digests,
            "semantic_equal": (
                len(semantic_digests) > 1
                and len(set(semantic_digests.values())) == 1
            ),
            "build_metadata_equal": (
                len(metadata_digests) > 1
                and len(set(metadata_digests.values())) == 1
            ),
            "byte_identical": (
                len(artifact_digests) > 1
                and len(set(artifact_digests.values())) == 1
            ),
        },
        "results": results,
    }


def measure_typescript_tools_interleaved(
    base_workdir: pathlib.Path,
    specs: list[ToolSpec],
    tsc: str | None,
    node: str | None,
    compiler_reason: str | None,
    size: int,
    iterations: int,
    jobs: int,
    checkers_by_tool: dict[str, int],
    selected_scenarios: tuple[str, ...],
) -> list[dict[str, Any]]:
    """Measure the same TypeScript project and compiler in alternating order."""
    initial_total = size * (size - 1) // 2
    roots: dict[str, pathlib.Path] = {}
    expected_totals: dict[str, int] = {}
    samples: dict[str, dict[str, list[float]]] = {}
    summaries: dict[str, list[str]] = {}
    failures: dict[str, str] = {}
    skipped: dict[str, dict[str, Any]] = {}
    active: list[ToolSpec] = []

    for spec in specs:
        root = base_workdir / f"typescript-{spec.name}-{size}"
        roots[spec.name] = root
        expected_totals[spec.name] = initial_total
        samples[spec.name] = {scenario: [] for scenario in selected_scenarios}
        summaries[spec.name] = []
        if tsc is None:
            skipped[spec.name] = {
                "tool": spec.name,
                "size": size,
                "status": "skipped",
                "reason": compiler_reason or "native TypeScript 7 compiler was not found",
                "scenarios": {},
            }
            continue
        if node is None:
            skipped[spec.name] = {
                "tool": spec.name,
                "size": size,
                "status": "skipped",
                "reason": "node executable was not found; set NODE_BIN to include it",
                "scenarios": {},
            }
            continue
        if not spec.argv:
            skipped[spec.name] = {
                "tool": spec.name,
                "size": size,
                "status": "skipped",
                "reason": "frost executable was not found; set FROST_BIN to include it",
                "scenarios": {},
            }
            continue
        try:
            generate_typescript_workspace(
                root,
                size,
                spec.name,
                tsc,
                checkers_by_tool[spec.name],
            )
        except Exception as error:
            failures[spec.name] = str(error)
            continue
        active.append(spec)

    def ordered(iteration: int) -> list[ToolSpec]:
        available = [spec for spec in active if spec.name not in failures]
        return available if iteration % 2 == 0 else list(reversed(available))

    def remember_summary(name: str, output: str) -> None:
        summaries[name].extend(
            line.strip()
            for line in output.splitlines()
            if line.strip().startswith("frost:")
        )

    def build_and_validate(spec: ToolSpec, scenario: str | None) -> None:
        if spec.name in failures:
            return
        try:
            elapsed, output = run_typescript_tool(
                roots[spec.name],
                spec,
                jobs,
                checkers_by_tool[spec.name],
            )
            remember_summary(spec.name, output)
            verify_typescript_output(
                roots[spec.name],
                spec,
                size,
                expected_totals[spec.name],
                node or "node",
            )
            if scenario is not None:
                samples[spec.name][scenario].append(elapsed)
        except Exception as error:
            failures[spec.name] = str(error)

    if "clean" in selected_scenarios:
        for iteration in range(iterations):
            for spec in ordered(iteration):
                try:
                    clean_typescript_outputs(roots[spec.name], spec)
                except Exception as error:
                    failures[spec.name] = str(error)
                    continue
                build_and_validate(spec, "clean")

    if "noop" in selected_scenarios:
        for spec in active:
            if "clean" not in selected_scenarios:
                build_and_validate(spec, None)
            build_and_validate(spec, None)
        for iteration in range(iterations):
            for spec in ordered(iteration):
                build_and_validate(spec, "noop")

    if "incremental_leaf" in selected_scenarios:
        for spec in active:
            build_and_validate(spec, None)
        for iteration in range(iterations):
            for spec in ordered(iteration):
                if spec.name in failures:
                    continue
                changed_value = size - 1 + iteration + 1
                write_typescript_module(
                    roots[spec.name]
                    / "src"
                    / f"{typescript_module_name(size - 1)}.ts",
                    changed_value,
                )
                expected_totals[spec.name] = initial_total + iteration + 1
                build_and_validate(spec, "incremental_leaf")

    results = []
    for spec in specs:
        if spec.name in skipped:
            results.append(skipped[spec.name])
            continue
        if spec.name in failures:
            results.append(
                {
                    "tool": spec.name,
                    "size": size,
                    "status": "failed",
                    "reason": failures[spec.name],
                    "scenarios": {},
                }
            )
            continue
        root = roots[spec.name]
        artifact = verify_typescript_output(
            root,
            spec,
            size,
            expected_totals[spec.name],
            node or "node",
        )
        results.append(
            {
                "tool": spec.name,
                "version": (
                    command_version(spec.argv[0])
                    if spec.name == "frost"
                    else command_version(tsc)
                ),
                "compiler_version": command_version(tsc),
                "runtime_version": command_version(node),
                "size": size,
                "status": "ok",
                "iterations": iterations,
                "jobs": jobs,
                "checkers": checkers_by_tool[spec.name],
                "source_count": size + 1,
                "project_count": 1,
                "artifact": artifact,
                "configuration": typescript_configuration_metrics(root, spec),
                "observed_summaries": summaries[spec.name][-3:],
                "scenarios": {
                    scenario: summarize(samples[spec.name][scenario])
                    for scenario in selected_scenarios
                },
            }
        )
    return results


def typescript_graph_contract(size: int) -> dict[str, Any]:
    sources = typescript_sources(size)
    payload = json.dumps(
        {
            "entrypoint": "src/main.ts",
            "module_values": list(range(size)),
            "sources": sources,
        },
        sort_keys=True,
        separators=(",", ":"),
    ).encode()
    return {
        "language": "TypeScript",
        "shape": "single-project-module-set",
        "source_count": size + 1,
        "module_count": size,
        "project_count": 1,
        "initial_expected_stdout": str(size * (size - 1) // 2),
        "contract_sha256": hashlib.sha256(payload).hexdigest(),
        "same_source_graph": True,
        "compilation_boundary": "one incremental tsc project",
    }


def typescript_toolchain_contract(tsc: str | None) -> dict[str, Any] | None:
    if tsc is None:
        return None
    directory = pathlib.Path(tsc).resolve().parent
    files = [pathlib.Path(tsc).resolve(), *sorted(directory.glob("lib*.d.ts"))]
    hasher = hashlib.sha256()
    total_bytes = 0
    for path in files:
        payload = path.read_bytes()
        total_bytes += len(payload)
        hasher.update(path.relative_to(directory).as_posix().encode())
        hasher.update(b"\0")
        hasher.update(payload)
        hasher.update(b"\0")
    return {
        "source_directory": directory.as_posix(),
        "file_count": len(files),
        "bytes": total_bytes,
        "sha256": hasher.hexdigest(),
        "workspace_copy": "typescript-sdk/lib",
        "compiler_executable_fingerprinted": True,
        "standard_library_declarations_declared_as_frost_inputs": True,
    }


def run_typescript_benchmark(args: argparse.Namespace) -> dict[str, Any]:
    tools = parse_csv(args.tools, valid=TYPESCRIPT_TOOLS)
    requested_scenarios = parse_csv(args.scenarios, valid=TYPESCRIPT_SCENARIOS)
    if not tools:
        raise SystemExit("--tools must select at least one TypeScript frontend")
    if not requested_scenarios:
        raise SystemExit("--scenarios must select at least one scenario")
    checkers_by_tool = {
        "frost": getattr(args, "frost_checkers", None) or args.checkers,
        "tsc": getattr(args, "tsc_checkers", None) or args.checkers,
    }
    if min(args.size, args.iterations, args.jobs, *checkers_by_tool.values()) <= 0:
        raise SystemExit("--size, --iterations, --jobs, and --checkers must be positive")
    selected_scenarios = tuple(
        scenario
        for scenario in TYPESCRIPT_SCENARIOS
        if scenario in requested_scenarios
    )
    environment = environment_snapshot()

    if args.workdir:
        base_workdir = pathlib.Path(args.workdir).resolve()
        base_workdir.mkdir(parents=True, exist_ok=True)
        temp_context = None
    else:
        temp_context = tempfile.TemporaryDirectory(prefix="frost-typescript-bench-")
        base_workdir = pathlib.Path(temp_context.name)

    specs, tsc, node, compiler_reason = typescript_tool_specs(tools)
    try:
        results = measure_typescript_tools_interleaved(
            base_workdir,
            specs,
            tsc,
            node,
            compiler_reason,
            args.size,
            args.iterations,
            args.jobs,
            checkers_by_tool,
            selected_scenarios,
        )
    finally:
        if temp_context is not None and not args.keep_workdir:
            temp_context.cleanup()

    successful = [result for result in results if result["status"] == "ok"]
    semantic_digests = {
        result["tool"]: result["artifact"]["semantic_sha256"]
        for result in successful
    }
    artifact_digests = {
        result["tool"]: result["artifact"]["artifact_sha256"]
        for result in successful
    }
    return {
        "schema": TYPESCRIPT_SCHEMA,
        "suite": "typescript",
        "generated_at": utc_now(),
        "environment": environment,
        "config": {
            "tools": tools,
            "size": args.size,
            "iterations": args.iterations,
            "jobs": args.jobs,
            "checkers": checkers_by_tool,
            "scenarios": list(selected_scenarios),
            "compiler": {
                "path": tsc,
                "version": command_version(tsc),
            },
            "runtime": {
                "path": node,
                "version": command_version(node),
            },
            "profile": {
                "declaration": False,
                "incremental": True,
                "module": "NodeNext",
                "module_resolution": "NodeNext",
                "no_emit_on_error": True,
                "source_map": False,
                "strict": True,
                "target": "ES2022",
            },
            "clean_policy": (
                "delete emitted JavaScript, tsbuildinfo, and Frost state; "
                "retain the copied compiler/runtime declaration closure"
            ),
            "warm_policy": (
                "retain each frontend's tsbuildinfo for no-op and one-module-change samples"
            ),
            "execution_order": (
                "round-robin; reverse frontend order on every measured iteration"
            ),
        },
        "graph": typescript_graph_contract(args.size),
        "toolchain_closure": typescript_toolchain_contract(tsc),
        "output_equivalence": {
            "comparison_unit": (
                "exact emitted JavaScript names and bytes plus successful Node "
                "execution with exact stdout and empty stderr after every timed build"
            ),
            "validated_after_every_timed_build": True,
            "semantic_digests": semantic_digests,
            "artifact_digests": artifact_digests,
            "semantic_equal": (
                len(semantic_digests) > 1
                and len(set(semantic_digests.values())) == 1
            ),
            "byte_identical": (
                len(artifact_digests) > 1
                and len(set(artifact_digests.values())) == 1
            ),
        },
        "results": results,
    }


def run_typescript_solution_tool(
    root: pathlib.Path,
    spec: ToolSpec,
    jobs: int,
    checkers: int,
) -> tuple[float, str]:
    if not spec.argv:
        raise FileNotFoundError(spec.name)
    if spec.name == "frost":
        command = [
            spec.argv[0],
            "build",
            "--workspace",
            ".",
            "--jobs",
            str(max(1, jobs)),
            "--no-tui",
        ]
    elif spec.name == "tsc":
        command = [
            (root / "typescript-sdk/lib/tsc").as_posix(),
            "--build",
            "tsconfig.json",
            "--checkers",
            str(max(1, checkers)),
            "--pretty",
            "false",
        ]
    else:
        raise ValueError(spec.name)
    start = time.perf_counter()
    completed = subprocess.run(
        command,
        cwd=root,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        check=False,
    )
    elapsed_ms = (time.perf_counter() - start) * 1000
    if completed.returncode != 0:
        tail = "\n".join(completed.stdout.splitlines()[-30:])
        raise RuntimeError(
            f"{spec.name} failed with exit {completed.returncode}: "
            f"{' '.join(command)}\n{tail}"
        )
    return elapsed_ms, completed.stdout


def clean_typescript_solution(root: pathlib.Path, spec: ToolSpec) -> None:
    candidate = root / (".frost" if spec.name == "frost" else "target")
    if candidate.exists():
        shutil.rmtree(candidate)


def typescript_solution_configuration_metrics(
    root: pathlib.Path,
    spec: ToolSpec,
    projects: int,
) -> dict[str, Any]:
    files = [
        "package.json",
        "tsconfig.json",
        *[
            f"projects/{typescript_project_name(index)}/tsconfig.json"
            for index in range(projects)
        ],
    ]
    if spec.name == "frost":
        files.insert(0, "frost.toml")
    texts = [(root / path).read_text(encoding="utf-8") for path in files]
    return {
        "files": files,
        "configuration_lines": sum(len(text.splitlines()) for text in texts),
        "configuration_bytes": sum(len(text.encode()) for text in texts),
        "declared_compilation_partitions": projects if spec.name == "frost" else 1,
        "one_source_change_compiler_scope": "one referenced project",
        "generated_toolchain_bundle_excluded_from_configuration_size": [
            "typescript-sdk/lib"
        ],
    }


def measure_typescript_solutions_interleaved(
    base_workdir: pathlib.Path,
    specs: list[ToolSpec],
    tsc: str | None,
    node: str | None,
    compiler_reason: str | None,
    projects: int,
    modules: int,
    iterations: int,
    jobs: int,
    checkers_by_tool: dict[str, int],
    selected_scenarios: tuple[str, ...],
) -> list[dict[str, Any]]:
    initial_total = modules * (modules - 1) // 2
    roots: dict[str, pathlib.Path] = {}
    expected: dict[str, list[int]] = {}
    samples: dict[str, dict[str, list[float]]] = {}
    summaries: dict[str, list[str]] = {}
    failures: dict[str, str] = {}
    skipped: dict[str, dict[str, Any]] = {}
    active: list[ToolSpec] = []

    for spec in specs:
        root = base_workdir / f"typescript-projects-{spec.name}-{projects}x{modules}"
        roots[spec.name] = root
        expected[spec.name] = [initial_total] * projects
        samples[spec.name] = {scenario: [] for scenario in selected_scenarios}
        summaries[spec.name] = []
        reason = None
        if tsc is None:
            reason = compiler_reason or "native TypeScript 7 compiler was not found"
        elif node is None:
            reason = "node executable was not found; set NODE_BIN to include it"
        elif not spec.argv:
            reason = "frost executable was not found; set FROST_BIN to include it"
        if reason is not None:
            skipped[spec.name] = {
                "tool": spec.name,
                "status": "skipped",
                "reason": reason,
                "scenarios": {},
            }
            continue
        try:
            generate_typescript_solution(
                root,
                projects,
                modules,
                spec.name,
                tsc or "tsc",
                checkers_by_tool[spec.name],
            )
        except Exception as error:
            failures[spec.name] = str(error)
            continue
        active.append(spec)

    def ordered(iteration: int) -> list[ToolSpec]:
        available = [spec for spec in active if spec.name not in failures]
        return available if iteration % 2 == 0 else list(reversed(available))

    def build_and_validate(spec: ToolSpec, scenario: str | None) -> None:
        if spec.name in failures:
            return
        try:
            elapsed, output = run_typescript_solution_tool(
                roots[spec.name],
                spec,
                jobs,
                checkers_by_tool[spec.name],
            )
            summaries[spec.name].extend(
                line.strip()
                for line in output.splitlines()
                if line.strip().startswith("frost:")
            )
            verify_typescript_solution_output(
                roots[spec.name],
                spec,
                projects,
                modules,
                expected[spec.name],
                node or "node",
            )
            if scenario is not None:
                samples[spec.name][scenario].append(elapsed)
        except Exception as error:
            failures[spec.name] = str(error)

    if "clean" in selected_scenarios:
        for iteration in range(iterations):
            for spec in ordered(iteration):
                clean_typescript_solution(roots[spec.name], spec)
                build_and_validate(spec, "clean")

    if "noop" in selected_scenarios:
        for spec in active:
            if "clean" not in selected_scenarios:
                build_and_validate(spec, None)
            build_and_validate(spec, None)
        for iteration in range(iterations):
            for spec in ordered(iteration):
                build_and_validate(spec, "noop")

    if "incremental_leaf" in selected_scenarios:
        for spec in active:
            build_and_validate(spec, None)
        for iteration in range(iterations):
            for spec in ordered(iteration):
                if spec.name in failures:
                    continue
                changed = modules - 1 + iteration + 1
                project = typescript_project_name(projects - 1)
                write_typescript_module(
                    roots[spec.name]
                    / "projects"
                    / project
                    / "src"
                    / f"{typescript_module_name(modules - 1)}.ts",
                    changed,
                )
                expected[spec.name][projects - 1] = initial_total + iteration + 1
                build_and_validate(spec, "incremental_leaf")

    results = []
    for spec in specs:
        if spec.name in skipped:
            results.append(skipped[spec.name])
            continue
        if spec.name in failures:
            results.append(
                {
                    "tool": spec.name,
                    "status": "failed",
                    "reason": failures[spec.name],
                    "scenarios": {},
                }
            )
            continue
        root = roots[spec.name]
        artifact = verify_typescript_solution_output(
            root,
            spec,
            projects,
            modules,
            expected[spec.name],
            node or "node",
        )
        results.append(
            {
                "tool": spec.name,
                "status": "ok",
                "version": (
                    command_version(spec.argv[0])
                    if spec.name == "frost"
                    else command_version(tsc)
                ),
                "compiler_version": command_version(tsc),
                "runtime_version": command_version(node),
                "iterations": iterations,
                "jobs": jobs,
                "checkers_per_process": checkers_by_tool[spec.name],
                "maximum_compiler_processes": (
                    min(projects, jobs) if spec.name == "frost" else 1
                ),
                "maximum_worker_budget": (
                    min(projects, jobs) * checkers_by_tool[spec.name]
                    if spec.name == "frost"
                    else checkers_by_tool[spec.name]
                ),
                "source_count": projects * (modules + 1),
                "project_count": projects,
                "artifact": artifact,
                "configuration": typescript_solution_configuration_metrics(
                    root, spec, projects
                ),
                "observed_summaries": summaries[spec.name][-3:],
                "scenarios": {
                    scenario: summarize(samples[spec.name][scenario])
                    for scenario in selected_scenarios
                },
            }
        )
    return results


def run_typescript_projects_benchmark(args: argparse.Namespace) -> dict[str, Any]:
    tools = parse_csv(args.tools, valid=TYPESCRIPT_TOOLS)
    requested_scenarios = parse_csv(args.scenarios, valid=TYPESCRIPT_SCENARIOS)
    checkers_by_tool = {
        "frost": args.frost_checkers,
        "tsc": args.tsc_checkers,
    }
    if not tools or not requested_scenarios:
        raise SystemExit("--tools and --scenarios must not be empty")
    if min(
        args.projects,
        args.modules,
        args.iterations,
        args.jobs,
        *checkers_by_tool.values(),
    ) <= 0:
        raise SystemExit("project benchmark counts must be positive")
    selected_scenarios = tuple(
        scenario
        for scenario in TYPESCRIPT_SCENARIOS
        if scenario in requested_scenarios
    )
    if args.workdir:
        base_workdir = pathlib.Path(args.workdir).resolve()
        base_workdir.mkdir(parents=True, exist_ok=True)
        temp_context = None
    else:
        temp_context = tempfile.TemporaryDirectory(prefix="frost-typescript-projects-")
        base_workdir = pathlib.Path(temp_context.name)
    specs, tsc, node, compiler_reason = typescript_tool_specs(tools)
    try:
        results = measure_typescript_solutions_interleaved(
            base_workdir,
            specs,
            tsc,
            node,
            compiler_reason,
            args.projects,
            args.modules,
            args.iterations,
            args.jobs,
            checkers_by_tool,
            selected_scenarios,
        )
    finally:
        if temp_context is not None and not args.keep_workdir:
            temp_context.cleanup()
    successful = [result for result in results if result["status"] == "ok"]
    semantic_digests = {
        result["tool"]: result["artifact"]["semantic_sha256"]
        for result in successful
    }
    artifact_digests = {
        result["tool"]: result["artifact"]["artifact_sha256"]
        for result in successful
    }
    contract = json.dumps(
        {
            "projects": args.projects,
            "modules_per_project": args.modules,
            "independent_references": [
                typescript_project_name(index) for index in range(args.projects)
            ],
        },
        sort_keys=True,
        separators=(",", ":"),
    ).encode()
    return {
        "schema": TYPESCRIPT_PROJECTS_SCHEMA,
        "suite": "typescript-projects",
        "generated_at": utc_now(),
        "environment": environment_snapshot(),
        "config": {
            "tools": tools,
            "projects": args.projects,
            "modules_per_project": args.modules,
            "iterations": args.iterations,
            "jobs": args.jobs,
            "checkers_per_process": checkers_by_tool,
            "scenarios": list(selected_scenarios),
            "compiler": {"path": tsc, "version": command_version(tsc)},
            "runtime": {"path": node, "version": command_version(node)},
            "execution_order": (
                "round-robin frontends; reverse order on every measured iteration"
            ),
            "clean_policy": (
                "delete all solution outputs/buildinfo and Frost state; retain copied compiler closure"
            ),
        },
        "graph": {
            "language": "TypeScript",
            "shape": "independent-project-reference-solution",
            "project_count": args.projects,
            "modules_per_project": args.modules,
            "source_count": args.projects * (args.modules + 1),
            "contract_sha256": hashlib.sha256(contract).hexdigest(),
            "same_source_graph": True,
        },
        "toolchain_closure": typescript_toolchain_contract(tsc),
        "output_equivalence": {
            "comparison_unit": (
                "exact JavaScript/declaration names and bytes plus exact Node execution for every project after every timed build"
            ),
            "validated_after_every_timed_build": True,
            "semantic_digests": semantic_digests,
            "artifact_digests": artifact_digests,
            "semantic_equal": (
                len(semantic_digests) > 1
                and len(set(semantic_digests.values())) == 1
            ),
            "byte_identical": (
                len(artifact_digests) > 1
                and len(set(artifact_digests.values())) == 1
            ),
        },
        "results": results,
    }


def python_graph_contract(size: int) -> dict[str, Any]:
    sources = [
        f"src/{PYTHON_IMPORT_PACKAGE}/__init__.py",
        *[
            f"src/{PYTHON_IMPORT_PACKAGE}/{python_module_name(index)}.py"
            for index in range(size)
        ],
    ]
    payload = json.dumps(
        {
            "distribution": PYTHON_DISTRIBUTION,
            "version": PYTHON_VERSION,
            "sources": sources,
            "wheel_tag": "py3-none-any",
        },
        sort_keys=True,
        separators=(",", ":"),
    ).encode()
    return {
        "language": "Python",
        "shape": "one pure-Python source-layout distribution",
        "module_count": size,
        "source_count": size + 1,
        "distribution": PYTHON_DISTRIBUTION,
        "version": PYTHON_VERSION,
        "wheel_tag": "py3-none-any",
        "contract_sha256": hashlib.sha256(payload).hexdigest(),
        "same_source_graph": True,
    }


def measure_python_tools_interleaved(
    base_workdir: pathlib.Path,
    specs: list[ToolSpec],
    python: str | None,
    build_reason: str | None,
    size: int,
    iterations: int,
    jobs: int,
    selected_scenarios: tuple[str, ...],
) -> list[dict[str, Any]]:
    initial_total = size * (size - 1) // 2
    frost = next((spec.argv[0] for spec in specs if spec.name == "frost" and spec.argv), "frost")
    roots: dict[str, pathlib.Path] = {}
    expected_totals: dict[str, int] = {}
    samples: dict[str, dict[str, list[float]]] = {}
    summaries: dict[str, list[str]] = {}
    failures: dict[str, str] = {}
    skipped: dict[str, dict[str, Any]] = {}
    active: list[ToolSpec] = []

    for spec in specs:
        root = base_workdir / f"python-{spec.name}-{size}"
        roots[spec.name] = root
        expected_totals[spec.name] = initial_total
        samples[spec.name] = {scenario: [] for scenario in selected_scenarios}
        summaries[spec.name] = []
        reason = None
        if python is None:
            reason = "python3 executable was not found; set PYTHON_BIN"
        elif spec.name == "python-build" and not spec.argv:
            reason = build_reason or "Python build frontend is unavailable"
        elif spec.name == "uv" and not spec.argv:
            reason = "uv executable was not found; set UV_BIN"
        elif spec.name == "frost" and not spec.argv:
            reason = "frost executable was not found; set FROST_BIN"
        if reason is not None:
            skipped[spec.name] = {
                "tool": spec.name,
                "status": "skipped",
                "reason": reason,
                "scenarios": {},
            }
            continue
        try:
            generate_python_workspace(root, size, spec.name, frost)
        except Exception as error:
            failures[spec.name] = str(error)
            continue
        active.append(spec)

    def ordered(iteration: int) -> list[ToolSpec]:
        available = [spec for spec in active if spec.name not in failures]
        return available if iteration % 2 == 0 else list(reversed(available))

    def build_and_validate(spec: ToolSpec, scenario: str | None) -> None:
        if spec.name in failures:
            return
        try:
            elapsed, output = run_python_tool(
                roots[spec.name], spec, jobs, python or "python3"
            )
            summaries[spec.name].extend(
                line.strip()
                for line in output.splitlines()
                if line.strip().startswith("frost:")
                or "Successfully built" in line
                or "Built" in line
            )
            verify_python_wheel(
                roots[spec.name],
                spec,
                size,
                expected_totals[spec.name],
                python or "python3",
            )
            if scenario is not None:
                samples[spec.name][scenario].append(elapsed)
        except Exception as error:
            failures[spec.name] = str(error)

    if "clean" in selected_scenarios:
        for iteration in range(iterations):
            for spec in ordered(iteration):
                try:
                    clean_python_outputs(roots[spec.name], spec)
                except Exception as error:
                    failures[spec.name] = str(error)
                    continue
                build_and_validate(spec, "clean")

    if "noop" in selected_scenarios:
        for spec in active:
            if "clean" not in selected_scenarios:
                build_and_validate(spec, None)
            build_and_validate(spec, None)
        for iteration in range(iterations):
            for spec in ordered(iteration):
                build_and_validate(spec, "noop")

    if "incremental_leaf" in selected_scenarios:
        for spec in active:
            build_and_validate(spec, None)
        for iteration in range(iterations):
            for spec in ordered(iteration):
                if spec.name in failures:
                    continue
                value = size - 1 + iteration + 1
                write_python_module(
                    roots[spec.name]
                    / "src"
                    / PYTHON_IMPORT_PACKAGE
                    / f"{python_module_name(size - 1)}.py",
                    value,
                )
                expected_totals[spec.name] = initial_total + iteration + 1
                build_and_validate(spec, "incremental_leaf")

    results = []
    for spec in specs:
        if spec.name in skipped:
            results.append(skipped[spec.name])
            continue
        if spec.name in failures:
            results.append(
                {
                    "tool": spec.name,
                    "status": "failed",
                    "reason": failures[spec.name],
                    "scenarios": {},
                }
            )
            continue
        root = roots[spec.name]
        artifact = verify_python_wheel(
            root,
            spec,
            size,
            expected_totals[spec.name],
            python or "python3",
        )
        results.append(
            {
                "tool": spec.name,
                "status": "ok",
                "version": python_tool_version(spec, python),
                "runtime_version": command_version(python),
                "iterations": iterations,
                "jobs": jobs if spec.name == "frost" else None,
                "source_count": size + 1,
                "artifact": artifact,
                "configuration": python_configuration_metrics(root, spec),
                "observed_summaries": summaries[spec.name][-3:],
                "scenarios": {
                    scenario: summarize(samples[spec.name][scenario])
                    for scenario in selected_scenarios
                },
            }
        )
    return results


def run_python_benchmark(args: argparse.Namespace) -> dict[str, Any]:
    tools = parse_csv(args.tools, valid=PYTHON_TOOLS)
    requested_scenarios = parse_csv(args.scenarios, valid=PYTHON_SCENARIOS)
    if not tools or not requested_scenarios:
        raise SystemExit("--tools and --scenarios must not be empty")
    if min(args.size, args.iterations, args.jobs) <= 0:
        raise SystemExit("--size, --iterations, and --jobs must be positive")
    selected_scenarios = tuple(
        scenario for scenario in PYTHON_SCENARIOS if scenario in requested_scenarios
    )
    if args.workdir:
        base_workdir = pathlib.Path(args.workdir).resolve()
        base_workdir.mkdir(parents=True, exist_ok=True)
        temp_context = None
    else:
        temp_context = tempfile.TemporaryDirectory(prefix="frost-python-bench-")
        base_workdir = pathlib.Path(temp_context.name)
    specs, python, build_reason = python_tool_specs(tools)
    try:
        results = measure_python_tools_interleaved(
            base_workdir,
            specs,
            python,
            build_reason,
            args.size,
            args.iterations,
            args.jobs,
            selected_scenarios,
        )
    finally:
        if temp_context is not None and not args.keep_workdir:
            temp_context.cleanup()
    successful = [result for result in results if result["status"] == "ok"]
    semantic_digests = {
        result["tool"]: result["artifact"]["semantic_sha256"]
        for result in successful
    }
    artifact_digests = {
        result["tool"]: result["artifact"]["artifact_sha256"]
        for result in successful
    }
    return {
        "schema": PYTHON_SCHEMA,
        "suite": "python",
        "generated_at": utc_now(),
        "environment": environment_snapshot(),
        "config": {
            "tools": tools,
            "size": args.size,
            "iterations": args.iterations,
            "jobs": args.jobs,
            "scenarios": list(selected_scenarios),
            "python": {"path": python, "version": command_version(python)},
            "build_backend": "setuptools.build_meta",
            "build_isolation": False,
            "source_date_epoch": 946684800,
            "execution_order": (
                "round-robin frontends; reverse order on every measured iteration"
            ),
            "clean_policy": (
                "delete wheel, backend build tree and egg-info; retain Python, "
                "installed build dependencies and uv cache"
            ),
        },
        "graph": python_graph_contract(args.size),
        "output_equivalence": {
            "comparison_unit": (
                "exact installed Python source names and bytes; Name/Version, "
                "py3-none-any tag, complete verified RECORD, and exact execution "
                "after extracting every timed wheel"
            ),
            "metadata_outside_contract": (
                "backend-specific optional metadata and archive layout may differ"
            ),
            "validated_after_every_timed_build": True,
            "semantic_digests": semantic_digests,
            "artifact_digests": artifact_digests,
            "semantic_equal": (
                len(semantic_digests) > 1
                and len(set(semantic_digests.values())) == 1
            ),
            "byte_identical": (
                len(artifact_digests) > 1
                and len(set(artifact_digests.values())) == 1
            ),
        },
        "results": results,
    }


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


def run_java_command(args: argparse.Namespace) -> int:
    report = run_java_benchmark(args)
    report["digest"] = report_digest(report)
    if args.out:
        write_report(pathlib.Path(args.out), report)
    print(json.dumps(report, indent=2, sort_keys=True))
    return 1 if any(result["status"] == "failed" for result in report["results"]) else 0


def run_rust_command(args: argparse.Namespace) -> int:
    report = run_rust_benchmark(args)
    report["digest"] = report_digest(report)
    if args.out:
        write_report(pathlib.Path(args.out), report)
    print(json.dumps(report, indent=2, sort_keys=True))
    return 1 if any(result["status"] == "failed" for result in report["results"]) else 0


def run_go_command(args: argparse.Namespace) -> int:
    report = run_go_benchmark(args)
    report["digest"] = report_digest(report)
    if args.out:
        write_report(pathlib.Path(args.out), report)
    print(json.dumps(report, indent=2, sort_keys=True))
    return 1 if any(result["status"] == "failed" for result in report["results"]) else 0


def run_typescript_command(args: argparse.Namespace) -> int:
    report = run_typescript_benchmark(args)
    report["digest"] = report_digest(report)
    if args.out:
        write_report(pathlib.Path(args.out), report)
    print(json.dumps(report, indent=2, sort_keys=True))
    return 1 if any(result["status"] == "failed" for result in report["results"]) else 0


def run_typescript_projects_command(args: argparse.Namespace) -> int:
    report = run_typescript_projects_benchmark(args)
    report["digest"] = report_digest(report)
    if args.out:
        write_report(pathlib.Path(args.out), report)
    print(json.dumps(report, indent=2, sort_keys=True))
    return 1 if any(result["status"] == "failed" for result in report["results"]) else 0


def run_python_command(args: argparse.Namespace) -> int:
    report = run_python_benchmark(args)
    report["digest"] = report_digest(report)
    if args.out:
        write_report(pathlib.Path(args.out), report)
    print(json.dumps(report, indent=2, sort_keys=True))
    return 1 if any(result["status"] == "failed" for result in report["results"]) else 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="FrostBuild benchmark harness")
    sub = parser.add_subparsers(dest="cmd", required=True)

    run_parser = sub.add_parser("run", help="run a benchmark suite")
    run_parser.add_argument("--suite", default="standard")
    run_parser.add_argument("--tools", default="ninja,make")
    run_parser.add_argument("--sizes", default="1000,10000")
    run_parser.add_argument(
        "--scenarios",
        default=",".join(STANDARD_SCENARIOS),
        help="comma-separated scenarios to run",
    )
    run_parser.add_argument("--iterations", type=int, default=5)
    run_parser.add_argument("--jobs", type=int, default=max(1, (os.cpu_count() or 4) // 2))
    run_parser.add_argument("--workdir", help="directory for generated benchmark workspaces")
    run_parser.add_argument("--keep-workdir", action="store_true")
    run_parser.add_argument("--out", help="write JSON report to this path as well as stdout")
    run_parser.set_defaults(func=run)

    java_parser = sub.add_parser(
        "java",
        help="compare Java builds through Frost unit/batch adapters, Gradle and Maven",
    )
    java_parser.add_argument("--tools", default=",".join(JAVA_DEFAULT_TOOLS))
    java_parser.add_argument("--size", type=int, default=100)
    java_parser.add_argument(
        "--scenarios",
        default=",".join(JAVA_SCENARIOS),
        help="comma-separated clean, noop, incremental_leaf scenarios",
    )
    java_parser.add_argument("--iterations", type=int, default=5)
    java_parser.add_argument(
        "--jobs",
        type=int,
        default=max(1, (os.cpu_count() or 4) // 2),
    )
    java_parser.add_argument("--workdir", help="directory for generated workspaces")
    java_parser.add_argument("--keep-workdir", action="store_true")
    java_parser.add_argument("--out", help="write JSON report to this path as well as stdout")
    java_parser.set_defaults(func=run_java_command)

    rust_parser = sub.add_parser(
        "rust",
        help="compare the same incremental rustc crate through Frost and Cargo",
    )
    rust_parser.add_argument("--tools", default=",".join(RUST_TOOLS))
    rust_parser.add_argument("--size", type=int, default=100)
    rust_parser.add_argument(
        "--scenarios",
        default=",".join(RUST_SCENARIOS),
        help="comma-separated clean, noop, incremental_leaf scenarios",
    )
    rust_parser.add_argument("--iterations", type=int, default=7)
    rust_parser.add_argument(
        "--jobs",
        type=int,
        default=max(1, (os.cpu_count() or 4) // 2),
    )
    rust_parser.add_argument("--workdir", help="directory for generated workspaces")
    rust_parser.add_argument("--keep-workdir", action="store_true")
    rust_parser.add_argument("--out", help="write JSON report to this path as well as stdout")
    rust_parser.set_defaults(func=run_rust_command)

    go_parser = sub.add_parser(
        "go",
        help="compare a Go package through Frost native/wrapper paths and go build",
    )
    go_parser.add_argument("--tools", default=",".join(GO_TOOLS))
    go_parser.add_argument("--size", type=int, default=100)
    go_parser.add_argument(
        "--scenarios",
        default=",".join(GO_SCENARIOS),
        help="comma-separated clean, noop, incremental_leaf scenarios",
    )
    go_parser.add_argument("--iterations", type=int, default=7)
    go_parser.add_argument(
        "--jobs",
        type=int,
        default=max(1, (os.cpu_count() or 4) // 2),
    )
    go_parser.add_argument("--workdir", help="directory for generated workspaces")
    go_parser.add_argument("--keep-workdir", action="store_true")
    go_parser.add_argument("--out", help="write JSON report to this path as well as stdout")
    go_parser.set_defaults(func=run_go_command)

    typescript_parser = sub.add_parser(
        "typescript",
        help="compare one incremental TypeScript project through Frost and native tsc",
    )
    typescript_parser.add_argument("--tools", default=",".join(TYPESCRIPT_TOOLS))
    typescript_parser.add_argument("--size", type=int, default=100)
    typescript_parser.add_argument(
        "--scenarios",
        default=",".join(TYPESCRIPT_SCENARIOS),
        help="comma-separated clean, noop, incremental_leaf scenarios",
    )
    typescript_parser.add_argument("--iterations", type=int, default=7)
    typescript_parser.add_argument(
        "--jobs",
        type=int,
        default=max(1, (os.cpu_count() or 4) // 2),
        help="maximum parallel Frost actions",
    )
    typescript_parser.add_argument(
        "--checkers",
        type=int,
        default=max(1, (os.cpu_count() or 4) // 2),
        help="native TypeScript 7 semantic checker workers used by both frontends",
    )
    typescript_parser.add_argument(
        "--frost-checkers",
        type=int,
        help="override --checkers for the Frost frontend after a recorded sweep",
    )
    typescript_parser.add_argument(
        "--tsc-checkers",
        type=int,
        help="override --checkers for the direct tsc frontend after a recorded sweep",
    )
    typescript_parser.add_argument("--workdir", help="directory for generated workspaces")
    typescript_parser.add_argument("--keep-workdir", action="store_true")
    typescript_parser.add_argument(
        "--out",
        help="write JSON report to this path as well as stdout",
    )
    typescript_parser.set_defaults(func=run_typescript_command)

    typescript_projects_parser = sub.add_parser(
        "typescript-projects",
        help="compare Frost action parallelism with a native tsc project-reference solution",
    )
    typescript_projects_parser.add_argument(
        "--tools",
        default=",".join(TYPESCRIPT_TOOLS),
    )
    typescript_projects_parser.add_argument("--projects", type=int, default=8)
    typescript_projects_parser.add_argument("--modules", type=int, default=25)
    typescript_projects_parser.add_argument(
        "--scenarios",
        default=",".join(TYPESCRIPT_SCENARIOS),
    )
    typescript_projects_parser.add_argument("--iterations", type=int, default=7)
    typescript_projects_parser.add_argument("--jobs", type=int, default=4)
    typescript_projects_parser.add_argument(
        "--frost-checkers",
        type=int,
        default=1,
        help="checker workers per Frost project process",
    )
    typescript_projects_parser.add_argument(
        "--tsc-checkers",
        type=int,
        default=max(1, (os.cpu_count() or 4) // 2),
        help="checker workers in native tsc --build",
    )
    typescript_projects_parser.add_argument("--workdir")
    typescript_projects_parser.add_argument("--keep-workdir", action="store_true")
    typescript_projects_parser.add_argument("--out")
    typescript_projects_parser.set_defaults(func=run_typescript_projects_command)

    python_parser = sub.add_parser(
        "python",
        help="compare standards-compliant pure-Python wheels through Frost, build and uv",
    )
    python_parser.add_argument("--tools", default=",".join(PYTHON_TOOLS))
    python_parser.add_argument("--size", type=int, default=100)
    python_parser.add_argument(
        "--scenarios",
        default=",".join(PYTHON_SCENARIOS),
        help="comma-separated clean, noop, incremental_leaf scenarios",
    )
    python_parser.add_argument("--iterations", type=int, default=7)
    python_parser.add_argument(
        "--jobs",
        type=int,
        default=max(1, (os.cpu_count() or 4) // 2),
        help="maximum parallel Frost actions (the fixture has one packaging action)",
    )
    python_parser.add_argument("--workdir")
    python_parser.add_argument("--keep-workdir", action="store_true")
    python_parser.add_argument("--out")
    python_parser.set_defaults(func=run_python_command)

    args = parser.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    raise SystemExit(main())
