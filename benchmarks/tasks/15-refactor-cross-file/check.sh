#!/usr/bin/env bash
set -euo pipefail
grep -q 'pub struct Summary' src/report.rs
grep -q 'pub fn summarize_data' src/report.rs
grep -q 'pub fn summarize' src/report.rs
# 原有输出格式必须逐字符不变（原测试仍在且通过）
grep -q '"s: n=2 avg=3.00 median=3.00"' src/report.rs
cargo test --quiet
