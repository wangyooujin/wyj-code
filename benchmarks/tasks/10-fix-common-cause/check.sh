#!/usr/bin/env bash
set -euo pipefail
# 空输入分支必须恢复
grep -q 'if values.is_empty()' src/stats.rs
grep -q 'if radius < 0.0' src/geometry.rs
cargo test --quiet
