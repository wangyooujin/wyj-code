#!/usr/bin/env bash
# 注入一个「共同根因」bug：把三处「非法/空输入的防御性早退」一起删掉。
# 表面上是三个互不相关的测试失败（stats 空输入、geometry 负半径、vector 零向量），
# 实际根因是同一类遗漏：输入校验被移除。任务考的是「先归纳再动手」。
set -euo pipefail
python3 - <<'PY'
from pathlib import Path

edits = [
    ("src/stats.rs",
     "    if values.is_empty() {\n        return 0.0;\n    }\n    let sum",
     "    let sum"),
    ("src/geometry.rs",
     "    if radius < 0.0 {\n        return 0.0;\n    }\n    PI * radius * radius",
     "    PI * radius * radius"),
    ("src/vector.rs",
     "        let n = self.norm();\n        if n == 0.0 {\n            return Self { x: 0.0, y: 0.0 };\n        }\n",
     "        let n = self.norm();\n"),
]
for rel, old, new in edits:
    path = Path(rel)
    text = path.read_text()
    if old not in text:
        raise SystemExit(f"setup: 未在 {rel} 找到待注入片段")
    path.write_text(text.replace(old, new, 1))
PY
