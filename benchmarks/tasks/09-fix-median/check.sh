#!/usr/bin/env bash
set -euo pipefail
# 偶数长度分支必须重新取两个中间值的平均
! grep -q '        sorted\[mid\]\n    }\n}' src/stats.rs
python3 - <<'PY'
from pathlib import Path
text = Path("src/stats.rs").read_text()
buggy = "    if sorted.len() % 2 == 0 {\n        sorted[mid]\n    } else {\n        sorted[mid]\n    }\n"
if buggy in text:
    raise SystemExit("bug 未被修掉")
PY
cargo test --quiet
