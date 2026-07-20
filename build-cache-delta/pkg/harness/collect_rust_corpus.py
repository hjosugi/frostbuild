#!/usr/bin/env python3
"""Large-artifact corpus: ripgrep debug builds (Rust toolchain).

Addresses the top external-validity threat in results/SUMMARY.md: the lua
artifacts are <=3 MB, far below CDC's intended blob sizes. A ripgrep debug
binary is tens of MB with debug info spread across the whole image — the
regime where FastCDC chunk reuse should work best. Same measurement core
and CAS model as run_experiment.py; artifact = target/debug/rg only.

Dependency crates stay cached in target/ across commits (only workspace
crates rebuild), mirroring a warm CI builder.
"""

import json
import os
import shutil
import subprocess
import sys

from run_experiment import (
    ART, CORPUS, RESULTS, analyze, summarize,
)

RG = CORPUS / "ripgrep"
# Pin the target dir: the host sets a global CARGO_TARGET_DIR, which would
# silently divert the binary away from the corpus checkout.
TARGET = RG / "target"
N_COMMITS = 25
CONFIG = "rg-debug"


def commit_list() -> list[str]:
    out = subprocess.run(
        ["git", "-C", str(RG), "log", "--first-parent", f"-{N_COMMITS}",
         "--format=%H"],
        capture_output=True, text=True, check=True,
    ).stdout.split()
    out.reverse()
    return out


def build_corpus(shas: list[str]):
    outdir = ART / CONFIG
    outdir.mkdir(parents=True, exist_ok=True)
    meta = []
    for i, sha in enumerate(shas):
        dest = outdir / f"{i:03d}-{sha[:12]}"
        if (dest / ".done").exists():
            meta.append((sha, dest))
            continue
        subprocess.run(["git", "-C", str(RG), "checkout", "-q", sha],
                       check=True)
        proc = subprocess.run(
            ["cargo", "build", "--quiet"],
            cwd=RG, capture_output=True,
            env={**os.environ, "CARGO_TARGET_DIR": str(TARGET)},
        )
        if proc.returncode != 0:
            print(f"  [{CONFIG}] build failed at {sha[:12]}; commit skipped",
                  flush=True)
            continue
        rg = TARGET / "debug/rg"
        if not rg.exists():
            print(f"  [{CONFIG}] no rg binary at {sha[:12]}; skipped",
                  flush=True)
            continue
        if dest.exists():
            shutil.rmtree(dest)
        dest.mkdir(parents=True)
        shutil.copy2(rg, dest / "rg")
        (dest / ".done").write_text(sha)
        meta.append((sha, dest))
        size_mb = (dest / "rg").stat().st_size / 1e6
        print(f"  [{CONFIG}] built {i + 1}/{len(shas)} {sha[:12]} "
              f"(rg {size_mb:.1f} MB)", flush=True)
    return meta


def main() -> None:
    RESULTS.mkdir(exist_ok=True)
    shas = commit_list()
    print(f"corpus: ripgrep, {len(shas)} first-parent commits "
          f"{shas[0][:12]}..{shas[-1][:12]}", flush=True)
    meta = build_corpus(shas)
    report = analyze(CONFIG, meta)
    (RESULTS / f"{CONFIG}.json").write_text(json.dumps(report, indent=1))
    summary = summarize(report)
    print(json.dumps(summary, indent=2))
    # merge into summary.json
    summaries = []
    combined = RESULTS / "summary.json"
    if combined.exists():
        summaries = [s for s in json.loads(combined.read_text())
                     if s["config"] != CONFIG]
    summaries.append(summary)
    combined.write_text(json.dumps(summaries, indent=1))


if __name__ == "__main__":
    sys.exit(main())
