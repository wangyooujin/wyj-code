#!/usr/bin/env bash
# 向 fixture 注入 off-by-one bug：avg 分母多加 1
set -euo pipefail
sed -i '' 's|sum / values.len() as f64|sum / (values.len() as f64 + 1.0)|' src/stats.rs
