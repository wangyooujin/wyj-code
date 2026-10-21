#!/usr/bin/env bash
set -euo pipefail
# 源码里不应再有裸 unwrap()（测试模块内允许保留，因为断言失败本就是 panic 语义）
python3 - <<'PY'
import re, sys
from pathlib import Path

offenders = []
for path in Path("src").glob("*.rs"):
    text = path.read_text()
    # 去掉 #[cfg(test)] 之后的测试模块
    idx = text.find("#[cfg(test)]")
    if idx != -1:
        text = text[:idx]
    for m in re.finditer(r"\.unwrap\(\)", text):
        line = text[:m.start()].count("\n") + 1
        offenders.append(f"{path}:{line}")
if offenders:
    print("remaining unwrap(): " + ", ".join(offenders), file=sys.stderr)
    raise SystemExit(1)
PY
cargo test --quiet
