#!/usr/bin/env bash
set -euo pipefail
grep -q 'pub fn percentile' src/stats.rs
grep -q 'p90=' src/report.rs
# 边界测试必须存在
grep -Eq 'percentile\([^)]*,\s*0\.0' src/stats.rs
grep -Eq 'percentile\([^)]*,\s*100\.0' src/stats.rs
cargo test --quiet
