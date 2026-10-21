#!/usr/bin/env bash
set -euo pipefail
grep -q 'pub struct Circle' src/geometry.rs
grep -q 'pub fn area' src/geometry.rs
grep -q 'pub fn circumference' src/geometry.rs
grep -q 'pub fn largest_circle' src/geometry.rs
cargo test --quiet
