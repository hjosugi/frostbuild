from __future__ import annotations

import os
import json
import pathlib
import shutil
import socket
import statistics
import tempfile
import time
import unittest
import dataclasses
from types import SimpleNamespace
from unittest import mock

import frost
import frost_bench


class FrostTestCase(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.root = pathlib.Path(self.tmp.name)
        (self.root / "src").mkdir()
        (self.root / "src/lib.fb").write_text("EXPORT lib\n", encoding="utf-8")
        (self.root / "src/app.fb").write_text("EXPORT app\nIMPORT lib\n", encoding="utf-8")
        (self.root / "frost.toml").write_text(
            """
[workspace]
toolchain = "test-toolchain-v1"
default_targets = ["app"]

[target.lib]
kind = "build"
src = "src/lib.fb"
deps = []
out = ".frost/out/lib.out"
cost_ms = 1

[target.app]
kind = "build"
src = "src/app.fb"
deps = ["lib"]
out = ".frost/out/app.out"
cost_ms = 1
""".strip()
            + "\n",
            encoding="utf-8",
        )
        self.ws = frost.Workspace(self.root)

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def test_action_key_normalizes_environment_and_cwd(self) -> None:
        target = self.ws.targets["lib"]

        key_a = frost.action_key(
            self.ws,
            target,
            env={"IGNORED": "a", "FROSTBUILD_FLAGS": "--release"},
            cwd=self.root,
        )
        key_b = frost.action_key(
            self.ws,
            target,
            env={"FROSTBUILD_FLAGS": "--release", "IGNORED": "b"},
            cwd=self.root / ".",
        )
        key_c = frost.action_key(
            self.ws,
            target,
            env={"FROSTBUILD_FLAGS": "--debug"},
            cwd=self.root,
        )

        self.assertEqual(key_a, key_b)
        self.assertNotEqual(key_a, key_c)

    def test_output_clean_rebuild_is_full_cache_hit(self) -> None:
        selected = set(self.ws.targets)
        warm = frost.execute_plan(self.ws, selected, jobs=2)
        self.assertEqual(warm["executed"], 2)
        self.assertEqual(warm["cache_hit"], 0)

        shutil.rmtree(self.ws.out_dir)
        rebuild = frost.execute_plan(self.ws, selected, jobs=2)

        self.assertEqual(rebuild["executed"], 0)
        self.assertEqual(rebuild["cache_hit"], 2)
        self.assertLess(rebuild["cache_lookup_latency_ms_p50"], 1.0)

    def test_flag_change_misses_action_cache(self) -> None:
        selected = {"lib"}
        with mock.patch.dict(os.environ, {"FROSTBUILD_FLAGS": "--release"}, clear=False):
            warm = frost.execute_plan(self.ws, selected, jobs=1)
        self.assertEqual(warm["executed"], 1)

        self.ws.output_path("lib").unlink()
        with mock.patch.dict(os.environ, {"FROSTBUILD_FLAGS": "--debug"}, clear=False):
            rebuild = frost.execute_plan(self.ws, selected, jobs=1)

        self.assertEqual(rebuild["executed"], 1)
        self.assertEqual(rebuild["cache_hit"], 0)

    def test_metadata_catalog_indexes_and_reverse_closure_match_graph(self) -> None:
        catalog = frost.build_metadata_catalog(self.ws)

        self.assertFalse(frost.metadata_catalog_is_stale(self.ws, catalog))
        self.assertEqual(catalog["indexes"]["source_to_partitions"]["src/app.fb"], ["app"])
        self.assertEqual(catalog["indexes"]["source_to_targets"]["src/lib.fb"], ["lib"])
        self.assertEqual(frost.catalog_changed_targets(catalog, ["src/lib.fb"]), {"lib"})
        self.assertEqual(
            frost.catalog_reverse_closure(catalog, {"lib"}),
            self.ws.reverse_closure({"lib"}),
        )

    def test_metadata_catalog_scales_to_10k_targets(self) -> None:
        ws = object.__new__(frost.Workspace)
        ws.root = pathlib.Path("/")
        ws.toolchain = "synthetic-toolchain-v1"
        ws.default_targets = ("t9999",)
        ws.targets = {}
        for i in range(10_000):
            name = f"t{i}"
            deps = (f"t{i - 1}",) if i else ()
            ws.targets[name] = frost.Target(
                name=name,
                kind="build",
                src=f"src/{name}.fb",
                deps=deps,
                out=f".frost/out/{name}.out",
                cost_ms=1,
            )

        # A single wall-clock sample is too sensitive to shared-runner scheduling.
        # This correctness suite keeps a generous catastrophic-regression ceiling;
        # the 100 ms performance target belongs to the dedicated benchmark job,
        # which records host metadata and compares like-for-like baselines.
        build_samples = []
        query_samples = []
        catalogs = []
        changed = set()
        for _ in range(5):
            start = time.perf_counter()
            catalog = frost.build_metadata_catalog(ws)
            build_samples.append((time.perf_counter() - start) * 1000)
            catalogs.append(catalog)

            start = time.perf_counter()
            changed = frost.catalog_changed_targets(catalog, ["src/t9999.fb"])
            query_samples.append((time.perf_counter() - start) * 1000)

        self.assertLess(statistics.median(build_samples), 250)
        self.assertLess(statistics.median(query_samples), 1)
        catalog = catalogs[-1]
        self.assertEqual(changed, {"t9999"})
        self.assertEqual(
            frost.catalog_reverse_closure(catalog, {"t9998"}),
            ws.reverse_closure({"t9998"}),
        )

    def test_stat_cache_skips_hash_when_stat_matches(self) -> None:
        first = frost.all_source_hashes(self.ws)
        self.assertIn("src/lib.fb", first)

        with mock.patch("frost.sha256_file", side_effect=AssertionError("hash should be cached")):
            second = frost.all_source_hashes(self.ws)

        self.assertEqual(first, second)

    def test_depfile_ingestion_records_dynamic_inputs(self) -> None:
        header = self.root / "src/lib.h"
        header.write_text("#define LIB 1\n", encoding="utf-8")
        depfile = self.root / ".frost/out/lib.d"
        depfile.parent.mkdir(parents=True, exist_ok=True)
        depfile.write_text(".frost/out/lib.out: src/lib.fb src/lib.h\n", encoding="utf-8")
        self.ws.targets["lib"] = dataclasses.replace(self.ws.targets["lib"], depfile=".frost/out/lib.d")

        result = frost.execute_plan(self.ws, {"lib"}, jobs=1)

        self.assertEqual(result["executed"], 1)
        self.assertEqual(frost.load_dynamic_deps(self.ws)["lib"], ["src/lib.fb", "src/lib.h"])

    def test_sandbox_denies_undeclared_read(self) -> None:
        (self.root / "src/lib.fb").write_text("EXPORT lib\nREAD secrets.txt\n", encoding="utf-8")

        with self.assertRaises(frost.ActionExecutionError):
            frost.execute_plan(self.ws, {"lib"}, jobs=1, sandbox=True)

    def test_keep_going_runs_independent_work_after_failure(self) -> None:
        (self.root / "src/lib.fb").write_text("EXPORT lib\nFAIL\n", encoding="utf-8")
        (self.root / "src/other.fb").write_text("EXPORT other\n", encoding="utf-8")
        self.ws.targets["other"] = frost.Target(
            name="other",
            kind="build",
            src="src/other.fb",
            deps=(),
            out=".frost/out/other.out",
            cost_ms=1,
        )

        result = frost.execute_plan(self.ws, {"lib", "other"}, jobs=2, keep_going=True)

        self.assertEqual(result["failed"], 1)
        self.assertEqual(result["executed"], 1)

    def test_comment_only_change_reexecutes_leaf_but_downstream_is_cache_hit(self) -> None:
        frost.execute_plan(self.ws, {"lib", "app"}, jobs=1)
        (self.root / "src/lib.fb").write_text("EXPORT lib\n# comment only\n", encoding="utf-8")
        plan = frost.build_plan(self.ws, {"app"}, {"build"}, explicit_changed=None, force_all=False)

        result = frost.execute_plan(self.ws, plan.selected_targets, jobs=1)

        self.assertEqual(plan.selected_targets, {"lib", "app"})
        self.assertEqual(result["executed"], 1)
        self.assertEqual(result["cache_hit"], 1)

    def test_affected_test_selection_uses_test_roots(self) -> None:
        (self.root / "tests").mkdir()
        (self.root / "tests/lib.fbtest").write_text("TEST lib\nIMPORT lib\n", encoding="utf-8")
        self.ws.targets["lib_test"] = frost.Target(
            name="lib_test",
            kind="test",
            src="tests/lib.fbtest",
            deps=("lib",),
            out=".frost/out/lib_test.ok",
            cost_ms=1,
        )
        frost.execute_plan(self.ws, {"lib", "app", "lib_test"}, jobs=1)
        (self.root / "src/lib.fb").write_text("EXPORT lib\nVALUE changed\n", encoding="utf-8")

        plan = frost.build_plan(self.ws, frost.test_roots(self.ws, None), {"build", "test"}, None, False)

        self.assertIn("lib_test", plan.selected_targets)
        self.assertIn("lib", plan.selected_targets)
        self.assertNotIn("app", plan.selected_targets)

    def test_toolchain_compiler_hash_changes_action_key(self) -> None:
        compiler = self.root / "cc"
        compiler.write_text("compiler v1\n", encoding="utf-8")
        self.ws.toolchain_compiler_path = "cc"
        key_a = frost.action_key(self.ws, self.ws.targets["lib"])

        compiler.write_text("compiler version 2 with different size\n", encoding="utf-8")
        key_b = frost.action_key(self.ws, self.ws.targets["lib"])

        self.assertNotEqual(key_a, key_b)

    def test_graph_dot_and_ninja_importer(self) -> None:
        dot = frost.graph_dot(self.ws, {"app"})
        self.assertIn('"lib" -> "app"', dot)

        ninja = self.root / "build.ninja"
        ninja.write_text(
            "rule cc\n  command = cc -c $in -o $out\n"
            "build out/lib.o: cc src/lib.c\n"
            "build out/app.o: cc out/lib.o src/app.c\n"
            "default out/app.o\n",
            encoding="utf-8",
        )
        parsed = frost.parse_ninja_subset(ninja)
        toml = frost.ninja_subset_to_frost_toml(parsed)

        self.assertIn("target.out_lib_o", toml)
        self.assertIn('deps = ["out_lib_o"]', toml)

    def test_daemon_frame_round_trip(self) -> None:
        left, right = socket.socketpair()
        try:
            frost.send_frame(left, {"ok": True})
            self.assertEqual(frost.recv_frame(right), {"ok": True})
        finally:
            left.close()
            right.close()


class JobserverTestCase(unittest.TestCase):
    def test_parses_pipe_auth_and_returns_tokens(self) -> None:
        read_fd, write_fd = os.pipe()
        try:
            os.write(write_fd, frost.TOKEN_BYTE * 2)
            lease = frost.JobserverLease.from_environment(
                4,
                env={"MAKEFLAGS": f"--jobserver-auth={read_fd},{write_fd} -j"},
            )
            self.assertEqual(lease.mode, "client")
            self.assertEqual(lease.effective_jobs, 3)
            self.assertEqual(lease.borrowed_tokens, 2)

            lease.close()
            self.assertEqual(os.read(read_fd, 2), frost.TOKEN_BYTE * 2)
        finally:
            os.close(read_fd)
            os.close(write_fd)

    def test_server_exports_child_makeflags(self) -> None:
        lease = frost.JobserverLease.server(3)
        try:
            child_env = lease.child_env({"MAKEFLAGS": "w"})
            self.assertIn("--jobserver-auth=", child_env["MAKEFLAGS"])
            self.assertIn("-j", child_env["MAKEFLAGS"])
            self.assertTrue(os.get_inheritable(lease.server_read_fd))
            self.assertTrue(os.get_inheritable(lease.server_write_fd))
        finally:
            lease.close()


class FrostBenchTestCase(unittest.TestCase):
    def test_parse_sizes_rejects_non_positive_values(self) -> None:
        with self.assertRaises(Exception):
            frost_bench.parse_sizes("1000,0")

    def test_bazel_and_frost_describe_the_same_action_chain(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            frost_bench.generate_workspace(root, 4)
            manifest = (root / "frost.toml").read_text(encoding="utf-8")
            bazel = (root / "BUILD.bazel").read_text(encoding="utf-8")

        for index in range(4):
            name = frost_bench.target_name(index)
            self.assertIn(f"[target.{name}]", manifest)
            self.assertIn(f'name = "{name}"', bazel)
        self.assertEqual(manifest.count('kind = "genrule"'), 4)
        self.assertEqual(bazel.count("genrule("), 4)
        self.assertEqual(
            frost_bench.graph_contract(4),
            {
                "shape": "linear-chain",
                "action_count": 4,
                "dependency_edge_count": 3,
                "edge_digest_sha256": "cacb186fa9fdae8bc99d9b7ac4d473f61b75f929bdc00fb7ab2be45c102e9217",
                "per_action_source_inputs": ["src/nodeNNNNN.txt", "include/hot.h"],
                "manifests_verified_equivalent": True,
            },
        )

    def test_java_unit_and_batch_manifests_have_the_same_artifact_contract(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            base = pathlib.Path(tmp)
            unit = base / "unit"
            batch = base / "batch"
            frost_bench.generate_java_workspace(unit, 4, "frost-unit")
            frost_bench.generate_java_workspace(batch, 4, "frost-batch")

            with (unit / "frost.toml").open("rb") as file:
                import tomllib

                unit_manifest = tomllib.load(file)
            with (batch / "frost.toml").open("rb") as file:
                batch_manifest = tomllib.load(file)

        unit_outputs = sorted(
            output
            for target in unit_manifest["target"].values()
            for output in target["outputs"]
        )
        batch_outputs = sorted(
            output
            for target in batch_manifest["target"].values()
            for output in target["outputs"]
        )
        self.assertEqual(unit_outputs, batch_outputs)
        self.assertEqual(len(unit_manifest["target"]), 4)
        self.assertEqual(len(batch_manifest["target"]), 1)
        self.assertEqual(
            frost_bench.java_graph_contract(4)["required_class_count"],
            4,
        )

    def test_java_jar_manifest_is_one_concise_multi_step_artifact(self) -> None:
        missing_frost = frost_bench.ToolSpec(name="frost", argv=())
        with tempfile.TemporaryDirectory() as tmp, mock.patch(
            "frost_bench.tool_specs", return_value=[missing_frost]
        ):
            root = pathlib.Path(tmp)
            frost_bench.generate_java_workspace(root, 4, "frost-jar")
            with (root / "frost.toml").open("rb") as file:
                import tomllib

                manifest = tomllib.load(file)

        self.assertEqual(list(manifest["target"]), ["archive"])
        target = manifest["target"]["archive"]
        self.assertEqual(target["inputs"], ["src/main/java/**/*.java"])
        self.assertEqual(
            target["outputs"],
            [".frost/out/${config}/java-bench.jar"],
        )
        self.assertEqual(target["clean_dirs"], [".frost/tmp/${config}/java-classes"])
        self.assertEqual(target["steps"][0]["tool"], "pack_jar")
        self.assertEqual(manifest["toolchain"]["tools"]["pack_jar"], "frost")

    def test_java_suite_records_missing_tools_instead_of_silently_omitting_them(self) -> None:
        with tempfile.TemporaryDirectory() as tmp, mock.patch.dict(
            os.environ,
            {"GRADLE_BIN": ""},
        ), mock.patch("frost_bench.shutil.which", return_value=None):
            report = frost_bench.run_java_benchmark(
                SimpleNamespace(
                    tools="gradle",
                    size=2,
                    scenarios="noop",
                    iterations=1,
                    jobs=1,
                    workdir=tmp,
                    keep_workdir=True,
                )
            )

        self.assertEqual(report["schema"], frost_bench.JAVA_SCHEMA)
        self.assertEqual(
            report["config"]["execution_order"],
            "round-robin; reverse frontend order on every measured iteration",
        )
        self.assertEqual(report["results"][0]["status"], "skipped")
        self.assertIn("was not found", report["results"][0]["reason"])

    def test_rust_frost_and_cargo_workspaces_have_the_same_crate_contract(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            base = pathlib.Path(tmp)
            frost_root = base / "frost"
            cargo_root = base / "cargo"
            frost_bench.generate_rust_workspace(
                frost_root,
                4,
                "frost",
                "/toolchains/rustc",
            )
            frost_bench.generate_rust_workspace(cargo_root, 4, "cargo")
            frost_sources = sorted(
                path.relative_to(frost_root).as_posix()
                for path in (frost_root / "src").glob("*.rs")
            )
            cargo_sources = sorted(
                path.relative_to(cargo_root).as_posix()
                for path in (cargo_root / "src").glob("*.rs")
            )
            with (frost_root / "frost.toml").open("rb") as file:
                import tomllib

                manifest = tomllib.load(file)
            cargo_manifest = (cargo_root / "Cargo.toml").read_text(encoding="utf-8")

        self.assertEqual(frost_sources, cargo_sources)
        self.assertEqual(manifest["toolchain"]["tools"]["rustc"], "/toolchains/rustc")
        self.assertEqual(manifest["target"]["binary"]["inputs"], ["src/**/*.rs"])
        self.assertIn("incremental = true", cargo_manifest)
        self.assertEqual(
            frost_bench.rust_graph_contract(4)["initial_expected_stdout"],
            "6",
        )

    def test_rust_suite_records_a_missing_compiler_for_every_frontend(self) -> None:
        with tempfile.TemporaryDirectory() as tmp, mock.patch.dict(
            os.environ,
            {"RUSTC_BIN": "", "CARGO_BIN": ""},
        ), mock.patch("frost_bench.shutil.which", return_value=None):
            report = frost_bench.run_rust_benchmark(
                SimpleNamespace(
                    tools="frost,cargo",
                    size=2,
                    scenarios="noop",
                    iterations=1,
                    jobs=1,
                    workdir=tmp,
                    keep_workdir=True,
                )
            )

        self.assertEqual(report["schema"], frost_bench.RUST_SCHEMA)
        self.assertEqual(
            report["config"]["execution_order"],
            "round-robin; reverse frontend order on every measured iteration",
        )
        self.assertEqual(
            [result["status"] for result in report["results"]],
            ["skipped", "skipped"],
        )
        self.assertTrue(
            all(
                "rustc executable was not found" in result["reason"]
                for result in report["results"]
            )
        )

    def test_go_frontends_share_sources_and_native_declares_toolchain_closure(self) -> None:
        info = {
            "GOARCH": "amd64",
            "GOAMD64": "v1",
            "GOOS": "linux",
            "GOROOT": "/toolchains/go",
            "GOTOOLDIR": "/toolchains/go/pkg/tool/linux_amd64",
            "GOVERSION": "go1.26.4",
        }
        with tempfile.TemporaryDirectory() as tmp:
            base = pathlib.Path(tmp)
            roots = {
                tool: base / tool
                for tool in ("frost-native", "frost-go", "go")
            }
            for tool, root in roots.items():
                frost_bench.generate_go_workspace(
                    root,
                    4,
                    tool,
                    2,
                    None,
                    info,
                )
            source_sets = {
                tool: sorted(path.name for path in root.glob("*.go"))
                for tool, root in roots.items()
            }
            with (roots["frost-native"] / "frost.toml").open("rb") as file:
                import tomllib

                native = tomllib.load(file)
            with (roots["frost-go"] / "frost.toml").open("rb") as file:
                wrapper = tomllib.load(file)

        self.assertEqual(len({tuple(paths) for paths in source_sets.values()}), 1)
        self.assertEqual(native["target"]["binary"]["tool"], "compile")
        self.assertEqual(native["target"]["binary"]["steps"][0]["tool"], "link")
        self.assertIn(".go-sdk/**/*", native["target"]["binary"]["inputs"])
        self.assertEqual(wrapper["target"]["binary"]["tool"], "go")
        self.assertEqual(
            frost_bench.go_graph_contract(4)["initial_expected_stderr"],
            "6",
        )

    def test_typescript_frontends_share_sources_and_preserve_incremental_outputs(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            base = pathlib.Path(tmp)
            frost_root = base / "frost"
            tsc_root = base / "tsc"
            frost_bench.generate_typescript_workspace(
                frost_root,
                4,
                "frost",
                checkers=3,
            )
            frost_bench.generate_typescript_workspace(tsc_root, 4, "tsc")
            source_sets = [
                sorted(path.name for path in (root / "src").glob("*.ts"))
                for root in (frost_root, tsc_root)
            ]
            with (frost_root / "frost.toml").open("rb") as file:
                import tomllib

                manifest = tomllib.load(file)

        self.assertEqual(source_sets[0], source_sets[1])
        action = manifest["target"]["javascript"]
        self.assertTrue(action["preserve_outputs"])
        self.assertEqual(action["outputs"], frost_bench.typescript_frost_outputs(4))
        self.assertIn("typescript-sdk/lib/lib*.d.ts", action["inputs"])
        self.assertIn("3", action["args"])
        self.assertEqual(
            frost_bench.typescript_graph_contract(4)["initial_expected_stdout"],
            "6",
        )

    def test_typescript_suite_records_missing_native_compiler(self) -> None:
        with tempfile.TemporaryDirectory() as tmp, mock.patch.dict(
            os.environ,
            {"TSC_BIN": "", "NODE_BIN": ""},
        ), mock.patch("frost_bench.shutil.which", return_value=None):
            report = frost_bench.run_typescript_benchmark(
                SimpleNamespace(
                    tools="frost,tsc",
                    size=2,
                    scenarios="noop",
                    iterations=1,
                    jobs=1,
                    checkers=1,
                    workdir=tmp,
                    keep_workdir=True,
                )
            )

        self.assertEqual(report["schema"], frost_bench.TYPESCRIPT_SCHEMA)
        self.assertEqual(
            [result["status"] for result in report["results"]],
            ["skipped", "skipped"],
        )
        self.assertTrue(
            all(
                "TypeScript 7 compiler was not found" in result["reason"]
                for result in report["results"]
            )
        )

    def test_typescript_project_solution_declares_parallel_incremental_boundaries(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            base = pathlib.Path(tmp)
            compiler = base / "compiler/lib/tsc"
            compiler.parent.mkdir(parents=True)
            compiler.write_bytes(b"\x7fELF-test-compiler")
            (compiler.parent / "lib.es2022.d.ts").write_text(
                "interface Array<T> {}\n",
                encoding="utf-8",
            )
            root = base / "workspace"
            frost_bench.generate_typescript_solution(
                root,
                projects=3,
                modules=2,
                tool="frost",
                tsc=compiler.as_posix(),
                checkers=1,
            )
            with (root / "frost.toml").open("rb") as file:
                import tomllib

                manifest = tomllib.load(file)
            solution = json.loads((root / "tsconfig.json").read_text(encoding="utf-8"))

        self.assertEqual(len(manifest["target"]), 3)
        self.assertTrue(
            all(target["preserve_outputs"] for target in manifest["target"].values())
        )
        self.assertEqual(len(solution["references"]), 3)
        self.assertIn(
            ".frost/typescript/${config}/project000.tsbuildinfo",
            manifest["target"]["project000"]["outputs"],
        )

    def test_python_frontends_share_a_standard_wheel_contract(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            base = pathlib.Path(tmp)
            roots = {name: base / name for name in frost_bench.PYTHON_TOOLS}
            for name, root in roots.items():
                frost_bench.generate_python_workspace(
                    root,
                    size=4,
                    tool=name,
                    frost="/tmp/frost",
                )
            source_sets = {
                name: sorted(
                    path.relative_to(root).as_posix()
                    for path in (root / "src").rglob("*.py")
                )
                for name, root in roots.items()
            }
            with (roots["frost"] / "frost.toml").open("rb") as file:
                import tomllib

                manifest = tomllib.load(file)

        self.assertEqual(len({tuple(paths) for paths in source_sets.values()}), 1)
        action = manifest["target"]["wheel"]
        self.assertEqual(action["tool"], "pack_wheel")
        self.assertIn("pack-wheel", action["args"])
        self.assertEqual(
            action["outputs"],
            [f".frost/out/${{config}}/{frost_bench.PYTHON_WHEEL}"],
        )
        self.assertEqual(frost_bench.python_graph_contract(4)["source_count"], 5)

    def test_python_suite_records_missing_tools(self) -> None:
        with tempfile.TemporaryDirectory() as tmp, mock.patch.dict(
            os.environ,
            {"PYTHON_BIN": "", "UV_BIN": "", "FROST_BIN": ""},
        ), mock.patch("frost_bench.shutil.which", return_value=None):
            report = frost_bench.run_python_benchmark(
                SimpleNamespace(
                    tools=",".join(frost_bench.PYTHON_TOOLS),
                    size=2,
                    scenarios="noop",
                    iterations=1,
                    jobs=1,
                    workdir=tmp,
                    keep_workdir=True,
                )
            )

        self.assertEqual(report["schema"], frost_bench.PYTHON_SCHEMA)
        self.assertEqual(
            [result["status"] for result in report["results"]],
            ["skipped", "skipped", "skipped"],
        )

    @unittest.skipUnless(shutil.which("ninja") or shutil.which("make"), "requires ninja or make")
    def test_standard_suite_reports_median_scenarios(self) -> None:
        tool = "ninja" if shutil.which("ninja") else "make"
        with tempfile.TemporaryDirectory() as tmp:
            report = frost_bench.run_standard(
                SimpleNamespace(
                    suite="standard",
                    tools=tool,
                    sizes="3",
                    iterations=1,
                    jobs=1,
                    workdir=tmp,
                    keep_workdir=True,
                ),
            )

        self.assertEqual(report["schema"], frost_bench.SCHEMA)
        self.assertEqual(report["config"]["scenarios"], list(frost_bench.STANDARD_SCENARIOS))
        self.assertEqual(len(report["results"]), 1)
        scenarios = report["results"][0]["scenarios"]
        self.assertGreaterEqual(scenarios["clean"]["median_ms"], 0)
        self.assertGreaterEqual(scenarios["noop"]["median_ms"], 0)
        self.assertGreaterEqual(scenarios["incremental_leaf"]["median_ms"], 0)
        self.assertGreaterEqual(scenarios["hot_header"]["median_ms"], 0)
        self.assertFalse(scenarios["cache_hit_rebuild"]["applicable"])

    @unittest.skipUnless(shutil.which("ninja") or shutil.which("make"), "requires ninja or make")
    def test_standard_suite_can_run_only_the_requested_scenario(self) -> None:
        tool = "ninja" if shutil.which("ninja") else "make"
        with tempfile.TemporaryDirectory() as tmp:
            report = frost_bench.run_standard(
                SimpleNamespace(
                    suite="standard",
                    tools=tool,
                    sizes="3",
                    scenarios="noop",
                    iterations=1,
                    jobs=1,
                    workdir=tmp,
                    keep_workdir=True,
                ),
            )

        self.assertEqual(report["config"]["scenarios"], ["noop"])
        self.assertEqual(list(report["results"][0]["scenarios"]), ["noop"])
        self.assertGreaterEqual(
            report["results"][0]["scenarios"]["noop"]["median_ms"], 0
        )


if __name__ == "__main__":
    unittest.main()
