#!/usr/bin/env bash
# Build a project at N consecutive commits and collect the build artifacts.
# Each commit's artifacts land in $OUT/<seq>_<sha>/ so the analyzer can replay
# them in order, exactly as a build cache would see them over time.
#
# Usage:
#   ./collect_corpus.sh <repo-url> <n-commits> <out-dir> <build-cmd> <artifact-glob...>
#
# Example (Lua):
#   ./collect_corpus.sh https://github.com/lua/lua 30 corpus/lua "make -j1 linux" '*.o' 'lua' 'liblua.a'

set -uo pipefail

REPO="$1"; N="$2"; OUT="$3"; BUILD="$4"; shift 4
GLOBS=("$@")

WORK="$(mktemp -d)"
mkdir -p "$OUT"

echo "[corpus] cloning $REPO"
git clone -q "$REPO" "$WORK/src" || exit 1
cd "$WORK/src"

# Oldest-first so the replay order matches real development order.
mapfile -t SHAS < <(git log --format=%H -n "$N" | tac)
echo "[corpus] ${#SHAS[@]} commits"

i=0
for SHA in "${SHAS[@]}"; do
  i=$((i + 1))
  DEST="$OUT/$(printf '%03d' $i)_${SHA:0:12}"
  if [ -d "$DEST" ]; then echo "[corpus] $i skip (exists)"; continue; fi

  git checkout -q -f "$SHA" 2>/dev/null || { echo "[corpus] $i checkout failed"; continue; }
  git clean -qfdx 2>/dev/null

  START=$(date +%s)
  if ! eval "$BUILD" > "$WORK/build.log" 2>&1; then
    echo "[corpus] $i BUILD FAILED ${SHA:0:12} (see log tail)"
    tail -3 "$WORK/build.log"
    continue
  fi
  ELAPSED=$(( $(date +%s) - START ))

  mkdir -p "$DEST"
  COUNT=0
  for G in "${GLOBS[@]}"; do
    while IFS= read -r -d '' F; do
      # Flatten path into the filename so same-named files in different dirs
      # stay distinct; the analyzer uses this as the artifact identity key.
      REL="${F#./}"
      cp "$F" "$DEST/${REL//\//__}" 2>/dev/null && COUNT=$((COUNT + 1))
    done < <(find . -name "$G" -type f -print0 2>/dev/null)
  done

  echo "$SHA" > "$DEST/.sha"
  echo "[corpus] $i ${SHA:0:12} ${ELAPSED}s ${COUNT} artifacts"
done

echo "[corpus] done -> $OUT"
rm -rf "$WORK"
