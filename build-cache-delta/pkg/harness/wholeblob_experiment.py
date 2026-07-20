#!/usr/bin/env python3
"""Neighbor-delta vs FastCDC dedup: transfer-size and CPU-time experiment.

Reproduces and extends the preliminary experiment in BUILD_CACHE_RESEARCH.md
(section 11): build N consecutive commits of lua/lua under two configurations,
then for every "cache miss with a same-path neighbor" measure the bytes a
remote cache would need to transfer under:

  zstd       per-blob zstd (Bazel --remote_cache_compression, level 3)
  cdc512k    FastCDC dedup, Bazel-default params (min128k/avg512k/max2M) + zstd
  cdc16k     FastCDC dedup, CDC-favorable params (min4k/avg16k/max64k) + zstd
  bsdiff     neighbor delta, bsdiff4 (suffix-sort, bzip2-framed)
  zstd_patch neighbor delta, zstd -19 --long=27 --patch-from (CLI)

Every neighbor-delta reconstruction is applied and digest-verified (the
soundness gate of the proposed design). CPU time (user+sys, self+children) is
recorded per operation — the memo's top unmeasured item.

CAS model: unlimited chunk retention over all artifacts of all previous
commits in the same configuration (CDC-favorable assumption, as in the memo).
Neighbor selection: trivial — same path, previous commit (selector (i)).
"""

import hashlib
import json
import resource
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

import bsdiff4
import zstandard
from fastcdc import fastcdc

ROOT = Path(__file__).resolve().parent.parent
CORPUS = ROOT / "corpus"
LUA = CORPUS / "lua"
ART = CORPUS / "artifacts"
RESULTS = ROOT / "results"

N_COMMITS = 40
CONFIGS = {
    "lua-O2": "-O2",
    "lua-g": "-g -O0",
}
# (min, avg, max); Bazel FastCdcChunker derives min = avg/4, max = avg*4.
CDC_PARAMS = {
    "cdc512k": (128 * 1024, 512 * 1024, 2048 * 1024),
    "cdc16k": (4 * 1024, 16 * 1024, 64 * 1024),
}
ZSTD_LEVEL = 3  # Bazel remote cache compression default
ZSTD_PATCH_ARGS = ["-19", "--single-thread", "--long=27"]


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def cpu_now() -> float:
    self_ru = resource.getrusage(resource.RUSAGE_SELF)
    kids_ru = resource.getrusage(resource.RUSAGE_CHILDREN)
    return (
        self_ru.ru_utime + self_ru.ru_stime + kids_ru.ru_utime + kids_ru.ru_stime
    )


def commit_list() -> list[str]:
    out = subprocess.run(
        ["git", "-C", str(LUA), "log", "--first-parent", f"-{N_COMMITS}",
         "--format=%H", "origin/master" if has_ref("origin/master") else "HEAD"],
        capture_output=True, text=True, check=True,
    ).stdout.split()
    out.reverse()  # oldest -> newest
    return out


def has_ref(ref: str) -> bool:
    return (
        subprocess.run(
            ["git", "-C", str(LUA), "rev-parse", "--verify", "-q", ref],
            capture_output=True,
        ).returncode
        == 0
    )


def build_corpus(config: str, flags: str, shas: list[str]) -> list[tuple[str, Path]]:
    outdir = ART / config
    outdir.mkdir(parents=True, exist_ok=True)
    meta = []
    for i, sha in enumerate(shas):
        dest = outdir / f"{i:03d}-{sha[:12]}"
        if (dest / ".done").exists():
            meta.append((sha, dest))
            continue
        subprocess.run(["git", "-C", str(LUA), "checkout", "-q", sha], check=True)
        subprocess.run(["make", "-C", str(LUA), "clean"],
                       check=True, capture_output=True)
        proc = subprocess.run(
            ["make", "-C", str(LUA), "-j8", "all", f"MYCFLAGS={flags}"],
            capture_output=True,
        )
        if proc.returncode != 0:
            print(f"  [{config}] build failed at {sha[:12]}; commit skipped")
            continue
        if dest.exists():
            shutil.rmtree(dest)
        dest.mkdir(parents=True)
        for p in sorted(LUA.iterdir()):
            if p.suffix in (".o", ".a") or p.name in ("lua", "luac"):
                shutil.copy2(p, dest / p.name)
        (dest / ".done").write_text(sha)
        meta.append((sha, dest))
        print(f"  [{config}] built {i + 1}/{len(shas)} {sha[:12]}", flush=True)
    return meta


