#!/usr/bin/env bash
set -euo pipefail
# 回答（stdout 落在 $WYJ_STDOUT）必须覆盖 stats 与 geometry 两处的空/非法输入处理
out="$WYJ_STDOUT"
grep -q 'stats.rs' "$out"
grep -q 'geometry.rs' "$out"
grep -q 'circle_area' "$out"
grep -Eq 'avg|median|variance' "$out"
# 源码必须未被修改（与 fixture 原始内容逐字节一致）
diff -r "$WYJ_FIXTURE/src" src >/dev/null
