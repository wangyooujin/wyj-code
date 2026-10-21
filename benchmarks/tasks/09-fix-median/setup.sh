#!/usr/bin/env bash
# 注入 median 偶数分支 bug：丢掉两个中间值的平均，直接取后一个。
set -euo pipefail
python3 - <<'PY'
from pathlib import Path

path = Path("src/stats.rs")
text = path.read_text()
old = "        (sorted[mid - 1] + sorted[mid]) / 2.0\n"
new = "        sorted[mid]\n"
if old not in text:
    raise SystemExit("setup: 未找到待注入的 median 偶数分支")
path.write_text(text.replace(old, new, 1))
PY
