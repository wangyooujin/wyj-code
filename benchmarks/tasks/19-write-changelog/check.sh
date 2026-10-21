#!/usr/bin/env bash
set -euo pipefail
test -f CHANGELOG.md
grep -q '^## \[Unreleased\]' CHANGELOG.md
# 至少两个小节、每节至少两条要点
sections=$(grep -c '^### ' CHANGELOG.md || true)
[ "$sections" -ge 2 ] || { echo "sections=$sections" >&2; exit 1; }
bullets=$(grep -c '^- ' CHANGELOG.md || true)
[ "$bullets" -ge 4 ] || { echo "bullets=$bullets" >&2; exit 1; }
grep -q 'vector' CHANGELOG.md
grep -q 'text' CHANGELOG.md
# 源码必须未被改动
diff -r "$WYJ_FIXTURE/src" src >/dev/null
cargo test --quiet
