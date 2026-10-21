# benchmarks — harness 评测基线

20 个真实任务，headless 跑 wyj-code，记录 token / API 轮次 / 时长 / 成败 / 门禁拦截次数，
用于 **harness 改动的前后对比**。

这套东西存在的理由，比「跑个分」具体得多：**「模型 X 在 wyj-code 里表现如何」这个命题，
严格说是不成立的**——被评测的从来不是模型，而是「模型 + 当前 harness」这个配置
（arXiv:2609.07785）。外部受控实验里 harness 的方差可达模型方差的 **7.8 倍**，9 组模型
配对中 **6 次**排名被换 harness 翻转（arXiv:2605.23950）。所以任何「这次改动让模型更强
还是更弱」的说法，都必须先有一个能分离这两者的测量口径。这就是它。

```bash
cargo build --release

benchmarks/verify-tasks.sh              # 先自检任务集（不花 API 费用）
benchmarks/run.sh                       # 跑全部 20 个任务
benchmarks/run.sh --repeat 3            # 每组 3 次（消融协议要求 ≥3）
benchmarks/run.sh --acceptance          # 开启验收门禁后再跑
benchmarks/run.sh explore               # 只跑名字含 explore 的任务

benchmarks/compare.sh results/A.jsonl results/B.jsonl   # 前后对比
```

## 任务集（20 个，五类）

| 类 | 任务 | 判定 |
|---|---|---|
| **T1 多文件功能实现** | 02-add-feature · 05-vec-ops · 06-text-summary · 14-add-geometry-feature · 16-add-percentile | grep 特征 + `cargo test` |
| **T2 bug 定位修复** | 01-fix-bug · 07-fix-truncate · 09-fix-median · 10-fix-common-cause · 18-fix-boundary | 注入点消失 + 测试未改 + `cargo test` |
| **T3 仓库理解问答** | 03-explore · 08-explore-vector · 11-explore-variance · 12-explore-swallow | stdout 关键词 + **源码逐字节未变** |
| **T4 长程重构** | 04-refactor · 13-refactor-multi · 15-refactor-cross-file · 17-refactor-extract · 20-refactor-unwrap | 新标识符存在 + 旧标识符消失 + `cargo test` |
| **T5 文档产出** | 19-write-changelog | 结构断言（标题/小节数/要点数）+ 源码未变 |

**为什么这几类**：T1/T2/T4 覆盖「改代码」，T3 覆盖「不改代码但要答对」，T5 覆盖
「产出非代码产物」。四类的失败模式完全不同——T3 的典型失败是「凭印象答」而不是「读源码」，
所以它的判据里有一条 `diff -r` 要求源码逐字节未变。

## 几个刻意的设计

**判定必须双向。** `verify-tasks.sh` 会验证三件事：注入 bug 后 `cargo test` 必须**变红**；
未修复时 `check.sh` 必须**失败**；恢复 fixture 源码后 `check.sh` 必须**通过**。少了任何一条，
这个任务测的都是「什么都不做也能过」或「永远过不了」。

**探索类任务的判据要求源码未变。** T3 的 `check.sh` 都带 `diff -r "$WYJ_FIXTURE/src" src`。
一个「顺手把代码改了」的答案即使文字对，也不算完成——这正是 `acceptance` 想拦的那类行为。

**任务 07/18 是一对。** 同一个 UTF-8 字节切片的 bug，07 把整段实现换掉（`count` 判断和
`take` 截取都坏），18 只坏截取那一半。后者更窄，考的是「能不能读出 count 是对的、截取是错的」
这个不对称。

**任务 10 考归纳。** 三处防御性早退被一起删掉，表面上是三个互不相关的测试失败。prompt 里
明确说「这些失败不是彼此独立的」，看模型会不会先归纳再动手。

**任务 07 的 prompt 里埋了一个错误假设。** 它说「用户猜测是因为 max 恰好等于字符数时仍然
追加了省略号」——这个猜测是错的（真正的 bug 是按字节切）。任务要求模型**证伪用户的猜测**。
这对应真实世界最常见的失败模式：用户给的定位是错的，模型照着修。

## 消融协议

跑对比时守三条，否则数字没有意义：

1. **一次只改一个组件。** 同时改两处，任何结果都无法归因。
2. **每组至少 3 次重复**（`--repeat 3`）。harness 差异（10–50pp）与模型随机性的量级重叠，
   单次运行分不开。`compare.sh` 按 task 聚合多条记录，成功率取通过数/总数。
3. **至少 2 个模型对照。** 只跑一个模型只能得到「这个配置」的分数，无法说模型本身如何。

`run.sh` 会把 `config=acceptance=N` 写进每条记录，`compare.sh` 按 task 聚合后对比。
`gate_blocks` 列是**门禁实际拦下过几次**——它是验证门禁是否真的生效的硬指标，而不是
「配置里写了 `[acceptance]`」。

## 目录

```
benchmarks/
├── fixtures/mathlib/     # 无依赖小型 Rust 项目（14 个单测，全绿）
├── tasks/<NN-name>/
│   ├── prompt.md         # 交给模型的原文
│   ├── setup.sh          # 可选：注入 bug / 破坏代码
│   └── check.sh          # 退出码即判定；cwd=工作区
├── run.sh                # 跑批
├── compare.sh            # 前后对比（支持重复聚合）
├── verify-tasks.sh       # 任务集自检（不花 API 费用）
└── results/              # <git-sha>-<ts>[-label].jsonl
```

`check.sh` 可用两个环境变量：`$WYJ_STDOUT`（模型 stdout 文件）、`$WYJ_FIXTURE`
（原始 fixture 路径）。

## 已知限制

- **会花真实 API 费用**。20 个任务 × 3 次重复 ≈ 60 次完整会话。
- **任务都在一个小 fixture 上**。mathlib 只有 5 个文件，测不出「大仓库里的定位成本」——
  那一类需要真实仓库快照，本版没有。
- **T3/T5 的判定是关键词级的**，不是语义级。它能抓住「漏了某个文件」，抓不住「说了但说错了」。
  需要语义判定时人工复核 stdout。
- **没有跨模型基线**。当前只有本机配置的那一个端点；多模型对照要手工换 profile 各跑一遍。
- **`setup.sh` 不能用 `sed -i`**：BSD sed 要求 `sed -i ''`、GNU sed 要求 `sed -i`，
  两种方式在对方平台都会失败。一律走 python3（`compare.sh` 已经依赖它）。

## 基础设施失败 ≠ 模型失败

端点 503 / 限流 / 网络中断时，`-p` 会在**一个 token 都没花**的情况下退出。把这种记录算成
`success=false` 会直接污染基线——看起来像「模型做不出来」，实际是「请求根本没发出去」。
一次上游抖动就能让整批对比失真。

`run.sh` 用 `api_calls == 0 && rc != 0` 判据把这类记录标成 `status=infra_error`，
`compare.sh` 在聚合时**直接排除**它们（不算通过也不算失败），并单独列出 `infra_errors` 计数。

用 `api_calls` 而不是匹配 stderr 文本：错误文案会随端点变化，而「一次请求都没成功」
是结构性事实。这个判据是踩过的——第一次跑批时上游恰好 503，09–20 十二个任务全部
`api_calls=0` 却记成了失败，看起来像「模型从第 9 个任务开始突然崩了」。

**排查口诀**：对比结果里出现成片的 `success_rate=0%` 且 `api_calls=0`，先查端点，
不要急着改 harness。
