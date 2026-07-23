#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

jobs="${FROST_BENCH_JOBS:-$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)}"
tools="${FROST_BENCH_TOOLS:-ninja,make}"
sizes="${FROST_BENCH_SIZES:-1000,10000}"
iterations="${FROST_BENCH_ITERATIONS:-5}"
daemon_targets="${FROST_BENCH_DAEMON_TARGETS:-10000}"
daemon_iterations="${FROST_BENCH_DAEMON_ITERATIONS:-31}"
stamp="$(date -u +%Y%m%dT%H%M%SZ)"
result_dir="${FROST_BENCH_RESULT_DIR:-bench/results/${stamp}}"
sample_dir="${FROST_BENCH_SAMPLE_DIR:-.frost-bench/reproduce-sample}"

mkdir -p "$result_dir"

python3 frost.py init-sample \
  --out "$sample_dir" \
  --groups 20 \
  --modules-per-group 8 \
  --cost-ms 30 \
  --force >/dev/null

python3 frost.py bench --workspace "$sample_dir" --jobs "$jobs" >"${result_dir}/frost-poc.json"

./frost-bench run \
  --suite standard \
  --tools "$tools" \
  --sizes "$sizes" \
  --iterations "$iterations" \
  --jobs "$jobs" \
  --workdir ".frost-bench/reproduce" \
  --out "${result_dir}/build-tools-standard.json" \
  >"${result_dir}/build-tools-standard.stdout.json"

cargo build --release --locked -p frostbuild-cli -p frostbuild-bench
cargo_target_dir="$(cargo metadata --format-version 1 --no-deps | \
  python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"
"${cargo_target_dir}/release/frost-bench-rs" daemon-graph \
  --frost "${cargo_target_dir}/release/frost" \
  --targets "$daemon_targets" \
  --iterations "$daemon_iterations" \
  --out "${result_dir}/daemon-graph.json" \
  >"${result_dir}/daemon-graph.txt"

cat <<EOF
Wrote benchmark reports:
  ${result_dir}/frost-poc.json
  ${result_dir}/build-tools-standard.json
  ${result_dir}/daemon-graph.json

Configuration:
  jobs=${jobs}
  tools=${tools}
  sizes=${sizes}
  iterations=${iterations}
  daemon_targets=${daemon_targets}
  daemon_iterations=${daemon_iterations}
  sample_dir=${sample_dir}
EOF
