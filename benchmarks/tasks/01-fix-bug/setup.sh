#!/usr/bin/env bash
# 向 fixture 注入 off-by-one bug：avg 分母多加 1。
#
# **不要用 `sed -i`**：BSD sed 要求 `sed -i ''`、GNU sed 要求 `sed -i`，
# 两种写法在对方平台上都会失败或产生多余文件。基准脚本必须在 macOS 与 Linux
# CI 上跑出同样的结果，所以一律走 python3（compare.sh 已经依赖它）。
set -euo pipefail
python3 - <<'PY'
from pathlib import Path

path = Path("src/stats.rs")
text = path.read_text()
old = "    sum / values.len() as f64\n"
new = "    sum / (values.len() as f64 + 1.0)\n"
if old not in text:
    raise SystemExit("setup: 未找到待注入的 avg 分母表达式")
path.write_text(text.replace(old, new, 1))
PY
