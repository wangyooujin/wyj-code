#!/usr/bin/env bash
set -euo pipefail
out="$WYJ_STDOUT"
grep -q 'src/vector.rs' "$out"
grep -q 'Vec2::new' "$out"
grep -q 'normalized' "$out"
grep -Eq 'NaN|非有限|not finite|non-finite' "$out"
grep -Eq '零|zero' "$out"
# 纯探索任务：源码必须未被修改
diff -r "$WYJ_FIXTURE/src" src >/dev/null
