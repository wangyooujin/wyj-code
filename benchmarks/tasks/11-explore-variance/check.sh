#!/usr/bin/env bash
set -euo pipefail
out="$WYJ_STDOUT"
grep -q 'src/stats.rs' "$out"
grep -q 'variance' "$out"
# 单元素：均值等于该元素，偏差为 0 → 0.0
grep -Eq '(^|[^0-9.])0(\.0+)?([^0-9.]|$)' "$out"
# 空输入：早退返回 0.0
grep -Eq 'empty|空' "$out"
diff -r "$WYJ_FIXTURE/src" src >/dev/null
