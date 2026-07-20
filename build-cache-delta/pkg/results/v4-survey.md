# V4 Novelty Verification: CDC Chunking (SplitBlob/SpliceBlob) vs. Neighbor-Selected Delta Encoding

Web survey, 2026-07-19. Negative results (queries that found nothing) are recorded deliberately — they support the novelty claim.

## Q1. Did the justbuild/Roloff team publish design docs, evaluations, papers, or talks on their CDC chunking — and did they ever compare against delta encoding?

### Found

**Design document — yes, one exists.** justbuild ships `doc/concepts/blob-splitting.md` (https://github.com/just-buildsystem/justbuild/blob/master/doc/concepts/blob-splitting.md). Contents: motivation ("For each small modification of the sources, the complete artifact needs to be downloaded even though maybe only a small fraction of the compiled binary artifact has been changed"), the SplitBlob contract (concatenation of chunks reproduces the blob), discussion of singleton vs. fixed-block vs. variable-block (CDC) chunking, recommendation of the Gear-hash/FastCDC algorithm (citing Xia et al., USENIX ATC 2016), and an acknowledged ~2x storage-cost tradeoff. **It contains no evaluation data and no mention of delta encoding, bsdiff, vcdiff, xdelta, zstd --patch-from, or rsync.**

**Evaluation data — informal only, buried in PR #282** (https://github.com/bazelbuild/remote-apis/pull/282, opened Nov 23 2023, merged Jul 9 2025 by tjgq). Roloff posted (Nov 28, 2023): ~800 MB filesystem images achieved "96–98% reuse factors for small changes"; a 300 MB executable with debug info achieved a "75% reuse factor." Separately, luluz66 (BuildBuddy) reported FastCDC experiments on ~5 TB of real Bazel data concluding 0.5 MB average chunk size is the sweet spot (Mar 7–8, 2024). Chunking algorithms compared in-thread: polynomial rolling hash, Rabin, Buzhash, FastCDC — comparison was **CDC-variant vs. CDC-variant only**. **No participant (incl. reviewers EdSchouten, sluongng, mostynb, tjgq, fmeum) ever raised delta/patch-based encoding as an alternative.** The only external design link is EdSchouten's REv3 "Elimination of large CAS objects" Google Doc.

**Proposal survey — no delta there either.** Issue #326 "Big blob support proposals summary" (sluongng, Jan 27 2025, https://github.com/bazelbuild/remote-apis/issues/326) lists exactly two approaches: Split/Splice RPCs and Remote Execution Manifest Blobs. No delta encoding, no quantitative data.

**Papers/talks — none on chunking.** The justbuild project page (https://www.linta.de/~aehlig/justbuild/) lists all project talks: Roloff's FOSDEM 2023 lightning talk "Staging of Artifacts in a Build System" (unrelated to chunking; https://archive.fosdem.org/2023/schedule/event/staging_of_artifacts_in_build_system/), Aehlig's FOSDEM 2025 lightning talk on build definitions, and two slide decks (2022, 2024) on justbuild generally. No arXiv paper, no BazelCon talk by Roloff/Aehlig on blob splitting was found.

### Not found (negative results, exact queries)
- "justbuild CDC chunking content-defined chunking build cache" — no justbuild-specific hits
- "Sascha Roloff BazelCon talk chunking large blobs justbuild" — no talk found
- "justbuild paper arXiv Aehlig Roloff build system Huawei" — no paper exists
- "FOSDEM justbuild talk 2023 2024 2025 build system Aehlig Roloff" — talks exist, none on chunking
- Delta-encoding mentions in PR #282, issue #326, blob-splitting.md — explicitly checked each document: zero occurrences

## Q2. Any prior work applying neighbor-selected delta encoding to build cache artifact transfer?

### Found — nothing in the build-cache domain; four adjacent-domain precedents to cite and scope against

1. **Dolstra 2005, "Efficient Upgrading in a Purely Functional Component Deployment Model"** (https://link.springer.com/chapter/10.1007/11424529_15) — bsdiff binary patches between Nix store paths, with automatic patch-sequence chaining between arbitrary versions. This is delta encoding of compiled build artifacts against a prior version — but in a *deployment/release-channel* setting (producer picks the base = previous release), not a general remote build cache with client-side neighbor selection. A 2024/2025 NixOS Discourse thread ("Nix store copy closure ... with binary diff", https://discourse.nixos.org/t/nix-store-copy-closure-or-import-export-with-binary-diff/58629) shows the delta idea remains *unimplemented* in modern Nix substitution.
2. **Post-deduplication delta compression with resemblance detection** (storage/backup literature): Shilane et al., "WAN-optimized replication of backup datasets using stream-informed delta compression" (FAST 2012 / ACM ToS 8(4)); Ddelta (Xia et al., 2014); Finesse/Odess; Argus (ACM ToS, https://dl.acm.org/doi/10.1145/3747839). This literature *is* neighbor-selected delta encoding (super-feature resemblance detection → delta against most-similar chunk) — for backup streams, not build caches. The novelty claim must be scoped to the build-cache/REAPI setting and to whole-artifact neighbor selection via build-graph locality rather than sketch-based resemblance.
3. **elfshaker/manyclangs** (https://github.com/elfshaker/elfshaker) — packs ~2,000 near-identical LLVM builds into ~100 MiB (~10,000x amortized) by exploiting cross-commit similarity of compiled artifacts. Storage format, not a cache-transfer protocol, but it is the strongest published evidence for the premise that neighbor artifacts are highly delta-compressible.
4. **In-ecosystem near-miss: remote-apis issue #272** "Support compression with external dictionary" (sluongng, Sep 22 2023, https://github.com/bazelbuild/remote-apis/issues/272, linked PR #276, still open). Proposes *trained* zstd/brotli dictionaries — not per-artifact `--patch-from` against a similar cached blob, no neighbor selection, no bsdiff/vcdiff mention, no evaluation. Closest existing REAPI idea; cite it to show the gap.

Also adjacent but distinct: Docker-image deltas (deltaimage tool, https://github.com/da-x/deltaimage; registry papers Slimmer/DupHunter are dedup-only) and the software-update lineage (bsdiff, Courgette/Zucchini, OSTree static deltas) — all producer-chosen-base update channels, not caches.

### Not found (negative results, exact queries)
- ""build cache" delta encoding bsdiff artifact transfer remote cache" — only generic bsdiff/delta material, nothing build-cache
- "arXiv "delta compression" "build artifacts" OR "continuous integration" transfer bandwidth" — only LLM-weight delta-compression papers
- ""remote cache" OR "build cache" "delta" compilation outputs paper evaluation arXiv bazel ccache" — no paper combining delta + build cache
- "sccache OR ccache OR "compiler cache" delta compression patch-based transfer zstd patch-from" — ccache/sccache have zstd storage compression only, no delta features
- "NativeLink OR EngFlow OR "remote execution" blog delta encoding patch cache artifacts" — no vendor delta/patch transport feature
- ""zstd --patch-from" CI cache build artifacts pipeline blog" — nothing
- "Gradle build cache OR "GitHub Actions cache" delta upload patch compression feature" — Gradle "layered cache" and zstd only, no delta
- "bazelbuild remote-apis issue delta compression bsdiff proposal github" — no delta/bsdiff proposal has ever been filed against remote-apis

## Q3. Published benchmarks for Bazel `--experimental_remote_cache_chunking`?

### Found
- **Bazel PR #28437** (author tyler-french/BuildBuddy, approved Mar 5 2026, merged ~Mar 17 2026; https://github.com/bazelbuild/bazel/pull/28437) contains the canonical benchmark: 50 commits of the BuildBuddy repo. Upload 85.6 GB → 52.0 GB (with disk cache), disk cache 246.5 GB → 146.6 GB, avg build time 100 s → 55 s; RPC count rises 273K → 626K. Headline: ~40% less upload, ~40% smaller disk cache.
- **BuildBuddy blog "Remote Cache CDC: Reusing Bytes"** (Tyler French, May 1 2026, https://www.buildbuddy.io/blog/content-defined-chunking/): FastCDC, ~512 KiB average chunk, chunking only for blobs >2 MiB (just 4.2% of objects); 85% dedup ratio on eligible writes; ~300 TiB duplicate uploads skipped in two weeks in production; overall savings 20–40%. Requires Bazel 8.7/9.1+. **No delta-encoding comparison anywhere in the post.**
- **buildbarn/go-cdc** (https://github.com/buildbarn/go-cdc, created in response to PR #282): FastCDC vs. MaxCDC vs. RepMaxCDC on 147 Linux kernel tarballs (~216 GB → ~6.7 GB unique chunks); MaxCDC only 1.74% better than FastCDC8KB. Again CDC-vs-CDC only.
- Caveat: the flag is still buggy/experimental — truncated outputs with `--disk_cache` (https://github.com/bazelbuild/bazel/issues/29544).

### Not found
- "bazel --experimental_remote_cache_chunking benchmark results" — no official Bazel-blog benchmark exists; the PR description and the BuildBuddy post are the only published numbers. No benchmark anywhere compares chunking against a delta/patch baseline.

## Verdict

The neighbor-selected delta encoding claim **survives, with mandatory scoping**. Every published artifact of the CDC line — justbuild's blob-splitting.md design doc, the entire 2023–2025 PR #282 review thread, issue #326's proposal survey, the Bazel PR #28437 benchmark, the BuildBuddy CDC blog, and buildbarn's go-cdc study — evaluates only CDC variants against each other and against whole-blob transfer; not one document even *mentions* bsdiff, vcdiff, xdelta, or zstd --patch-from, so there is no published CDC-vs-delta comparison for build caches to preempt this work, and no build-cache system (ccache, sccache, Bazel, BuildBuddy, EngFlow, NativeLink, Gradle, GitHub Actions) ships or has proposed a delta transport. However, the claim must be framed as novel *for remote build-cache artifact transfer with cache-driven neighbor selection*, not for delta encoding of build artifacts per se: Dolstra 2005 delta-encoded Nix store paths with bsdiff in a deployment channel, the post-dedup delta-compression literature (Shilane FAST'12 through Argus ToS'25) already does resemblance-detected neighbor delta in backup storage, remote-apis issue #272 floats dictionary-based compression as an unpursued in-ecosystem cousin, and elfshaker demonstrates the underlying similarity premise. A related-work section that positions against those four lines — and cites the exhaustive absence of delta in the REAPI/CDC record above — makes the novelty claim defensible.