def zstd_patch(old_path: Path, new_path: Path, tmp: Path) -> dict:
    """zstd --patch-from delta via CLI, with digest-verified apply."""
    patch = tmp / "patch.zst"
    restored = tmp / "restored"
    t = cpu_now()
    subprocess.run(
        ["zstd", *ZSTD_PATCH_ARGS, "-f", "-q",
         f"--patch-from={old_path}", str(new_path), "-o", str(patch)],
        check=True,
    )
    diff_cpu = cpu_now() - t
    t = cpu_now()
    subprocess.run(
        ["zstd", "-d", "-f", "-q", "--long=27",
         f"--patch-from={old_path}", str(patch), "-o", str(restored)],
        check=True,
    )
    apply_cpu = cpu_now() - t
    ok = sha256(restored.read_bytes()) == sha256(new_path.read_bytes())
    size = patch.stat().st_size
    patch.unlink()
    restored.unlink()
    return {"size": size, "diff_cpu": diff_cpu, "apply_cpu": apply_cpu,
            "verified": ok}


def chunk_file(path: Path, params: tuple[int, int, int]):
    mn, avg, mx = params
    return list(fastcdc(str(path), mn, avg, mx, False, hf=hashlib.sha256))


def analyze(config: str, meta: list[tuple[str, Path]]) -> dict:
    cctx = zstandard.ZstdCompressor(level=ZSTD_LEVEL)
    cas: dict[str, set] = {name: set() for name in CDC_PARAMS}
    misses = []
    prev: dict[str, Path] | None = None
    tmp = Path(tempfile.mkdtemp(prefix="cachebench-"))
    exact_hits = 0
    try:
        for idx, (sha, dest) in enumerate(meta):
            files = {p.name: p for p in sorted(dest.iterdir())
                     if p.name != ".done"}
            if prev is not None:
                for name, path in sorted(files.items()):
                    old_path = prev.get(name)
                    if old_path is None:
                        continue
                    new = path.read_bytes()
                    old = old_path.read_bytes()
                    if sha256(old) == sha256(new):
                        exact_hits += 1
                        continue
                    m = {"commit": sha[:12], "path": name, "raw": len(new)}
                    # (a) per-blob zstd — Bazel --remote_cache_compression
                    t = cpu_now()
                    z = cctx.compress(new)
                    m["zstd"] = {"size": len(z), "cpu": cpu_now() - t}
                    # (b) FastCDC dedup vs CAS of all previous commits + zstd
                    for cname, params in CDC_PARAMS.items():
                        t = cpu_now()
                        chunks = chunk_file(path, params)
                        fresh = [c for c in chunks
                                 if c.hash not in cas[cname]]
                        payload = b"".join(
                            new[c.offset:c.offset + c.length] for c in fresh)
                        zc = cctx.compress(payload) if payload else b""
                        m[cname] = {
                            "chunks": len(chunks),
                            "new_chunks": len(fresh),
                            "transfer": len(payload),
                            "transfer_zstd": len(zc),
                            "cpu": cpu_now() - t,
                        }
                    # (c) neighbor delta: bsdiff, digest-verified apply
                    t = cpu_now()
                    patch = bsdiff4.diff(old, new)
                    diff_cpu = cpu_now() - t
                    t = cpu_now()
                    restored = bsdiff4.patch(old, patch)
                    apply_cpu = cpu_now() - t
                    if sha256(restored) != sha256(new):
                        raise AssertionError(
                            f"bsdiff verify FAILED for {name} at {sha[:12]}")
                    m["bsdiff"] = {"size": len(patch), "diff_cpu": diff_cpu,
                                   "apply_cpu": apply_cpu, "verified": True}
                    # (d) neighbor delta: zstd --patch-from, digest-verified
                    m["zstd_patch"] = zstd_patch(old_path, path, tmp)
                    if not m["zstd_patch"]["verified"]:
                        raise AssertionError(
                            f"zstd_patch verify FAILED for {name} at {sha[:12]}")
                    misses.append(m)
            # Only after measuring this commit against the CAS of strictly
            # earlier commits do its own chunks enter the CAS.
            for name, path in files.items():
                for cname, params in CDC_PARAMS.items():
                    for c in chunk_file(path, params):
                        cas[cname].add(c.hash)
            prev = files
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
    return {"config": config, "commits": len(meta), "exact_hits": exact_hits,
            "misses": misses}


