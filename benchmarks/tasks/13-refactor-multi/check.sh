#!/usr/bin/env bash
set -euo pipefail
grep -q 'pub fn mean' src/stats.rs
grep -q 'pub fn length' src/vector.rs
grep -q 'pub fn length_sq' src/vector.rs
! grep -rqw 'avg' src/
! grep -rqw 'norm' src/
! grep -rqw 'norm_sq' src/
cargo test --quiet
