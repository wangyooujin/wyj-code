#!/usr/bin/env bash
set -euo pipefail
grep -q 'pub fn clamp' src/stats.rs
grep -q 'max=' src/report.rs
cargo test --quiet
