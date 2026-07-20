#!/usr/bin/env python3
"""DeltaCDC: chunk-level residual delta after content-defined chunking.

This is the measurement the proposal actually needs, and the one nobody has
published: after REAPI/Bazel CDC has reused every exactly-matching chunk, how
much of the *remaining* transfer can a binary delta against a similar cached
chunk remove — and at what CPU cost?

    old blob: [A][B ][C][D]
    new blob: [A][B'][C][D]
    CDC reuse:      A, C, D          <- Bazel today
    residual:       B' in full       <- DeltaCDC deltas this against a base

Schemes measured per artifact (all end in an exact digest check, so they are
equally sound; the only axis of comparison is bytes and CPU):

    raw           whole blob, uncompressed                      (B0)
    zstd          whole blob, zstd                              (B1)
    cdc           FastCDC 2020 split, fetch missing chunks      (B2, Bazel today)
    cdc+zstd      same, each missing chunk zstd-compressed      (B2 realistic)
    deltacdc/pos  residual chunks delta'd against the positionally
                  overlapping chunk of the previous version     (B4 at chunk level)
    deltacdc/sk   residual chunks delta'd against a super-feature
                  sketch match over the whole local chunk CAS   (B5, proposed)
    deltacdc/or   residual chunks delta'd against the best base in
                  the previous version (sampled upper bound)    (B6, oracle)
    blobdelta     whole-blob delta against previous version     (v1 measurement,
                  kept for continuity; unbounded CPU)

Base selection never affects correctness: every reconstructed chunk is verified
against its own digest, the concatenated blob is verified against the expected
blob digest, and any scheme whose delta is not smaller than the full chunk
falls back to the full chunk. A selector mistake costs time, never correctness
(memo 4.3, and the failure mode of Bazel issue #29544 is exactly what the
double verification here is designed to catch).

Delta engines:
    bsdiff      bsdiff4 (suffix sort + bzip2 framing)
    zstd-patch  zstd with the base as a raw-content dictionary, level 19 and a
                window large enough to address the whole base. NOTE: measuring
                this at the cache-compression level (3) understates it by an
                order of magnitude and is what made an earlier run conclude
                that zstd "cannot follow address shifts". At level 19 with long
                mode it lands within a few percent of bsdiff.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import random
import resource
import statistics
import sys
import time
from dataclasses import dataclass, field

import bsdiff4
import zstandard as zstd

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from fastcdc import FastCdcChunker, GEAR  # noqa: E402  (Bazel bit-compatible)

M64 = (1 << 64) - 1
ZLEVEL_CACHE = 3    # Bazel --remote_cache_compression
ZLEVEL_DELTA = 19   # delta encoding is a high-effort operation, like bsdiff
SKETCH_FEATURES = 12
SKETCH_SUPER = 3    # super-features = groups of SKETCH_FEATURES/SKETCH_SUPER


def sha(b: bytes) -> str:
    return hashlib.sha256(b).hexdigest()


def cpu_now() -> float:
    """User+sys CPU of this process and its children."""
    a = resource.getrusage(resource.RUSAGE_SELF)
    b = resource.getrusage(resource.RUSAGE_CHILDREN)
    return a.ru_utime + a.ru_stime + b.ru_utime + b.ru_stime


# --------------------------------------------------------------------------
# delta engines
# --------------------------------------------------------------------------

def zstd_size(data: bytes, level: int = ZLEVEL_CACHE) -> int:
    return len(zstd.ZstdCompressor(level=level).compress(data))


def _window_log(n: int) -> int:
    return min(30, max(20, (max(n, 1) - 1).bit_length() + 1))


def zstd_delta(base: bytes, target: bytes) -> bytes:
    """zstd delta with `base` as a raw-content dictionary (== --patch-from)."""
    d = zstd.ZstdCompressionDict(base, dict_type=zstd.DICT_TYPE_RAWCONTENT)
    params = zstd.ZstdCompressionParameters.from_level(
        ZLEVEL_DELTA, window_log=_window_log(max(len(base), len(target))),
        enable_ldm=1)
    return zstd.ZstdCompressor(dict_data=d,
                               compression_params=params).compress(target)


def zstd_undelta(base: bytes, patch: bytes) -> bytes:
    d = zstd.ZstdCompressionDict(base, dict_type=zstd.DICT_TYPE_RAWCONTENT)
    return zstd.ZstdDecompressor(dict_data=d,
                                 max_window_size=2 ** 30).decompress(patch)


ENGINES = {
    "bsdiff": (bsdiff4.diff, bsdiff4.patch),
    "zstd": (zstd_delta, zstd_undelta),
}


# --------------------------------------------------------------------------
# resemblance sketch: content-defined sub-chunk features (Odess/Finesse style)
# --------------------------------------------------------------------------
#
# A chunk's features are the k smallest digests among content-defined
# SUB-chunks contained in it. Sub-chunk boundaries are content-defined, so an
# insertion or an address shift re-synchronizes: the surviving sub-chunks keep
# their digests, and the min-k selection is therefore shift-invariant. This is
# the cheap resemblance primitive from the post-deduplication delta-compression
# literature (Shilane FAST'12 super-features; Odess/Finesse make it one pass).
#
# It also costs one extra chunking pass rather than k multiplications per byte,
# which is what a real client would pay. Note for the paper: a REAPI client
# already receives 512 KiB chunk digests from SplitBlob, so sub-chunk features
# are an *additional* index the protocol does not currently carry.

SUB_AVG = 8 * 1024
FEATURES_PER_CHUNK = 8


def blob_features(data: bytes, sub: FastCdcChunker) -> list[tuple[int, int]]:
    """(offset, 64-bit digest) for every content-defined sub-chunk."""
    out = []
    for off, ln in sub.chunk(data):
        h = int.from_bytes(
            hashlib.blake2b(data[off:off + ln], digest_size=8).digest(),
            "little")
        out.append((off, h))
    return out


def chunk_features(feats: list[tuple[int, int]], lo: int, hi: int
                   ) -> tuple[int, ...]:
    """The k smallest sub-chunk digests whose start falls inside [lo, hi)."""
    inside = [h for off, h in feats if lo <= off < hi]
    if not inside:
        return ()
    inside.sort()
    return tuple(inside[:FEATURES_PER_CHUNK])


# --------------------------------------------------------------------------
# corpus
# --------------------------------------------------------------------------

def load_commits(root: str) -> list[tuple[str, list[str]]]:
    """[(commit_dir, [artifact paths])] oldest first. Bytes are read lazily."""
    out = []
    for d in sorted(os.listdir(root)):
        p = os.path.join(root, d)
        if not os.path.isdir(p):
            continue
        files = [os.path.join(p, f) for f in sorted(os.listdir(p))
                 if not f.startswith(".")]
        if files:
            out.append((d, files))
    return out


class BlobStore:
    """Chunk bytes live on disk, exactly as they do in a real CAS.

    Keeping every cached chunk's bytes resident is what a naive harness does
    and it is both unfaithful and fatal: a 25-commit corpus of 50 MB binaries
    is >1 GB of chunk payload. Chunks are addressed by (path, offset, length)
    and read through a small mmap cache.
    """

    def __init__(self, limit: int = 64):
        self._maps: dict[str, memoryview] = {}
        self._order: list[str] = []
        self.limit = limit

    def _map(self, path: str) -> memoryview:
        mv = self._maps.get(path)
        if mv is None:
            import mmap
            with open(path, "rb") as f:
                mv = memoryview(mmap.mmap(f.fileno(), 0,
                                          access=mmap.ACCESS_READ))
            self._maps[path] = mv
            self._order.append(path)
            if len(self._order) > self.limit:
                old = self._order.pop(0)
                self._maps.pop(old, None)
        return mv

    def read(self, path: str, offset: int, length: int) -> bytes:
        return bytes(self._map(path)[offset:offset + length])


STORE = BlobStore()


@dataclass
class ChunkRef:
    """A chunk held in the local CAS, addressable for delta base selection."""
    digest: str
    blob: str          # artifact name it came from
    path: str          # file the bytes live in
    index: int         # chunk index within that blob
    offset: int
    length: int
    feats: tuple = ()

    @property
    def data(self) -> bytes:
        return STORE.read(self.path, self.offset, self.length)


@dataclass
class Totals:
    raw: int = 0
    zstd: int = 0
    cdc: int = 0
    cdc_zstd: int = 0
    blobdelta: dict = field(default_factory=lambda: {"bsdiff": 0, "zstd": 0})
    deltacdc: dict = field(default_factory=dict)   # (selector, engine) -> bytes
    cpu: dict = field(default_factory=dict)
    residual_chunks: int = 0
    residual_bytes: int = 0
    sel_found: dict = field(default_factory=dict)
    sel_none: dict = field(default_factory=dict)
    exact_chunk_hits: int = 0
    exact_blob_hits: int = 0
    misses: int = 0
    one_chunk_misses: int = 0
    verify_failures: int = 0
    fallbacks: dict = field(default_factory=dict)


def add_cpu(tot: Totals, key: str, dt: float) -> None:
    tot.cpu[key] = tot.cpu.get(key, 0.0) + dt


def best_delta(base: bytes, target: bytes, engine: str) -> tuple[int, float]:
    """Delta size and CPU seconds. Verifies the round trip."""
    diff, patch = ENGINES[engine]
    t = cpu_now()
    d = diff(base, target)
    dt = cpu_now() - t
    if patch(base, d) != target:
        raise AssertionError(f"{engine} round trip failed")
    return len(d), dt


def run(root: str, avg_kib: int, engines: list[str], oracle_sample: int,
        limit: int | None, seed: int) -> dict:
    rng = random.Random(seed)
    chunker = FastCdcChunker(avg_size=avg_kib * 1024)
    sub = FastCdcChunker(avg_size=SUB_AVG)
    commits = load_commits(root)
    if limit:
        commits = commits[:limit]

    cas_blobs: set[str] = set()
    cas_chunks: dict[str, ChunkRef] = {}
    sketch_index: dict[int, list[str]] = {}          # super-feature -> digests
    prev_blob: dict[str, bytes] = {}                 # artifact -> bytes
    prev_chunks: dict[str, list[ChunkRef]] = {}      # artifact -> chunk list

    tot = Totals()
    # pos = byte-range overlap in the previous version of the same artifact
    #       (build-graph locality; needs no index)
    # sk  = super-feature match over the whole local chunk CAS
    # hy  = sk when it finds a candidate, else pos (what one would ship)
    # or  = best base in the previous version (sampled upper bound)
    selectors = ["pos", "sk", "hy"] + (["or"] if oracle_sample else [])
    tot.sel_found = {s: 0 for s in selectors}
    tot.sel_none = {s: 0 for s in selectors}
    for s in selectors:
        for e in engines:
            tot.deltacdc[f"{s}/{e}"] = 0
            tot.fallbacks[f"{s}/{e}"] = 0
    per_chunk_rows = []
    t_wall = time.time()

    print(f"[deltacdc] {os.path.basename(root)}: {len(commits)} commits, "
          f"FastCDC avg={avg_kib}KiB min={chunker.min // 1024}KiB "
          f"max={chunker.max // 1024}KiB, engines={','.join(engines)}",
          flush=True)

    for ci, (cdir, paths) in enumerate(commits):
        for path in paths:
            name = os.path.basename(path)
            with open(path, "rb") as f:
                data = f.read()
            blob_digest = sha(data)

            t = cpu_now()
            spans = list(chunker.chunk(data))
            add_cpu(tot, "chunking", cpu_now() - t)
            t = cpu_now()
            feats = blob_features(data, sub)
            add_cpu(tot, "sketch", cpu_now() - t)
            chunk_list = []
            for i, (off, ln) in enumerate(spans):
                chunk_list.append(ChunkRef(
                    sha(data[off:off + ln]), name, path, i, off, ln,
                    chunk_features(feats, off, off + ln)))

            if blob_digest in cas_blobs:
                tot.exact_blob_hits += 1
            else:
                tot.misses += 1
                if len(chunk_list) == 1:
                    tot.one_chunk_misses += 1
                tot.raw += len(data)
                t = cpu_now()
                tot.zstd += zstd_size(data)
                add_cpu(tot, "zstd_whole", cpu_now() - t)

                residual = [c for c in chunk_list if c.digest not in cas_chunks]
                tot.exact_chunk_hits += len(chunk_list) - len(residual)
                tot.residual_chunks += len(residual)
                tot.residual_bytes += sum(c.length for c in residual)
                tot.cdc += sum(c.length for c in residual)
                t = cpu_now()
                tot.cdc_zstd += sum(
                    zstd_size(data[c.offset:c.offset + c.length])
                    for c in residual)
                add_cpu(tot, "zstd_chunks", cpu_now() - t)

                base_blob = (STORE.read(prev_blob[name][0], 0,
                                        prev_blob[name][1])
                             if name in prev_blob else None)
                base_chunks = prev_chunks.get(name, [])

                # whole-blob delta (v1 measurement, kept for continuity)
                if base_blob is not None:
                    for e in engines:
                        size, dt = best_delta(base_blob, data, e)
                        tot.blobdelta[e] += min(size, len(data))
                        add_cpu(tot, f"blobdelta_{e}", dt)
                else:
                    for e in engines:
                        tot.blobdelta[e] += len(data)

                # ---- DeltaCDC: delta only the residual chunks ----
                for c in residual:
                    cdata = data[c.offset:c.offset + c.length]
                    cands: dict[str, ChunkRef | None] = {}

                    # positional: the base chunk whose byte range overlaps most
                    cands["pos"] = _overlap_base(base_chunks, c)

                    # sketch: feature match over the whole local chunk CAS
                    t = cpu_now()
                    cands["sk"] = _sketch_base(c.feats, sketch_index,
                                               cas_chunks, c.length)
                    add_cpu(tot, "sketch_lookup", cpu_now() - t)
                    cands["hy"] = cands["sk"] or cands["pos"]
                    for s in ("pos", "sk", "hy"):
                        if cands.get(s) is None:
                            tot.sel_none[s] += 1
                        else:
                            tot.sel_found[s] += 1

                    # oracle: best base in the previous version (sampled)
                    do_oracle = ("or" in selectors and base_chunks
                                 and rng.random() < oracle_sample)
                    row = {"commit": ci, "name": name, "len": c.length}
                    for e in engines:
                        full = zstd_size(cdata)
                        for s in ("pos", "sk", "hy"):
                            b = cands.get(s)
                            if b is None:
                                tot.deltacdc[f"{s}/{e}"] += full
                                tot.fallbacks[f"{s}/{e}"] += 1
                                continue
                            size, dt = best_delta(b.data, cdata, e)
                            add_cpu(tot, f"delta_{e}", dt)
                            if size < full:
                                tot.deltacdc[f"{s}/{e}"] += size
                            else:
                                tot.deltacdc[f"{s}/{e}"] += full
                                tot.fallbacks[f"{s}/{e}"] += 1
                            row[f"{s}/{e}"] = size
                        if do_oracle:
                            best = None
                            for b in base_chunks:
                                size, dt = best_delta(b.data, cdata, e)
                                add_cpu(tot, f"delta_{e}_oracle", dt)
                                if best is None or size < best:
                                    best = size
                            tot.deltacdc[f"or/{e}"] += min(best, full)
                            row[f"or/{e}"] = best
                    if do_oracle:
                        per_chunk_rows.append(row)

                    # blob-level verification of the reconstruction path:
                    # rebuild the chunk from its chosen base and check its
                    # digest, exactly as SpliceBlob would.
                    b = cands.get("pos") or cands.get("sk")
                    if b is not None:
                        e = engines[0]
                        diff, patch = ENGINES[e]
                        if sha(patch(b.data, diff(b.data, cdata))) != c.digest:
                            tot.verify_failures += 1

            # commit the artifact into the local CAS
            cas_blobs.add(blob_digest)
            for c in chunk_list:
                if c.digest not in cas_chunks:
                    cas_chunks[c.digest] = c
                    for f in c.feats:
                        sketch_index.setdefault(f, []).append(c.digest)
            prev_blob[name] = (path, len(data))
            prev_chunks[name] = chunk_list

        if (ci + 1) % 5 == 0 or ci == len(commits) - 1:
            print(f"  [{ci + 1}/{len(commits)}] misses={tot.misses} "
                  f"residual_chunks={tot.residual_chunks} "
                  f"cdc+zstd={tot.cdc_zstd / 1e6:.1f}MB "
                  f"deltacdc/sk={tot.deltacdc.get('sk/' + engines[0], 0) / 1e6:.2f}MB",
                  flush=True)

    return _report(root, avg_kib, engines, tot, per_chunk_rows,
                   time.time() - t_wall)


def _overlap_base(base_chunks: list[ChunkRef], c: ChunkRef) -> ChunkRef | None:
    """Base chunk whose byte range overlaps the target's the most.

    Chunk boundaries shift when content is inserted, so index-for-index is
    wrong; maximal byte-range overlap is the cheap selector a real client can
    compute from metadata it already has.
    """
    best, best_ov = None, 0
    lo, hi = c.offset, c.offset + c.length
    for b in base_chunks:
        ov = min(hi, b.offset + b.length) - max(lo, b.offset)
        if ov > best_ov:
            best, best_ov = b, ov
    return best


def _sketch_base(sk, index, cas_chunks, target_len) -> ChunkRef | None:
    """Highest-agreement super-feature match, size-filtered."""
    if not sk:
        return None
    votes: dict[str, int] = {}
    for f in sk:
        for d in index.get(f, ()):
            votes[d] = votes.get(d, 0) + 1
    best, best_score = None, 0
    for d, v in votes.items():
        ref = cas_chunks.get(d)
        if ref is None:
            continue
        # a base far off in size is a poor delta base
        ratio = min(ref.length, target_len) / max(ref.length, target_len)
        score = v * ratio
        if score > best_score:
            best, best_score = ref, score
    return best


def _report(root, avg_kib, engines, tot: Totals, rows, wall) -> dict:
    def mb(x):
        return round(x / 1e6, 3)

    rep = {
        "corpus": os.path.basename(root),
        "avg_kib": avg_kib,
        "misses": tot.misses,
        "exact_blob_hits": tot.exact_blob_hits,
        "one_chunk_miss_rate": round(tot.one_chunk_misses / tot.misses, 3)
        if tot.misses else None,
        "exact_chunk_hits": tot.exact_chunk_hits,
        "residual_chunks": tot.residual_chunks,
        "bytes_mb": {
            "raw": mb(tot.raw),
            "zstd": mb(tot.zstd),
            "cdc": mb(tot.cdc),
            "cdc+zstd": mb(tot.cdc_zstd),
            **{f"blobdelta/{e}": mb(v) for e, v in tot.blobdelta.items()
               if e in engines},
            **{f"deltacdc/{k}": mb(v) for k, v in tot.deltacdc.items()},
        },
        # The oracle runs on a random sample of residual chunks, so its
        # totals cover a different, much smaller set than every other scheme.
        # Reporting them in the same table would invite a comparison that the
        # numbers do not support; the oracle's value is the per-chunk gap
        # reported under oracle_gap_*, not a corpus total.
        "oracle_totals_are_sampled_only": True,
        "vs_cdc_zstd_pct": {
            k: round(100 * v / tot.cdc_zstd, 1) for k, v in
            [(f"deltacdc/{k2}", v2) for k2, v2 in tot.deltacdc.items()
             if not k2.startswith("or/")]
            + [(f"blobdelta/{e}", tot.blobdelta[e]) for e in engines]
            if tot.cdc_zstd
        },
        "cpu_s": {k: round(v, 2) for k, v in sorted(tot.cpu.items())},
        "fallbacks": tot.fallbacks,
        "selector_base_found": tot.sel_found,
        "selector_no_base": tot.sel_none,
        "verify_failures": tot.verify_failures,
        "wall_s": round(wall, 1),
    }
    if rows:
        rep["oracle_sample"] = len(rows)
        for e in engines:
            key, okey = f"pos/{e}", f"or/{e}"
            pairs = [(r[key], r[okey]) for r in rows
                     if key in r and okey in r and r[okey]]
            if pairs:
                ratios = [o / p for p, o in pairs if p]
                rep[f"oracle_gap_{e}"] = {
                    "n": len(pairs),
                    "pos_mb": mb(sum(p for p, _ in pairs)),
                    "oracle_mb": mb(sum(o for _, o in pairs)),
                    "oracle_better_pct": round(
                        100 * (1 - sum(o for _, o in pairs)
                               / max(1, sum(p for p, _ in pairs))), 1),
                    "median_oracle_over_pos": round(
                        statistics.median(ratios), 3) if ratios else None,
                }
    return rep


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("corpus")
    ap.add_argument("--avg-kib", type=int, default=512)
    ap.add_argument("--engines", default="bsdiff,zstd")
    ap.add_argument("--oracle-sample", type=float, default=0.05,
                    help="probability a residual chunk gets the full oracle "
                         "search (0 disables)")
    ap.add_argument("--limit", type=int)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--out")
    a = ap.parse_args()
    rep = run(a.corpus, a.avg_kib, a.engines.split(","), a.oracle_sample,
              a.limit, a.seed)
    print(json.dumps(rep, indent=2))
    if a.out:
        with open(a.out, "w") as f:
            json.dump(rep, f, indent=1)
        print(f"wrote {a.out}")


if __name__ == "__main__":
    main()
