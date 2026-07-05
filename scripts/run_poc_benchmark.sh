#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
python3 frost.py init-sample --out sample --groups 20 --modules-per-group 8 --cost-ms 30 --force >/dev/null
python3 frost.py bench --workspace sample --jobs "${JOBS:-8}"
