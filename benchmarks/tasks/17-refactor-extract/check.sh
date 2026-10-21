#!/usr/bin/env bash
set -euo pipefail
grep -q 'pub fn truncate_with' src/text.rs
grep -q 'pub fn truncate' src/text.rs
# truncate 必须复用新函数，而不是复制一份实现
python3 - <<'PY'
from pathlib import Path
text = Path("src/text.rs").read_text()
if "truncate_with(text, max, " not in text:
    raise SystemExit("truncate 未复用 truncate_with")
PY
cargo test --quiet
