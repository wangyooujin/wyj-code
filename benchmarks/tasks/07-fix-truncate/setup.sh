#!/usr/bin/env bash
# 注入「按字节截断」bug：把字符级实现换成字节级实现。
# 这是真实世界里最常见的 UTF-8 事故形态，且 `cargo test` 会 panic 在
# 字符边界上——模型必须读懂 panic 才能定位。
set -euo pipefail
python3 - <<'PY'
from pathlib import Path

path = Path("src/text.rs")
text = path.read_text()
old = """    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
"""
new = """    if text.len() <= max {
        return text.to_string();
    }
    let mut out: String = text[..max].to_string();
    out.push('…');
    out
"""
if old not in text:
    raise SystemExit("setup: 未找到待注入的 truncate 实现")
path.write_text(text.replace(old, new, 1))
PY
