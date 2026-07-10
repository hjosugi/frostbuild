#!/bin/sh
# Generates gen/config.h. Output is deliberately deterministic so that
# re-running the genrule with an unchanged script produces identical bytes:
# that exercises frost's early cutoff (downstream compiles stay cached).
set -eu
out="$1"
cat > "$out" <<'EOF'
#ifndef FROST_SAMPLE_CONFIG_H
#define FROST_SAMPLE_CONFIG_H
#define FROST_GREETING "frost:"
#endif
EOF
