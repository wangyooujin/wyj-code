#!/usr/bin/env bash
set -euo pipefail
# 注入的 bug 必须被修掉：原实现用了 bytes 而不是 chars
! grep -q 'text.len() <= max' src/text.rs
! grep -q '&text[..max]' src/text.rs
# 仍然要保留字符级语义
grep -q 'chars()' src/text.rs
cargo test --quiet
