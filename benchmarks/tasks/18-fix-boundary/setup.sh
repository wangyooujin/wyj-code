#!/usr/bin/env bash
# 与 07 同源的 bug，但换一个注入点：把 chars().take 换成按字节切（会 panic），
# 而 chars().count() 保留——这样只有「截取」这一半坏了，排查面更窄、更考验
# 模型是否能读出「count 判断是对的、截取是错的」这个不对称。
set -euo pipefail
python3 - <<'PY'
from pathlib import Path

path = Path("src/text.rs")
text = path.read_text()
old = "    let mut out: String = text.chars().take(max).collect();\n"
new = "    let mut out: String = text[..max].to_string();\n"
if old not in text:
    raise SystemExit("setup: 未找到待注入的截取语句")
path.write_text(text.replace(old, new, 1))
PY
