#!/usr/bin/env bash
set -euo pipefail
grep -q 'pub fn scale' src/vector.rs
grep -q 'pub fn lerp' src/vector.rs
grep -q 'pub fn describe_vec' src/report.rs
grep -q 'len=' src/report.rs
cargo test --quiet
