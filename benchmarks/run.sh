#!/usr/bin/env bash
# 评测基准：对 benchmarks/tasks/ 下每个任务，从 fixture 复制出临时工作区，
# 用 headless -p + --bypass-permissions 跑 wyj-code，记录 token/轮次/时长/成败
# 到 results/<git-sha>-<timestamp>.jsonl，供 compare.sh 做改进前后对比。
#
# 用法：
#   benchmarks/run.sh                       # 跑全部任务，每组 1 次
#   benchmarks/run.sh explore               # 只跑名字含 explore 的任务
#   benchmarks/run.sh --repeat 3            # 每组跑 3 次（消融协议要求 ≥3）
#   benchmarks/run.sh --acceptance          # 给工作区声明验收契约后再跑
#   benchmarks/run.sh --label gate-on       # 给本批结果打标签，便于 compare.sh 对齐
#   benchmarks/run.sh --bin path/to/wyj-code
#
# 环境：WYJ_BIN 覆盖二进制路径（默认 target/release/wyj-code）
#
# ## 为什么要有 --repeat
#
# 单次运行不可信：harness 差异的量级（10–50pp）与模型随机性的量级在同一区间，
# 一次跑出「A 比 B 好」很可能是噪声。消融协议要求每组至少 3 次重复，看分布
# 而不是看单点。
#
# ## 为什么要有 --acceptance
#
# 验收门禁（`core::acceptance`）是**条件性生效**的：项目没在
# `.wyj-code/settings.json` 里声明 `[acceptance]` 就完全不参与。要测「门禁开 vs
# 关」的差异，就必须让同一批任务在两种状态下各跑一遍——这个开关就是干这个的。
# 注意它会先 `trust-hooks`：门禁的 command 判据与 `verify.command` 同级，未批准
# 会被静默跳过，那样测出来的就是「门禁根本没生效」而不是「门禁关着」。
set -uo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$ROOT/.." && pwd)"
BIN="${WYJ_BIN:-$REPO/target/release/wyj-code}"
FIXTURE="$ROOT/fixtures/mathlib"
REPEAT=1
USE_ACCEPTANCE=0
LABEL=""
# 选项解析完后剩下的位置参数才是任务名过滤词。不能用 `$#` 判「有没有过滤词」
# ——选项已被 shift 掉，但只要还剩下过滤词就必须继续筛。这里显式收集。
FILTERS=()

while [ $# -gt 0 ]; do
    case "$1" in
        --repeat) REPEAT="$2"; shift 2 ;;
        --acceptance) USE_ACCEPTANCE=1; shift ;;
        --label) LABEL="$2"; shift 2 ;;
        --bin) BIN="$2"; shift 2 ;;
        -h|--help) sed -n '2,30p' "$0"; exit 0 ;;
        --) shift; FILTERS+=("$@"); break ;;
        -*) echo "error: unknown option: $1" >&2; exit 1 ;;
        *) FILTERS+=("$1"); shift ;;
    esac
done

if [ ! -x "$BIN" ]; then
    echo "error: binary not found: $BIN  (先 cargo build --release)" >&2
    exit 1
fi

SHA=$(git -C "$REPO" rev-parse --short HEAD 2>/dev/null || echo "nosha")
TS=$(date +%Y%m%d-%H%M%S)
SUFFIX="${LABEL:+-$LABEL}"
OUT="$ROOT/results/$SHA-$TS$SUFFIX.jsonl"
mkdir -p "$ROOT/results"

# 本批的配置标签：写进每条记录，方便事后按配置分组而不是靠文件名猜。
CONFIG="acceptance=$USE_ACCEPTANCE"

# 验收契约：判据都是 fixture 里能自动判的东西。
# `tests-pass` 在这里是对的——fixture 本身是绿的，注入 bug 之后才红，所以它
# 恰好等价于「bug 修好了」。
ACCEPTANCE_JSON='{
  "acceptance": {
    "max_retries": 2,
    "criteria": [
      { "id": "tests-pass", "kind": "command", "command": "cargo test --quiet" },
      { "id": "no-todo-left", "kind": "grep_absent", "pattern": "todo!\\(|unimplemented!\\(", "paths": ["src"] }
    ]
  }
}'