def summarize(report: dict) -> dict:
    misses = report["misses"]
    def tot(fn):
        return sum(fn(m) for m in misses)
    n = len(misses)
    one_chunk = sum(1 for m in misses if m["cdc512k"]["chunks"] == 1)
    s = {
        "config": report["config"],
        "commits": report["commits"],
        "misses": n,
        "exact_hits": report["exact_hits"],
        "one_chunk_rate_cdc512k": round(one_chunk / n, 3) if n else None,
        "raw_mb": round(tot(lambda m: m["raw"]) / 1e6, 2),
        "zstd_mb": round(tot(lambda m: m["zstd"]["size"]) / 1e6, 2),
        "cdc512k_zstd_mb": round(
            tot(lambda m: m["cdc512k"]["transfer_zstd"]) / 1e6, 2),
        "cdc16k_zstd_mb": round(
            tot(lambda m: m["cdc16k"]["transfer_zstd"]) / 1e6, 2),
        "bsdiff_mb": round(tot(lambda m: m["bsdiff"]["size"]) / 1e6, 2),
        "zstd_patch_mb": round(
            tot(lambda m: m["zstd_patch"]["size"]) / 1e6, 2),
        "cpu_s": {
            "zstd": round(tot(lambda m: m["zstd"]["cpu"]), 2),
            "cdc512k": round(tot(lambda m: m["cdc512k"]["cpu"]), 2),
            "cdc16k": round(tot(lambda m: m["cdc16k"]["cpu"]), 2),
            "bsdiff_diff": round(tot(lambda m: m["bsdiff"]["diff_cpu"]), 2),
            "bsdiff_apply": round(tot(lambda m: m["bsdiff"]["apply_cpu"]), 2),
            "zstd_patch_diff": round(
                tot(lambda m: m["zstd_patch"]["diff_cpu"]), 2),
            "zstd_patch_apply": round(
                tot(lambda m: m["zstd_patch"]["apply_cpu"]), 2),
        },
        "all_deltas_verified": all(
            m["bsdiff"]["verified"] and m["zstd_patch"]["verified"]
            for m in misses),
    }
    if s["cdc512k_zstd_mb"]:
        s["bsdiff_vs_cdc512k_pct"] = round(
            100 * s["bsdiff_mb"] / s["cdc512k_zstd_mb"], 1)
    if s["cdc16k_zstd_mb"]:
        s["bsdiff_vs_cdc16k_pct"] = round(
            100 * s["bsdiff_mb"] / s["cdc16k_zstd_mb"], 1)
    return s


def main() -> None:
    RESULTS.mkdir(exist_ok=True)
    shas = commit_list()
    print(f"corpus: lua/lua, {len(shas)} first-parent commits "
          f"{shas[0][:12]}..{shas[-1][:12]}")
    summaries = []
    for config, flags in CONFIGS.items():
        print(f"== {config} (MYCFLAGS={flags!r})")
        meta = build_corpus(config, flags, shas)
        report = analyze(config, meta)
        (RESULTS / f"{config}.json").write_text(json.dumps(report, indent=1))
        summary = summarize(report)
        summaries.append(summary)
        print(json.dumps(summary, indent=2))
    (RESULTS / "summary.json").write_text(json.dumps(summaries, indent=1))
    print("done; raw per-miss data in results/<config>.json")


if __name__ == "__main__":
    sys.exit(main())
