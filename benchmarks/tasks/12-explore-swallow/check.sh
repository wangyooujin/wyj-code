#!/usr/bin/env bash
set -euo pipefail
out="$WYJ_STDOUT"
hits=0
for f in stats.rs geometry.rs vector.rs text.rs; do
    grep -q "$f" "$out" && hits=$((hits + 1))
done
if [ "$hits" -lt 3 ]; then
    echo "answer only covered $hits of 4 files" >&2
    exit 1
fi
# 至少要说出「返回默认值」这一形态与风险
grep -Eq '0\.0|空|zero' "$out"
grep -Eq '风险|risk|silently|静默' "$out"
diff -r "$WYJ_FIXTURE/src" src >/dev/null