run_one() {
    local task_dir="$1" task="$2" rep="$3"
    local work
    work=$(mktemp -d "${TMPDIR:-/tmp}/wyj-bench-XXXXXX")
    cp -R "$FIXTURE/." "$work/"

    if [ -f "$task_dir/setup.sh" ]; then
        (cd "$work" && bash "$task_dir/setup.sh") || {
            echo "$task: setup.sh failed" >&2
            rm -rf "$work"
            return
        }
    fi

    if [ "$USE_ACCEPTANCE" -eq 1 ]; then
        mkdir -p "$work/.wyj-code"
        # 任务目录里可以放 `acceptance.json` 覆盖默认契约——21-gate-changelog 这类
        # 任务需要「改动记录必须存在」这种专属判据，用默认那两条测不出来。
        if [ -f "$task_dir/acceptance.json" ]; then
            cp "$task_dir/acceptance.json" "$work/.wyj-code/settings.json"
        else
            printf '%s\n' "$ACCEPTANCE_JSON" >"$work/.wyj-code/settings.json"
        fi
        # 门禁的 command 判据与 verify.command 同级，必须过信任门控。
        (cd "$work" && "$BIN" trust-hooks >/dev/null 2>&1) || true
    fi

    local prompt stdout_file stderr_file start rc wall stats ok gate_blocks
    prompt=$(cat "$task_dir/prompt.md")
    stdout_file="$work/.wyj-stdout.log"
    stderr_file="$work/.wyj-stderr.log"

    echo "=== $task (rep $rep/$REPEAT) ==="
    start=$(date +%s)
    WYJ_STATS_JSON=1 "$BIN" -p "$prompt" --bypass-permissions --cwd "$work" \
        >"$stdout_file" 2>"$stderr_file"
    rc=$?
    wall=$(( $(date +%s) - start ))

    # 不锚定行首：stderr 里 thinking 增量可能不带换行结尾，JSON 会接在其后
    stats=$(grep -oE '\{"input_tokens".*\}' "$stderr_file" | tail -1)
    if [ -z "$stats" ]; then
        stats='{"input_tokens":0,"output_tokens":0,"cache_read_tokens":0,"cache_write_tokens":0,"full_input_tokens":0,"cache_hit_ratio":0.0,"context_tokens":0,"context_window":0,"api_calls":0,"duration_secs":0.0}'
    fi

    ok=false
    if [ "$rc" -eq 0 ] && (cd "$work" && WYJ_STDOUT="$stdout_file" WYJ_FIXTURE="$FIXTURE" \
            bash "$task_dir/check.sh" >/dev/null 2>&1); then
        ok=true
    fi

    # ── 区分「基础设施失败」与「模型失败」──────────────────────────────
    # 端点 503 / 限流 / 网络中断时，`-p` 会在**一个 token 都没花**的情况下退出。
    # 把它记成 `success=false` 会直接污染基线：看起来像「模型做不出来」，实际是
    # 「请求根本没发出去」。这类记录必须单独标 `status=infra_error`，并在统计时
    # **排除**而不是算作失败——否则一次上游抖动就会让整批对比失真。
    #
    # 判据：`api_calls == 0` 且 `rc != 0`。用 api_calls 而不是匹配 stderr 文本，
    # 因为错误文案会随端点变化，而「一次请求都没成功」是结构性事实。
    local api_calls=0
    api_calls=$(printf '%s' "$stats" | sed -n 's/.*"api_calls":\([0-9]*\).*/\1/p')
    api_calls=${api_calls:-0}
    local status="ok"
    if [ "$rc" -ne 0 ] && [ "$api_calls" -eq 0 ]; then
        status="infra_error"
        ok=false
        echo "  ! infra_error: no request succeeded (rc=${rc}); this row is excluded from the baseline" >&2
        tail -3 "$stderr_file" | sed 's/^/    /' >&2
    fi

    # 门禁是否真的拦下过东西：数 stdout 里的验收失败提示。
    gate_blocks=$(grep -c 'Acceptance failed\|验收未通过' "$stdout_file" 2>/dev/null || true)
    gate_blocks=${gate_blocks:-0}

    echo "{\"task\":\"$task\",\"rep\":$rep,\"config\":\"$CONFIG\",\"status\":\"$status\",\"success\":$ok,\"gate_blocks\":$gate_blocks,\"wall_secs\":$wall,${stats#\{}" >>"$OUT"
    echo "  status=$status success=$ok wall=${wall}s gate_blocks=$gate_blocks $stats"
    rm -rf "$work"
}

for task_dir in "$ROOT"/tasks/*/; do
    task=$(basename "$task_dir")
    if [ ${#FILTERS[@]} -gt 0 ]; then
        keep=false
        for f in "${FILTERS[@]}"; do [[ "$task" == *"$f"* ]] && keep=true; done
        $keep || continue
    fi
    for rep in $(seq 1 "$REPEAT"); do
        run_one "$task_dir" "$task" "$rep"
    done
done

echo ""
echo "results: $OUT"
