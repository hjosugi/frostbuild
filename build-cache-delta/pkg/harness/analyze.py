#!/usr/bin/env python3
"""
Replay a corpus of build artifacts commit by commit and measure how many bytes
each remote-cache transport scheme would move.

Model
-----
At commit i the client wants artifact A. It already holds everything produced
at commits 1..i-1 in its local CAS. The remote action cache is assumed to hit,
so the expected digest of A is known. The only question is how many bytes must
cross the network to materialize A locally.

Schemes
  raw       full blob                                (no compression)
  zstd      per-blob zstd                            (Bazel --remote_cache_compression)
  cdc       FastCDC 2020 split, fetch missing chunks (Bazel --experimental_remote_cache_chunking)
  cdc+zstd  same, chunks zstd-compressed             (realistic combination)
  bsdiff    binary delta against previous version    (proposed)
  zdelta    zstd raw-dictionary delta against prev   (proposed, cheaper)

Every scheme ends in an exact digest check, so all are equally sound. The only
axis of comparison is bytes moved.
"""

import argparse
import collections
import hashlib
import json
import os
import sys
import time

import bsdiff4
import zstandard as zstd

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from fastcdc import FastCdcChunker

ZLEVEL = 3  # Bazel uses zstd default-ish levels for cache compression


def sha(b):
    return hashlib.sha256(b).hexdigest()


def zstd_size(data, level=ZLEVEL):
    return len(zstd.ZstdCompressor(level=level).compress(data))


def zdelta_size(ref, target, level=ZLEVEL):
    """zstd delta using the reference as a raw-content dictionary.

    Mirrors what `zstd --patch-from` does. window_log is raised so the whole
    reference stays addressable for long-range matching.
    """
    if not ref:
        return zstd_size(target, level)
    wlog = max(20, (max(len(ref), len(target)) - 1).bit_length() + 1)
    wlog = min(wlog, 30)
    d = zstd.ZstdCompressionDict(ref, dict_type=zstd.DICT_TYPE_RAWCONTENT)
    c = zstd.ZstdCompressor(level=level, dict_data=d,
                            compression_params=zstd.ZstdCompressionParameters(
                                window_log=wlog, enable_ldm=1))
    return len(c.compress(target))


def bsdiff_size(ref, target):
    if not ref:
        return None
    return len(bsdiff4.diff(ref, target))


def load_corpus(root):
    """[(seq, sha, {artifact_name: bytes})] ordered oldest first."""
    out = []
    for d in sorted(os.listdir(root)):
        p = os.path.join(root, d)
        if not os.path.isdir(p):
            continue
        files = {}
        for f in sorted(os.listdir(p)):
            if f.startswith("."):
                continue
            files[f] = open(os.path.join(p, f), "rb").read()
        if files:
            out.append((d, open(os.path.join(p, ".sha")).read().strip()[:12], files))
    return out


def classify(name):
    if name.endswith(".o"):
        return ".o"
    if name.endswith(".a"):
        return ".a"
    return "bin"


def run(root, avg_kib, do_bsdiff=True, limit=None):
    chunker = FastCdcChunker(avg_size=avg_kib * 1024)
    commits = load_corpus(root)
    if limit:
        commits = commits[:limit]
    print(f"[analyze] {root}: {len(commits)} commits, "
          f"FastCDC avg={avg_kib}KiB min={chunker.min // 1024}KiB "
          f"max={chunker.max // 1024}KiB", flush=True)

    cas_blobs = set()          # exact blob digests the client already holds
    cas_chunks = set()         # chunk digests the client already holds
    prev_blob = {}             # artifact name -> most recent bytes

    rows = []
    t0 = time.time()

    for ci, (seq, csha, files) in enumerate(commits):
        for name, data in sorted(files.items()):
            h = sha(data)
            kind = classify(name)

            chunks = chunker.digests(data)
            n_chunks = len(chunks)

            if h in cas_blobs:
                rows.append(dict(commit=ci, name=name, kind=kind, size=len(data),
                                 exact_hit=True, n_chunks=n_chunks))
                # a repeat blob still contributes its chunks
                for cd, _ in chunks:
                    cas_chunks.add(cd)
                prev_blob[name] = data
                continue

            # --- cache miss: measure each transport scheme ---
            off = 0
            cdc_missing = 0
            cdc_missing_z = 0
            for (cd, ln) in chunks:
                if cd not in cas_chunks:
                    cdc_missing += ln
                    cdc_missing_z += zstd_size(data[off:off + ln])
                off += ln

            ref = prev_blob.get(name)
            row = dict(
                commit=ci, name=name, kind=kind, size=len(data),
                exact_hit=False, n_chunks=n_chunks,
                raw=len(data),
                zstd=zstd_size(data),
                cdc=cdc_missing,
                cdc_zstd=cdc_missing_z,
                has_ref=ref is not None,
                zdelta=zdelta_size(ref, data) if ref else None,
                bsdiff=(bsdiff_size(ref, data) if (ref and do_bsdiff) else None),
            )
            rows.append(row)

            cas_blobs.add(h)
            for cd, _ in chunks:
                cas_chunks.add(cd)
            prev_blob[name] = data

        if (ci + 1) % 10 == 0:
            print(f"[analyze]   commit {ci + 1}/{len(commits)} "
                  f"({time.time() - t0:.0f}s)", flush=True)

    return rows


