#!/usr/bin/env bash
set -euo pipefail
grep -q 'pub fn mean' src/stats.rs
! grep -rqw 'avg' src/
cargo test --quiet
