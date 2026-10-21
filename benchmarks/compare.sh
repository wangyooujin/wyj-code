#!/usr/bin/env bash
# 对比两份评测结果：benchmarks/compare.sh <baseline.jsonl> <candidate.jsonl>
#
# 同一个 task 可能有多条记录（`run.sh --repeat N`）。这里按 task 聚合：
# 成功率取**通过数/总数**而不是「最后一条」，token 类指标取均值。只看单条会
# 把随机性当成 harness 效果——外部实证里 harness 的差异量级（10–50pp）与模型
# 随机性的量级本来就重叠，单点对比没有意义。
set -euo pipefail
if [ $# -ne 2 ]; then
    echo "usage: $0 <baseline.jsonl> <candidate.jsonl>" >&2
    exit 1
fi
exec python3 - "$1" "$2" <<'EOF'
import json, sys
from collections import defaultdict

AVG_COLS = ["input_tokens", "output_tokens", "cache_read_tokens",
            "cache_write_tokens", "full_input_tokens", "cache_hit_ratio",
            "context_tokens", "context_window", "api_calls", "duration_secs",
            "wall_secs", "gate_blocks"]

def load(path):
    """按 task 聚合多条重复记录。

    `status=infra_error` 的记录（端点 503 / 限流 / 网络中断，一次请求都没成功）
    **直接排除**，既不算通过也不算失败。把它们算成失败会让一次上游抖动看起来像
    「模型能力下降」，那是评测里最典型的假信号。
    """
    raw = defaultdict(list)
    skipped = defaultdict(int)
    with open(path) as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            r = json.loads(line)
            if r.get("status") == "infra_error":
                skipped[r["task"]] += 1
                continue
            raw[r["task"]].append(r)
    agg = {}
    for task, rows in raw.items():
        n = len(rows)
        passed = sum(1 for r in rows if r.get("success"))
        out = {
            "runs": n,
            "passed": passed,
            "success_rate": passed / n if n else 0.0,
            "infra_errors": skipped.get(task, 0),
        }
        for col in AVG_COLS:
            vals = [r.get(col, 0) for r in rows]
            out[col] = sum(vals) / n if n else 0
        agg[task] = out
    # 整批都被排除的 task 也要露面，否则「这个任务没数据」会被静默吞掉。
    for task, n in skipped.items():
        if task not in agg:
            agg[task] = {"runs": 0, "passed": 0, "success_rate": 0.0,
                         "infra_errors": n, **{c: 0 for c in AVG_COLS}}
    return agg

def delta(key, b, c):
    if key == "success_rate":
        d = (c - b) * 100
        if abs(d) < 0.05:
            return "="
        return f"{d:+.0f}pp"
    if key in ("runs", "passed", "infra_errors"):
        return "=" if b == c else f"{c - b:+d}"
    if b == 0:
        return "n/a" if c == 0 else f"+{c:.0f}"
    return f"{(c - b) / b * 100:+.0f}%"

base, cand = load(sys.argv[1]), load(sys.argv[2])
tasks = sorted(set(base) | set(cand))
w = max([len(t) for t in tasks] + [4]) + 2

print(f"{'task':<{w}} {'metric':<18} {'baseline':>12} {'candidate':>12} {'delta':>10}")
print("-" * (w + 56))

tot = defaultdict(lambda: [0.0, 0.0, 0, 0])  # key -> [b_sum, c_sum, b_n, c_n]
for t in tasks:
    b, c = base.get(t), cand.get(t)
    if not b or not c:
        print(f"{t:<{w}} (missing in {'candidate' if not c else 'baseline'})")
        continue
    for key in ["success_rate", "runs", "infra_errors"] + AVG_COLS:
        bv, cv = b.get(key, 0), c.get(key, 0)
        if key == "success_rate":
            bs, cs = f"{bv*100:.0f}%", f"{cv*100:.0f}%"
        elif isinstance(bv, float):
            bs, cs = f"{bv:.1f}", f"{cv:.1f}"
        else:
            bs, cs = str(bv), str(cv)
        print(f"{t:<{w}} {key:<18} {bs:>12} {cs:>12} {delta(key, bv, cv):>10}")
        if key not in ("runs", "passed", "infra_errors"):
            tot[key][0] += bv
            tot[key][1] += cv
            tot[key][2] += 1
            tot[key][3] += 1
    print()

print(f"{'TOTAL (mean over tasks)':<{w}}")
for key in ["success_rate"] + AVG_COLS:
    bv = tot[key][0] / tot[key][2] if tot[key][2] else 0
    cv = tot[key][1] / tot[key][3] if tot[key][3] else 0
    if key == "success_rate":
        bs, cs = f"{bv*100:.0f}%", f"{cv*100:.0f}%"
    elif isinstance(bv, float):
        bs, cs = f"{bv:.1f}", f"{cv:.1f}"
    else:
        bs, cs = str(bv), str(cv)
    print(f"{'':<{w}} {key:<18} {bs:>12} {cs:>12} {delta(key, bv, cv):>10}")
EOF
