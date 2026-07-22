# Research notes: layered cache architecture (equivalence, dimensions, distance)

Design-direction record, July 2026. Distills an external research discussion
into FrostBuild terms so future cache work builds on theory instead of
intuition. Companion to [03_papers_and_references.md](03_papers_and_references.md)
and the v2 remote studies ([07](07_remote_cache_study.md), [11](11_remote_execution_study.md)).

## The three-layer model

A build cache can be organized as three layers with strictly separated roles:

```
Layer 3  sketch / embedding   distance          delta transfer, prefetch, scheduling
Layer 2  dimension hashes     partial equiv.    sound invalidation pruning
Layer 1  exact digest         equivalence       final reuse gate (correctness)
```

The central generalization: `lookup(x) ∈ {hit, miss}` becomes
`lookup(x) = argmin_{y ∈ cache} cost(y → x)` — an exact hit is `cost 0`, a
full build is `cost(∅ → x)`, and everything between is a new design space
(nearest-neighbor artifact + delta + exact-digest verification).

**Invariant that must never break:** cache *reuse* requires an equivalence
relation (`k(x) = k(y) ⇒ f(x) = f(y)`). Metric similarity can never gate
reuse — a one-character source change can flip build success — so vectors and
sketches live strictly in policy (Layer 3), never in the correctness gate.
Any restored/delta-reconstructed artifact must verify against its exact
digest before reuse.

## Where FrostBuild stands today

| Layer | Shipped | Notes |
|---|---|---|
| 1 — exact digest | Action keys (BLAKE3 over argv + toolchain closure + input digests), blob CAS, Bazel-compatible FastCDC 2020 chunk CAS for blobs over 2 MiB, journal, early cutoff, determinism-check mode | Every chunk is SHA-256 verified and the reconstructed blob is BLAKE3+mode verified before final-path publication. `UnverifiedBytes` cannot enter the publish boundary. |
| 2 — dimension hashes | Partial: depfile narrowing (used-headers only), order-only inputs (generated headers out of the key), platform/profile as explicit key axes | Missing: semantic dimensions *within* a file (API hash vs impl hash — the ijar/Rust-fingerprint analog for C/C++). |
| 3 — distance / policy | `--estimator {heuristic,journal,static,learned}`, `--predictive`, plus positional previous-artifact zstd residual deltas for local missing chunks | Remote transfer/cost negotiation, prefetch and learned eviction remain open. A selector miss changes cost only; two digest gates still decide reuse. |

Independent chunk digest/publication and verified positioned writes into the
private restore file now use the bounded Rayon pool. The final path remains
untouched until every chunk and the reconstructed BLAKE3+mode digest pass. A
64 MiB, median-of-7, alternating-order A/B measured 1.41x faster cold
publication, 1.89x faster exact-chunk restore and 1.88x faster delta restore;
the raw samples and high starting host load are recorded in
[`2026-07-21-E14-cas.json`](../bench/baselines/2026-07-21-E14-cas.json).
The result establishes a local parallelism win, not remote-cache economics.

## Adoptable directions (priority order)

1. **Dimension hashes for C/C++ (Layer 2).** Split a translation unit's
   identity into `{api, impl}` digests (preprocessed interface vs function
   bodies); dependents that only consume the API stop rebuilding on
   impl-only changes. This is Bazel ijar / Buck source-ABI generalized, and
   the soundness argument follows the SAC/Adapton lineage. Open research:
   automatic dimension discovery that minimizes expected invalidation
   (learnable from commit history).
2. **Positional residual-chunk delta transfer (Layer 3, v2 remote cache).**
   FastCDC exact reuse, the local chunk-addressable store and verified local
   zstd residual reconstruction are implemented.
   On a residual miss, use the byte-range-overlapping chunk from the previous
   version of the same graph artifact as a zstd dictionary, then verify the
   chunk and final blob digests. The measured corpus showed this positional
   selector beating the more complex sketch index; similarity never gates
   correctness. Remote CPU/bandwidth calibration remains the shipping gate.
3. **Algebraic root fingerprints (Layer 1 accelerator).** Homomorphic /
   lattice hashing (Bellare–Micciancio, LtHash) updates a workspace-root
   fingerprint in O(1) per file change — below even Merkle's O(log n) — a
   natural fit for the daemon's dirty tracking.
4. **Learned policy (Layer 3).** Embeddings predict durations, hit
   probability, next-needed artifacts; wrong predictions cost time only.
   Meta's Predictive Test Selection (ICSE-SEIP 2019) is the production
   precedent; our `--predictive`/`--estimator` flags are the safe scaffolding.

## Explicit non-adoptions

- **Vector similarity as a hit test** — unsound (silent wrong binaries);
  rejected permanently, not deferred.
- **Quantum algorithms** — Grover cannot beat classical I/O (QRAM encoding
  costs exceed SSD reads; the workload is I/O- and metadata-bound), and
  QUBO-annealed scheduling loses to classical solvers at build-graph scale
  (cf. Trummer & Koch, VLDB 2016, for the honest DB analog). Related-work
  material only.
- **Category-theoretic re-formalization for its own sake** (comonadic
  pruning, SDE/Langevin scheduling dressings) — formalization without a new
  theorem or algorithm; Build Systems à la Carte already provides the
  working abstraction.

## Primary literature

- Mokhov, Mitchell, Peyton Jones — *Build Systems à la Carte* (ICFP 2018 /
  JFP 2020): the coordinate system (scheduler × rebuilder; early cutoff).
- Acar — *Self-Adjusting Computation* (2005); Hammer et al. — *Adapton*
  (PLDI 2014); Cai et al. — *Incremental Lambda Calculus* (PLDI 2014):
  fine-grained incremental theory and soundness-proof templates.
- Mitchell — *Shake* (ICFP 2012): monadic/dynamic dependencies.
- Dolstra — *Nix* thesis (2006): purely functional deployment; hermeticity
  as the precondition for Layer 1.
- Budiu et al. — *DBSP* (VLDB 2023): algebraic unification of incremental
  computation (bridge between DB and build views).
- Machalica et al. — *Predictive Test Selection* (ICSE-SEIP 2019): learned
  policy with a verification boundary.
- Indyk & Motwani (LSH, 1998); Charikar (SimHash, 2002); Broder (MinHash);
  Bellare–Micciancio (incremental hashing); LtHash: the Layer 3 toolbox.

One-line summary: **equivalence guards correctness, dimensions shrink
invalidation, distance earns performance — and the three must never trade
places.**
