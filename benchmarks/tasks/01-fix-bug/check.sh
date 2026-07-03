#!/usr/bin/env bash
set -euo pipefail
# bug 必须被修掉，且测试未被篡改、全部通过
! grep -q 'values.len() as f64 + 1.0' src/stats.rs
grep -q 'assert_eq!(avg(&\[2.0, 4.0\]), 3.0);' src/stats.rs
cargo test --quiet
