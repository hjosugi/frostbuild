# DeltaCDC on large multi-chunk artifacts

Measured 2026-07-20 on E14. Harness: `harness/deltacdc.py`, chunker
bit-compatible with Bazel's `FastCdcChunker.java`. Raw: `deltacdc/rg-debug_avg512.json`.

Corpus: **ripgrep, 25 consecutive commits, debug binaries ~50 MB each** — the
regime CDC is built for and where the Lua measurement said nothing. Every blob
is multi-chunk (1-chunk miss rate 0.0), against Lua where Bazel's default
parameters left 70-78% of misses in a single chunk.

24 cache misses, 1403 residual chunks after exact-chunk reuse. Bazel default
parameters (avg 512 KiB / min 128 KiB / max 2 MiB).

## Transfer

| scheme | MB | vs CDC |
|---|---|---|
| raw | 1199.2 | 610% |
| whole-blob zstd | 281.8 | 143% |
| CDC, chunks uncompressed | 758.1 | — |
| **CDC + zstd (Bazel today)** | **196.5** | **100%** |
| DeltaCDC, sketch-selected base | 155.0–157.0 | 79% |
| **DeltaCDC, positional base** | **93.2–99.9** | **47–51%** |
| DeltaCDC, sketch-or-positional | 89.2–95.0 | 45–48% |
| whole-blob delta vs previous version | 77.4 (bsdiff) / 164.2 (zstd) | 39% / 84% |

All 1403 reconstructions were applied and digest-verified. Zero mismatches.

## What this changes

**1. DeltaCDC roughly halves what Bazel's CDC transfers** on large artifacts —
47–51% with a selector that needs no index. The go/no-go in the memo was
"10% below CDC-only or stop"; this clears it by a wide margin, in the regime
that matters most.

**2. The sketch index is not worth building.** Super-feature matching over the
whole chunk CAS found a base for 549 of 1403 residual chunks, where the
positional selector — the chunk of the previous version of the same artifact
whose byte range overlaps most — found one for 1305, and beat the sketch on
transfer whenever both had a candidate (79% vs 51% of CDC). The hybrid
(sketch, else positional) lands within 3 points of positional alone.
**Build-graph locality beats content resemblance here**, and it costs a
metadata lookup instead of an index. That simplifies the protocol: the base is
named by (artifact, previous version), which a client already knows.

**3. Whole-blob delta transfers less than chunk-level delta on this corpus**
(39% vs 47–51%), which contradicts an earlier reading of a 3-commit slice
where it appeared 2x worse. That slice was dominated by the first commit,
which has no base and pays full size; over 25 commits that one-time cost
amortizes. The case for chunk-level is therefore **not** transfer size — it is
bounded cost: bsdiff over a 50 MB blob suffix-sorts 50 MB, while a chunk is
capped at the 2 MiB max chunk and parallelizes. Measured CPU was 700 s
whole-blob against 542 s chunk-level for the same corpus, and the gap widens
with blob size. Any claim that chunk-level moves fewer bytes should be
withdrawn.

**4. zstd `--patch-from` matches or beats bsdiff at chunk level** (47.5% vs
50.8% positional). Combined with the Lua finding that the two land within 5-7%
of each other, the memo's "delta format choice is the essential part" reads as
an artifact of measuring zstd at cache-compression level 3 rather than at a
delta-appropriate level. The practical consequence is good: REAPI already
carries zstd, so the transport needs no new compressor.

## Not established by this run

- **The oracle numbers in the JSON are sampled, not corpus totals.** The full
  oracle search ran on ~2% of residual chunks, so its totals cover a different
  set and must not be read next to the others; the field is flagged in the
  output. The per-chunk gap is the usable signal, and this run collected too
  few samples to state one.
- One project, one language, one artifact kind, one machine.
- No CPU/bandwidth tradeoff model: 542 s of delta CPU to save 100 MB is a win
  on a slow link and a loss on a fast one. The cost model in memo §4.2 exists
  precisely for this and has not been calibrated.
- The 20-40% figure BuildBuddy reports for production CDC is not comparable to
  the 100% baseline here, which is CDC measured on this corpus.
