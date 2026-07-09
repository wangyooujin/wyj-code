#!/usr/bin/env bash
# 对比两份评测结果：benchmarks/compare.sh <baseline.jsonl> <candidate.jsonl>
set -euo pipefail
if [ $# -ne 2 ]; then
    echo "usage: $0 <baseline.jsonl> <candidate.jsonl>" >&2
    exit 1
fi
exec python3 - "$1" "$2" <<'EOF'
import json, sys

def load(path):
    rows = {}
    with open(path) as f:
        for line in f:
            line = line.strip()
            if line:
                r = json.loads(line)
                rows[r["task"]] = r
    return rows

base, cand = load(sys.argv[1]), load(sys.argv[2])
cols = ["success", "input_tokens", "output_tokens", "cache_read_tokens",
        "cache_write_tokens", "full_input_tokens", "cache_hit_ratio",
        "context_tokens", "context_window", "api_calls", "duration_secs"]

def delta(b, c):
    if isinstance(b, bool):
        return {(False, True): "✓ fixed", (True, False): "✗ broke"}.get((b, c), "=")
    if b == 0:
        return "n/a" if c == 0 else f"+{c}"
    pct = (c - b) / b * 100
    return f"{pct:+.0f}%"

tasks = sorted(set(base) | set(cand))
w = max(len(t) for t in tasks) + 2
print(f"{'task':<{w}} {'metric':<18} {'baseline':>12} {'candidate':>12} {'delta':>10}")
print("-" * (w + 56))
totals_b, totals_c = {}, {}
for t in tasks:
    b, c = base.get(t), cand.get(t)
    if not b or not c:
        print(f"{t:<{w}} (missing in {'candidate' if not c else 'baseline'})")
        continue
    for col in cols:
        bv, cv = b.get(col, 0), c.get(col, 0)
        if isinstance(bv, (int, float)) and not isinstance(bv, bool):
            totals_b[col] = totals_b.get(col, 0) + bv
            totals_c[col] = totals_c.get(col, 0) + cv
        print(f"{t:<{w}} {col:<18} {str(bv):>12} {str(cv):>12} {delta(bv, cv):>10}")
    print()
print(f"{'TOTAL':<{w}}")
for col in cols[1:]:
    bv, cv = totals_b.get(col, 0), totals_c.get(col, 0)
    bs = f"{bv:.1f}" if isinstance(bv, float) else str(bv)
    cs = f"{cv:.1f}" if isinstance(cv, float) else str(cv)
    print(f"{'':<{w}} {col:<18} {bs:>12} {cs:>12} {delta(bv, cv):>10}")
EOF
