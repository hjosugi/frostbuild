from __future__ import annotations

import os
import pathlib
import shutil
import socket
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

        start = time.perf_counter()
        catalog = frost.build_metadata_catalog(ws)
        build_ms = (time.perf_counter() - start) * 1000

        start = time.perf_counter()
        changed = frost.catalog_changed_targets(catalog, ["src/t9999.fb"])
        query_ms = (time.perf_counter() - start) * 1000

        self.assertLess(build_ms, 100)
        self.assertLess(query_ms, 1)
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


if __name__ == "__main__":
    unittest.main()
