#!/usr/bin/env bash
set -euo pipefail
# 必须恢复字符级语义
python3 - <<'PY'
from pathlib import Path
text = Path("src/text.rs").read_text()
if "text.chars().count()" not in text:
    raise SystemExit("未恢复按字符计数")
if "text.chars().take(max)" not in text:
    raise SystemExit("未恢复按字符截取")
PY
cargo test --quiet
