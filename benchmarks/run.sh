#!/usr/bin/env bash
# 评测基准：对 benchmarks/tasks/ 下每个任务，从 fixture 复制出临时工作区，
# 用 headless -p + --bypass-permissions 跑 wyj-code，记录 token/轮次/时长/成败
# 到 results/<git-sha>-<timestamp>.jsonl，供 compare.sh 做改进前后对比。
#
# 用法：benchmarks/run.sh [任务名过滤...]   （无参数=跑全部）
# 环境：WYJ_BIN 覆盖二进制路径（默认 target/release/wyj-code）
set -uo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$ROOT/.." && pwd)"
BIN="${WYJ_BIN:-$REPO/target/release/wyj-code}"
FIXTURE="$ROOT/fixtures/mathlib"

if [ ! -x "$BIN" ]; then
    echo "error: binary not found: $BIN  (先 cargo build --release)" >&2
    exit 1
fi

SHA=$(git -C "$REPO" rev-parse --short HEAD 2>/dev/null || echo "nosha")
TS=$(date +%Y%m%d-%H%M%S)
OUT="$ROOT/results/$SHA-$TS.jsonl"
mkdir -p "$ROOT/results"

for task_dir in "$ROOT"/tasks/*/; do
    task=$(basename "$task_dir")
    if [ $# -gt 0 ]; then
        keep=false
        for f in "$@"; do [[ "$task" == *"$f"* ]] && keep=true; done
        $keep || continue
    fi

    work=$(mktemp -d "${TMPDIR:-/tmp}/wyj-bench-XXXXXX")
    cp -R "$FIXTURE/." "$work/"
    if [ -f "$task_dir/setup.sh" ]; then
        (cd "$work" && bash "$task_dir/setup.sh") || {
            echo "$task: setup.sh failed" >&2
            rm -rf "$work"
            continue
        }
    fi

    prompt=$(cat "$task_dir/prompt.md")
    stdout_file="$work/.wyj-stdout.log"
    stderr_file="$work/.wyj-stderr.log"

    echo "=== $task ==="
    start=$(date +%s)
    WYJ_STATS_JSON=1 "$BIN" -p "$prompt" --bypass-permissions --cwd "$work" \
        >"$stdout_file" 2>"$stderr_file"
    rc=$?
    wall=$(( $(date +%s) - start ))

    # 不锚定行首：stderr 里 thinking 增量可能不带换行结尾，JSON 会接在其后
    stats=$(grep -oE '\{"input_tokens".*\}' "$stderr_file" | tail -1)
    if [ -z "$stats" ]; then
        stats='{"input_tokens":0,"output_tokens":0,"cache_read_tokens":0,"cache_write_tokens":0,"api_calls":0,"duration_secs":0.0}'
    fi

    ok=false
    if [ "$rc" -eq 0 ] && (cd "$work" && WYJ_STDOUT="$stdout_file" WYJ_FIXTURE="$FIXTURE" \
            bash "$task_dir/check.sh" >/dev/null 2>&1); then
        ok=true
    fi

    echo "{\"task\":\"$task\",\"success\":$ok,\"wall_secs\":$wall,${stats#\{}" >>"$OUT"
    echo "  success=$ok wall=${wall}s $stats"
    rm -rf "$work"
done

echo ""
echo "results: $OUT"
