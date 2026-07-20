#!/usr/bin/env bash
# Full DeltaCDC measurement sweep. Sequential on purpose: the CPU numbers are
# part of the result, so runs must not contend with each other.
set -uo pipefail
cd "$(dirname "$0")/.."
PY=.venv/bin/python
mkdir -p results/deltacdc

run() {  # corpus avg_kib oracle
  local c=$1 k=$2 o=$3
  echo "=== $c avg=${k}KiB oracle=$o"
  $PY harness/deltacdc.py "corpus/$c" --avg-kib "$k" --oracle-sample "$o" \
      --engines bsdiff,zstd --out "results/deltacdc/${c}_avg${k}.json" \
      2>&1 | tail -4
}

# Large multi-chunk artifacts: the regime where CDC actually works and where
# the residual-delta question is open (memo P0).
run rg-debug 512 0.02      # Bazel default parameters
run rg-debug 64  0.01      # CDC-favourable sweep

# Small artifacts: the regime where CDC is a no-op (already known, re-measured
# here with the same harness so every number in the paper is comparable).
run lua-O2 512 0.05
run lua-g  512 0.05
run lua-O2 16  0.02
run lua-g  16  0.02

touch results/deltacdc/.done
echo "ALL DONE"
