#!/usr/bin/env bash
# 任务集自检：不调用模型、不花 API 费用，只验证 20 个任务的
# setup.sh / check.sh / prompt.md 在结构上是对的。
#
# 检查三件事（缺一不可，否则跑批出来的数字没有意义）：
#
# 1. **每个任务三个文件齐全**（prompt.md 必填；setup.sh / check.sh 按需）。
# 2. **注入的 bug 真的能让 fixture 变红**——有 setup.sh 的任务，执行后
#    `cargo test` 必须失败。否则这个任务测的是「什么都不做也能过」。
# 3. **check.sh 在未修复时必定失败**——否则判定太松，任何行为都算成功。
#    对「修 bug」类任务，还会额外验证「恢复 fixture 源码后 check 通过」，
#    确保判定不是恒假。
#
# 用法：benchmarks/verify-tasks.sh [任务名过滤...]
set -uo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
FIXTURE="$ROOT/fixtures/mathlib"
FAILED=0
CHECKED=0

# 修 bug 类任务：恢复 fixture 源码应当让 check 通过。
# 这份名单必须手工维护——脚本无法从 prompt.md 推断任务语义。
FIX_TASKS="01-fix-bug 07-fix-truncate 09-fix-median 10-fix-common-cause 18-fix-boundary"

is_fix_task() {
    local name="$1"
    for t in $FIX_TASKS; do [ "$t" = "$name" ] && return 0; done
    return 1
}

for task_dir in "$ROOT"/tasks/*/; do
    task=$(basename "$task_dir")
    if [ $# -gt 0 ]; then
        keep=false
        for f in "$@"; do [[ "$task" == *"$f"* ]] && keep=true; done
        $keep || continue
    fi
    CHECKED=$((CHECKED + 1))

    if [ ! -f "$task_dir/prompt.md" ]; then
        echo "FAIL $task: 缺 prompt.md"
        FAILED=$((FAILED + 1))
        continue
    fi
    if [ ! -f "$task_dir/check.sh" ]; then
        echo "FAIL $task: 缺 check.sh"
        FAILED=$((FAILED + 1))
        continue
    fi

    work=$(mktemp -d "${TMPDIR:-/tmp}/wyj-verify-XXXXXX")
    cp -R "$FIXTURE/." "$work/"

    if [ -f "$task_dir/setup.sh" ]; then
        if ! (cd "$work" && bash "$task_dir/setup.sh" >/dev/null 2>&1); then
            echo "FAIL $task: setup.sh 执行失败"
            FAILED=$((FAILED + 1))
            rm -rf "$work"
            continue
        fi
        # 注入后必须变红
        if (cd "$work" && cargo test --quiet >/dev/null 2>&1); then
            echo "FAIL $task: setup.sh 注入后 cargo test 仍然通过（bug 没生效）"
            FAILED=$((FAILED + 1))
            rm -rf "$work"
            continue
        fi
    fi

    # 未修复时 check 必须失败
    if (cd "$work" && WYJ_STDOUT=/dev/null WYJ_FIXTURE="$FIXTURE" \
            bash "$task_dir/check.sh" >/dev/null 2>&1); then
        echo "FAIL $task: 未修复时 check.sh 竟然通过（判定太松）"
        FAILED=$((FAILED + 1))
        rm -rf "$work"
        continue
    fi

    # 修 bug 类：恢复 fixture 源码后 check 必须通过
    if is_fix_task "$task"; then
        rm -rf "$work/src"
        cp -R "$FIXTURE/src" "$work/src"
        if ! (cd "$work" && WYJ_STDOUT=/dev/null WYJ_FIXTURE="$FIXTURE" \
                bash "$task_dir/check.sh" >/dev/null 2>&1); then
            echo "FAIL $task: 恢复 fixture 源码后 check 仍然失败（判定恒假）"
            FAILED=$((FAILED + 1))
            rm -rf "$work"
            continue
        fi
    fi

    echo "ok   $task"
    rm -rf "$work"
done

echo ""
if [ "$FAILED" -gt 0 ]; then
    echo "✗ $FAILED/$CHECKED 个任务有问题"
    exit 1
fi
echo "✓ $CHECKED 个任务结构自检通过"
