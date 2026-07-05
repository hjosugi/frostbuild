#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

if ! command -v bazel >/dev/null 2>&1; then
  echo "bazel is not installed; skipping Bazel comparison."
  echo "Run: python3 frost.py bench --workspace sample --jobs 8"
  exit 0
fi

python3 frost.py init-sample --out sample --groups 20 --modules-per-group 8 --cost-ms 30 --force >/dev/null
python3 frost.py bench --workspace sample --jobs "${JOBS:-8}" --with-bazel
