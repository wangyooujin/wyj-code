# benchmarks — 轻量评测基准

固定 4 个真实任务，headless 跑 wyj-code，记录 token 消耗 / API 轮次 / 时长 / 成败，
用于效率改进的前后对比。

```bash
cargo build --release
benchmarks/run.sh                 # 跑全部任务 → results/<sha>-<ts>.jsonl
benchmarks/run.sh explore         # 只跑名字含 explore 的任务
benchmarks/compare.sh results/A.jsonl results/B.jsonl   # 前后对比表
```

## 任务集

| 任务 | 类型 | 判定 |
|---|---|---|
| 01-fix-bug | 修 fixture 中注入的 off-by-one bug | 测试通过且未改测试 |
| 02-add-feature | 跨 2 个文件加小功能 + 测试 | grep 特征 + 测试通过 |
| 03-explore | 纯探索问答（禁止改文件） | 回答关键词 + 源码未变 |
| 04-refactor | 跨文件重命名 avg → mean | 无残留 avg + 测试通过 |

## 机制

- 每个任务从 `fixtures/mathlib`（无依赖小型 Rust 项目）复制出临时工作区，
  可选 `setup.sh` 注入 bug；`check.sh` 退出码判定成败（cwd=工作区，
  `$WYJ_STDOUT` 指向模型 stdout，`$WYJ_FIXTURE` 指向原始 fixture）。
- 统计来自 `WYJ_STATS_JSON=1` 时 wyj-code `-p` 路径在 stderr 输出的单行 JSON。
- 注意：跑一轮消耗真实 API 费用；结果受模型随机性影响，对比时以趋势为准，
  必要时同一 commit 跑 2~3 轮取均值。
