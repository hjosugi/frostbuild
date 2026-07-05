from __future__ import annotations

import os
import pathlib
import shutil
import tempfile
import time
import unittest
from unittest import mock

import frost


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


if __name__ == "__main__":
    unittest.main()
