#!/usr/bin/env bash
set -euo pipefail
# 功能改动必须真的做了（单遍实现）
python3 - <<'PY'
from pathlib import Path
text = Path("src/stats.rs").read_text()
start = text.index("pub fn variance")
body = text[start:start + 900]
# 单遍实现不应再调用 avg()
if "avg(values)" in body or "mean(values)" in body:
    raise SystemExit("variance 仍在调用 avg/mean，不是单遍实现")
PY
# 项目规约要求的改动记录必须存在
test -f CHANGELOG.md
grep -q '^## \[Unreleased\]' CHANGELOG.md
grep -q '^- ' CHANGELOG.md
cargo test --quiet
