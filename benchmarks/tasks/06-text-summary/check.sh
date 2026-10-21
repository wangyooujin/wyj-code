#!/usr/bin/env bash
set -euo pipefail
grep -q 'pub fn word_count' src/text.rs
grep -q 'pub fn longest_word' src/text.rs
grep -q 'pub fn describe_text' src/report.rs
grep -q 'longest=' src/report.rs
cargo test --quiet