def summarize(rows, label):
    miss = [r for r in rows if not r["exact_hit"] and r.get("has_ref")]
    hits = [r for r in rows if r["exact_hit"]]
    first = [r for r in rows if not r["exact_hit"] and not r.get("has_ref")]

    print(f"\n{'=' * 72}\n{label}\n{'=' * 72}")
    print(f"artifact instances : {len(rows)}")
    print(f"  exact CAS hits   : {len(hits)}  ({100 * len(hits) / len(rows):.1f}%)")
    print(f"  first appearance : {len(first)}")
    print(f"  misses w/ prev   : {len(miss)}  <- the measurable population")
    if not miss:
        return {}

    def tot(k):
        return sum(r[k] for r in miss if r.get(k) is not None)

    base = tot("raw")
    schemes = ["raw", "zstd", "cdc", "cdc_zstd", "zdelta", "bsdiff"]
    print(f"\ntransferred bytes over {len(miss)} cache misses "
          f"(total artifact size {base:,} B)\n")
    print(f"  {'scheme':<10} {'bytes':>14} {'vs raw':>9} {'vs zstd':>9} {'vs cdc+zstd':>12}")
    zb, cz = tot("zstd"), tot("cdc_zstd")
    res = {}
    for s in schemes:
        v = tot(s)
        res[s] = v
        print(f"  {s:<10} {v:>14,} {100 * v / base:>8.1f}% "
              f"{100 * v / zb:>8.1f}% {100 * v / cz:>11.1f}%")

    # chunking behaviour
    cnt = collections.Counter(r["n_chunks"] for r in miss)
    single = cnt[1]
    print(f"\nFastCDC behaviour on misses:")
    print(f"  artifacts producing exactly 1 chunk : {single}/{len(miss)} "
          f"({100 * single / len(miss):.1f}%)  <- CDC is a no-op for these")
    print(f"  chunk count distribution            : "
          f"{dict(sorted(cnt.items())[:6])}")

    # per kind
    print(f"\nby artifact kind (bytes, and best delta vs cdc+zstd):")
    print(f"  {'kind':<6} {'n':>5} {'raw':>12} {'zstd':>11} {'cdc+zstd':>11} "
          f"{'zdelta':>11} {'bsdiff':>11}")
    for kind in sorted(set(r["kind"] for r in miss)):
        g = [r for r in miss if r["kind"] == kind]
        def t(k):
            return sum(r[k] for r in g if r.get(k) is not None)
        print(f"  {kind:<6} {len(g):>5} {t('raw'):>12,} {t('zstd'):>11,} "
              f"{t('cdc_zstd'):>11,} {t('zdelta'):>11,} {t('bsdiff'):>11,}")

    best = min(res["zdelta"], res["bsdiff"])
    print(f"\nHEADLINE: best neighbor delta vs Bazel's cdc+zstd = "
          f"{100 * best / cz:.1f}%  "
          f"({'delta wins' if best < cz else 'CDC wins'}, "
          f"{abs(cz - best):,} B difference)")
    return res


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("corpus")
    ap.add_argument("--avg-kib", type=int, default=512,
                    help="FastCDC average chunk size (Bazel default 512)")
    ap.add_argument("--limit", type=int, default=None)
    ap.add_argument("--no-bsdiff", action="store_true")
    ap.add_argument("--out", default=None)
    a = ap.parse_args()

    rows = run(a.corpus, a.avg_kib, do_bsdiff=not a.no_bsdiff, limit=a.limit)
    label = f"{a.corpus}  (FastCDC avg={a.avg_kib}KiB)"
    summarize(rows, label)
    if a.out:
        json.dump(rows, open(a.out, "w"))
        print(f"\n[analyze] rows -> {a.out}")
