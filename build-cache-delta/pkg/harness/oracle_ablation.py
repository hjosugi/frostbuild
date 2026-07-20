#!/usr/bin/env python3
"""Neighbor-selection ablation on the lua corpus.

Memo §6 ablation row (iii): how far is the trivial selector (same path,
previous commit) from the same-path ORACLE (best bsdiff delta against any
earlier version of that path)? If trivial ≈ oracle, sketch-based ANN
selection is unnecessary for this corpus and the paper can say so.

Runs over the artifact corpus produced by run_experiment.py.
"""

import json
import sys
from pathlib import Path

import bsdiff4

from run_experiment import ART, RESULTS, sha256, cpu_now

CONFIGS = ["lua-O2", "lua-g"]


def versions(config: str) -> list[Path]:
    outdir = ART / config
    return sorted(d for d in outdir.iterdir() if (d / ".done").exists())


def ablate(config: str) -> dict:
    dirs = versions(config)
    # history[path] = list of (commit_index, digest, Path) in commit order
    history: dict[str, list[tuple[int, str, Path]]] = {}
    misses = []
    t0 = cpu_now()
    for idx, dest in enumerate(dirs):
        for path in sorted(dest.iterdir()):
            if path.name == ".done":
                continue
            data = path.read_bytes()
            digest = sha256(data)
            prior = history.setdefault(path.name, [])
            if prior and prior[-1][1] != digest:
                # cache miss with neighbors available: measure trivial vs oracle
                new = data
                trivial_size = None
                best = None
                seen_digests = set()
                for j, (jidx, jdigest, jpath) in enumerate(reversed(prior)):
                    if jdigest in seen_digests:
                        continue
                    seen_digests.add(jdigest)
                    size = len(bsdiff4.diff(jpath.read_bytes(), new))
                    if j == 0:
                        trivial_size = size
                    if best is None or size < best[0]:
                        best = (size, jidx)
                misses.append({
                    "commit_index": idx,
                    "path": path.name,
                    "raw": len(new),
                    "trivial": trivial_size,
                    "oracle": best[0],
                    "oracle_commit_index": best[1],
                    "oracle_is_trivial": best[0] == trivial_size,
                })
            prior.append((idx, digest, path))
    cpu = cpu_now() - t0
    n = len(misses)
    tot_trivial = sum(m["trivial"] for m in misses)
    tot_oracle = sum(m["oracle"] for m in misses)
    summary = {
        "config": config,
        "misses": n,
        "trivial_mb": round(tot_trivial / 1e6, 3),
        "oracle_mb": round(tot_oracle / 1e6, 3),
        "oracle_saving_pct": round(100 * (1 - tot_oracle / tot_trivial), 1)
        if tot_trivial else None,
        "oracle_is_trivial_rate": round(
            sum(1 for m in misses if m["oracle_is_trivial"]) / n, 3)
        if n else None,
        "cpu_s": round(cpu, 1),
    }
    return {"summary": summary, "misses": misses}


def main() -> None:
    out = {}
    for config in CONFIGS:
        print(f"== oracle ablation: {config}", flush=True)
        out[config] = ablate(config)
        print(json.dumps(out[config]["summary"], indent=2), flush=True)
    (RESULTS / "oracle.json").write_text(json.dumps(out, indent=1))
    print("wrote results/oracle.json")


if __name__ == "__main__":
    sys.exit(main())
