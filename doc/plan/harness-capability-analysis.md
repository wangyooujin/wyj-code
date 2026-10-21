# Harness 设计与模型能力发挥：外部实证调研 × wyj-code 差距分析

> **报告日期**：2026-10-21
> **数据时效**：2022–2026，网络资料覆盖至 2026-10
> **维护责任**：wyj-code 维护组
> **本报告不包含实测 benchmark**。所有外部结论均来自公开论文与官方博客（附 URL），所有对 wyj-code 的判断均来自代码层事实复核（附 `file:line`）。标注「未核实」的条目表示未找到一手可靠来源。
> **本报告不改动任何代码**。第 7 章的路线图是待实施建议，第 8 章是评测集规格，均未落地。

---

## 0. 摘要（TL;DR）

### 0.1 七条核心结论

**1. harness 是与模型权重正交的独立能力变量，而且常常是更大的那个。** 业内已形成共识公式 `agent = model + harness`（Wikipedia《Agent harness》词条，AI Security Institute 2023）。最强的定量证据来自 arXiv:2605.23950 的受控 3 模型 × 3 harness 实验：**harness 方差 / 模型方差 = 7.80×**，且 9 组模型配对中出现 **6 次排名反转**——也就是说，「换 harness」足以翻转「哪个模型更强」的结论。**证据强度：强**。

**2. 但这不等于「harness 决定一切」。** 同一篇实验显示跨模型方差在某个 harness 下被压到 1.17pp²、另一个下升到 4.22pp²——更好的 harness 既可能吸收模型差异，也可能暴露模型差异。反方向也有硬边界：METR 的 50% time horizon 从 GPT-4 的 ~4 分钟增长到 Claude Opus 4.6 的 ~719 分钟（50–100×），这部分是**模型绑定的**，没有任何 scaffold 能把 7 分钟 horizon 的模型变成小时级 agent。**证据强度：强**。

**3. harness 的收益高度非线性且非单调——「加结构」经常不如「不加」。** 同一批 NLAH 论文的模块消融里，verifier 在 SWE 上是 **−0.8pp**、在 OSWorld 上是 **0.0pp**；多候选搜索在 OSWorld 上是 **−5.6pp**；HAL 的 21,730 次 rollout 显示**提高 reasoning effort 在多数运行中降低准确率**。**证据强度：强**。

**4. 对 wyj-code 的判断：harness 底子高于多数自研 agent，但缺两块承重墙。** 已经做对的：system prompt 的 stable/volatile 两段拆分（`crates/api/src/provider.rs:28-59`）、lazy 工具折叠 + ToolSearch 检索式暴露（`crates/core/src/agent.rs:1073-1095`）、清理优先于摘要 + CAS 外部化（`crates/core/src/context_edit.rs:56-65`）、编辑后自动校验回路（`crates/core/src/verify.rs`）、prompt 指纹（`crates/core/src/prompt_fingerprint`）。**没做的两件事**：① 没有「模型宣布完成 → 框架独立验收」的闭合回路；② 没有任何**任务成功率**口径的评测基线，只有行为统计（轮数、调用分布）。**证据强度：中**（前半为代码事实，后半为外部证据映射）。

**5. 「让模型自己评自己的活」是不可信的验收方式。** Anthropic 的原话是：让 agent 评自己的活，它会「自信地夸赞明显平庸的作品」；把干活和评判分开是强杠杆，但「调出一个怀疑型 evaluator 比让 generator 批判自己容易得多」。同时要注意反例：**若局部验收对象与最终验收对象不对齐，多加 verifier 反而降分**。**证据强度：中**。

**6. 工具的「数量」与「描述质量」是两个独立的杠杆，且都有定量证据。** Anthropic 内部有一个 tool-testing agent：给它一个有缺陷的 MCP 工具，它反复试用并**重写工具描述**以规避失败——改进后的描述让后续 agent 的**任务完成时间下降 40%**。另一边，arXiv:2605.24660 用 chance-corrected 指标覆盖 20–3,251 个工具的注册表，结论是「**在工具选择上，更多的候选会主动损害准确率**」，自适应展示 ~7 个工具即可覆盖 90.3%。**证据强度：强（描述）/ 中（数量）**。

**7. 元原则（也是本报告最有行动价值的一条）：每个 harness 组件都编码了一个关于「模型做不到什么」的假设。** Anthropic 原文：*"every component in a harness encodes an assumption about what the model can't do on its own, and those assumptions are worth stress testing, both because they may be incorrect, and because they can quickly go stale as models improve."* 他们自己的实践是：Opus 4.6 发布后**移除 sprint 分解**，成本从 $200 降到 $124.70 / 3h50m。**证据强度：中**。

### 0.2 证据强度总表

| 强度 | 判定标准 | 本文中的代表结论 |
|---|---|---|
| **强** | 受控实验 / 受控方差分解 | harness 方差 7.8× 模型方差（§3.1）；工具选择中更多候选主动有害（§3.4b）；无 oracle 时自我反思降分（§4.3）；summary 压缩在中等预算不优于 FIFO（§3.4c） |
| **中** | 有具体数字的观察性数据 / 多方独立复现 | 同模型换 harness 的 10–48pp 分数差（§3.2）；工具描述重写 → 完成时间 −40%（§3.4a）；多 agent +90.2%（§3.4d）；Anthropic 长程 harness 案例（§3.3） |
| **弱** | 从业者主张 / 博客 / 单案例 | 「工具超过 10–15 个就掉点」这类具体阈值（**未找到一手支撑**，见 §9） |

> **⚠️ 后续更新（2026-10-21）**：§7.2–§7.4 记录了两项 P0 的落地与**首轮实测**。
> 最重要的实测结论是：**在当前 20 个任务的基线（20/20 全过）上，验收门禁一次都没触发，
> 收益为零**——它把 G1 从「已证实」降级为「在饱和任务集上不成立，需先提高难度再测」。
> 读 §6 的缺口清单时请配合 §7.4 一起看。

### 0.3 本文对 wyj-code 的 6 个缺口（一行版）

| | 缺口 | 主证据 | 严重度 |
|---|---|---|---|
| G1 | 无闭合验证回路（模型说「做完了」无人验） | §3.1 H2→H3 增益来源；§3.3 自评不可信 | **高** |
| G2 | 无 harness 评测基线（无法回答「这次改动让模型更强还是更弱」） | §3.3 AHE +7.3pp 可迁移；§2.4 「被评测的是配置不是模型」 | **高** |
| G3 | 长程状态靠压缩硬扛（无跨 context window 的文件化进度载体） | §3.3「compaction 并不足够」 | 中高 |
| G4 | 常驻工具 20 个偏多 + 工具描述是超长单行 | §3.4a / §3.4b | 中 |
| G5 | 「默认委派子 agent」对编码类任务可能是负收益；且无 harness 组件台账 | §3.4d 多 agent 的负面条件；§4.4 元原则 | 中 |
| G6 | 无「完成门禁」，也无 oracle 感知的采样分配 | §3.3 硬阈值 criterion；§4.1 弱验证下加宽候选无用 | 中 |

---

## 1. 研究方法与资料来源

### 1.1 网络调研

围绕四个问题检索并**逐条抓取一手页面核实**（未抓到的在 §9 单独列出，不编造）：

1. 「harness」在 LLM agent 语境下的正式定义与边界；
2. 同模型不同 harness 的分数差异实证（含受控方差分解）；
3. 组件级证据：工具描述、工具数量、上下文压缩、子 agent、验证回路、test-time compute 分配；
4. **反面证据**：哪些 harness 设计反而降低模型能力。

主要一手来源：

- 论文：arXiv:2605.23950（harness 方差分解）、arXiv:2603.25723（harness 三元形式化）、arXiv:2609.07785（评测对象是「配置」而非「模型」）、arXiv:2605.24660（工具数量）、arXiv:2608.06503（上下文压缩的执行不稳定性）、arXiv:2609.39982（Mid-Harness，动作级验证）、arXiv:2510.11977（HAL，reasoning effort 非单调）、arXiv:2604.25850（AHE，自动演化 harness）、arXiv:2405.15793（SWE-agent）
- 官方工程博客：Anthropic《Effective harnesses for long-running agents》（2025-11-26）、《Harness design for long-running application development》（2026-03-24）、《Building a multi-agent research system》（2025-06-13）
- 数据集与基准：METR time horizons、Terminal-Bench 2、SWE-bench Verified / Pro
- 综述性来源：Wikipedia《Agent harness》、LangChain（"If you're not the model, you're the harness"）、OpenAI《Harness engineering》（仅摘要，未抓取正文）

### 1.2 代码层调研（wyj-code）

只读盘点，未修改任何文件。覆盖：

- `crates/core/src/prompts.rs`（系统提示词全部常量与渲染函数）
- `crates/core/src/agent.rs`（主循环、工具执行、并行分组、错误回灌、`ALWAYS_VISIBLE_TOOL_SCHEMAS`）
- `crates/core/src/compact.rs` / `crates/core/src/context_edit.rs`（压缩与清理）
- `crates/core/src/auto_mode.rs` / `auto_rules.rs` / `permission.rs`（确定性规则 vs 模型判断的边界）
- `crates/core/src/verify.rs`（编辑后自动校验回路）
- `crates/tools/src/`（工具实现、输出截断阈值、`descriptions.rs` 工具描述）
- `crates/cli/src/assembly.rs`（工具装配、lazy 折叠接线）
- `crates/api/src/provider.rs`（stable/volatile 两段契约）

**方法学提醒**：Rust 的 `dead_code` lint 对 `pub` 项不生效，所以「clippy 0 warning」不等于无死代码；本报告不依赖 lint 结论，全部结论以源码阅读为准。

### 1.3 局限

1. 本文**没有任何本地实测**：所有「wyj-code 会怎样」的判断都是「外部证据 + 代码事实」的映射，属推断而非测量。这正是第 8 章要建评测集的原因。
2. 外部数据来自不同 harness、不同模型、不同基准，跨来源相加需谨慎；本文按来源分别陈述，不做算术合并。
3. METR 的 time horizon 数据里每个模型用的 scaffold 不同（如 Opus 4.5 用 `flock-public`、Opus 4.6 用 `inspect`、GPT-5.3 Codex 用 `triframe`），**这些 p50 数字之间的差异不能干净地读作纯模型差异**。METR 官方也未公开「同模型换 scaffold 后 time horizon 变化多少」的量化。

---

## 2. 概念框架：harness 是什么，边界在哪

### 2.1 三种定义

| 来源 | 定义要点 |
|---|---|
| Wikipedia《Agent harness》（2026-07 建词） | "An agent harness, also known as **agent scaffolding**, is the software infrastructure surrounding a large language model (LLM) that enables it to operate as an AI agent. It manages tool use, memory, state persistence, execution environments and feedback loops, **as opposed to the model's internal reasoning**." 明确 `agent = model + harness` |
| AI Security Institute（2023） | "The resultant system, built out of the AI model and the scaffold, is an AI agent." |
| Anthropic | 称 Claude Agent SDK 为 "the agent harness that powers Claude Code"；OpenAI 2026-02 发了《Harness engineering》，把 harness 工程作为独立职能 |

### 2.2 三元形式化（arXiv:2603.25723）

目前边界最清晰的一版，把 harness 形式化为三件事：

- **control** —— 工作如何被分解与调度
- **contracts** —— 必须产出什么产物、什么门禁必须满足、何时停止
- **state** —— 什么必须跨步骤 / 跨分支 / 被委派的 worker 持续存在

原文对三层边界的表述：*"By **context engineering** we mean designing the immediate prompt and retrieved context for a single call; **a harness subsumes this**, but also manages multi-step structure, tool mediation, verification, and durable state."*

**这条形式化对 wyj-code 的意义**：「context engineering」在 wyj-code 里已经是一等公民（`core::prompts`、`core::compact`、`core::context_edit`），但 **contracts（验收门禁）** 与 **state（跨步骤持久状态）** 两块基本是空白——这正是 §6 的 G1 与 G3。

### 2.3 可枚举的七层（arXiv:2605.23950 的披露分类）

| 层 | 含义 | wyj-code 落点 |
|---|---|---|
| **E**xecution | 运行时底座、沙箱、步数/任务预算 | `crates/tools/src/bash.rs` 沙箱、`agent.rs` 轮数上限 |
| **T**ool | 工具清单、schema、错误契约 | `crates/tools/src/registry.rs`、`descriptions.rs`、`tool_search.rs` |
| **C**ontext | 窗口上限、压缩与检索策略、持久记忆 | `core/compact.rs`、`core/context_edit.rs`、`core/memory_v3.rs` |
| **S**cheduling | agent loop、重试与升级规则 | `core/agent.rs` 主循环、`MAX_STREAM_RETRIES` |
| **O**bservability | 日志产物与 trace | `crates/tools/src/trace.rs` |
| **V**erification | 校验、自检、异常检测 | `core/verify.rs`（**仅在项目显式声明时存在**） |
| **G**overnance | 权限模型、副作用边界、人工审批点 | `core/permission.rs`、`core/auto_mode.rs` |

**这张表本身就是 wyj-code 的 harness 自检清单**：七层都有落点，但 **Verification 一层的覆盖是条件性的**（`verify.rs:23` 的零假设原则——未声明 `verify.command` 就完全不跑）。

### 2.4 方法论后果：被评测的从来不是「模型」

arXiv:2609.07785 的界定更严格：

> "The evaluated unit is an **agent configuration** a: base model, scaffold, tools, action space, prompt policy, stopping rule, verifier, and resource budget. A model-only claim requires extra design support, such as multiple scaffolds per model and multiple models per scaffold. Otherwise, the score describes the evaluated configuration."

推论：**只跑一个 scaffold 的排行榜，其分数只能归因到「配置」**。这也是 §6 G2 的论证基础——wyj-code 现在所有关于「模型 X 在 wyj-code 里表现如何」的说法，严格说都是「模型 X + 当前 harness」的表现，没有一个分离后的模型基线。

### 2.5 内层 / 外层 harness

Thoughtworks（Böckeler）区分：

- **inner harness**：模型厂商出厂的（Agent SDK、Cursor、Codex、Claude Code）
- **outer harness**：用户自建的（instruction 文件、MCP server、自定义 skills）

并把外部构件分为 **guides**（行动前引导）与 **sensors**（观察结果让 agent 自我纠偏），每类再分 **computational**（lint/test 这类确定性检查）与 **inferential**（LLM-as-judge 这类语义检查）。

> 来源：经 Wikipedia《Agent harness》词条引用 martinfowler.com（2026-04-02）。**本文未直接抓取该原文，标注为间接来源。**

**对 wyj-code 的映射非常直接**：
- `CLAUDE.md` 规约链 = **guides**（computational）
- `core/verify.rs` = **sensors**（computational）——**但需要项目显式声明**
- `core/auto_mode.rs` 的分类器 = **sensors**（inferential）
- `core/prompts.rs` 的 MAIN = **guides**（inferential）
- `crates/cli/src/review_cmd.rs` = 确定性 diff 扫描器，**不调用 LLM**（`ReviewFinding` 全部由规则产生）

---

## 3. 实证证据：harness 确实影响模型能力上限

### 3.1 最强的一条：受控方差分解

**实验设计**（arXiv:2605.23950）：SWE-bench Verified 难度分层 100 题子集；3 个前沿模型（GPT-5.4、Kimi K2.6、GLM-5.1，能力刻意选得接近）× 3 个 harness 配置（H1 最小化 / H2 改进 / H3 完整）；每格 2 次独立运行，固定任务顺序、Docker 环境、50 步预算、120s 超时。

| | H1 (Minimal) | H2 (Improved) | H3 (Full) | HV(模型) |
|---|---|---|---|---|
| GLM-5.1 | 52.5 | 56.5 | **65.5** | 29.56 |
| GPT-5.4 | 55.0 | 58.5 | **63.5** | 12.17 |
| Kimi K2.6 | 52.0 | 59.0 | **60.5** | 13.72 |
| **MV(harness)** | 1.72 | 1.17 | 4.22 | |

- 平均 **HV = 18.48 pp²**，平均 **MV = 2.37 pp²**，比值 **7.80×**（第一次运行 8.72×，第二次 6.76×，非平均化伪影）
- 换 harness 让 GLM-5.1 动了 **13.0pp**，另两个各 8.5pp；换模型（harness 固定）只动 3.0 / 2.5 / 5.0pp
- **9 组模型配对中出现 6 次排名反转**
- **机制分析**（读 trajectory log）：H1→H2 的增益来自**降低控制噪声**（严格 JSON 动作格式、精简工具 schema、压缩近期历史、结构化错误反馈），**不是**模型知识变多；H2→H3 的增益来自**闭合验证与恢复回路**
- **成本反直觉**：H2 反而比 H1 便宜（GPT-5.4：$34.19 vs $22.19），因为工具 schema 精简减少了往返

该文还提出一个对 wyj-code 直接可用的框架：把 agent 建模为**闭环控制系统**，harness 是控制器 C_H，LLM 是被控的随机策略 π_θ。据此定义三个 harness 侧可靠性量：

- **Stability**（进展是否单调）
- **Context Drift**（任务信息离开上下文的速率）
- **Control Lag**（检测到异常到纠正信号抵达策略的步数）

三者都不是模型属性——「更强模型能降低异常发生率，但异常发生后的响应是 C_H 的属性，模型升级不会改善它」。

### 3.2 同模型换 harness 的公开分数差

| 基准 | 固定的模型 | harness 变化 | Δ | 强度 |
|---|---|---|---|---|
| SWE-bench Pro | Claude Opus 4.5 | SEAL → Claude Code | **+9.5pp**（45.9% → 55.4%） | 中 |
| SWE-bench Pro | 六个前沿模型 | 固定 SEAL | 模型间仅 **4.9pp**（41.0–45.9%），**小于**单模型换 harness 的 9.5pp | 中 |
| SWE-bench Pro | MiniMax 2.5 vs Claude Opus 4.6 | **+一个** WarpGrep 搜索子 agent | +2.1–2.2pp，**足以翻转两者排名** | 中 |
| SWE-bench Verified Mini (HAL) | Claude Sonnet 4.5 | SWE-agent ↔ HAL Generalist | **34pp**（68% ↔ 34%） | 中 |
| SWE-bench Verified Mini (HAL) | GPT-5 Medium | 同上 | **34pp**（46% ↔ 12%） | 中 |
| SWE-bench Verified Mini (HAL) | o4-mini | 同上 | **近 48pp** | 中 |
| SWE-bench Verified | GPT-5 / Kimi K2 Thinking | 仅 scaffold 变化 | **11pp / 15pp** | 中 |
| SWE-bench Verified | Grok 4 | SWE-agent → xAI scaffold | **+14–16pp**（58.6% → 72–75%） | 中 |
| TerminalBench 2.0 | 固定模型 | prompt + middleware + verification | **+13.7pp**（52.8% → 66.5%） | 中 |
| SWE-bench Verified | 固定前沿模型 | mini-swe-agent（**100 行**纯 bash ReAct 循环） | **>74%** | 中 |
| SWE-bench（原始） | 同一 GPT-4 级模型 | RAG baseline → SWE-agent → Agentless | **~2% → 12.47% → 33.2%（约 15×）**；其中**接口设计消融单项值 10.7pp** | 中 |
| Agent 任务（Vercel） | 固定模型 | **15 个工具 → 2 个工具** | **80% → 100%** | 中 |
| 编码基准 | 固定模型 | context 格式 + 工具定义 | **~10×** | 中 |

**值得单独记一笔的两条**：

1. **mini-swe-agent 100 行 bash 循环拿到 SWE-bench Verified >74%**——在 frontier 上，「更轻的 harness」可能优于「更重的 harness」。这与 Anthropic 砍掉 sprint 分解是同一个方向（§4.4）。
2. **「接口设计消融单项值 10.7pp」**——SWE-agent 论文里，工具接口的设计质量单独贡献了总提升的绝大部分。这与 §3.4a 的「工具描述质量 → 完成时间 −40%」互相印证：**工具描述是被严重低估的杠杆**。

### 3.3 长程任务：模型绑定的部分与 harness 能补偿的部分

#### METR time horizon（模型绑定的部分）

- 50% time horizon 自 2019 起约每 **7 个月**翻倍（doubling time point estimate 201.15 天）
- TH1.1 数据（同 scaffold 家族内）：GPT-4 p50 ≈ 3.99 分钟 → Claude 3.7 Sonnet 60.4 分钟 → Claude Opus 4.5 293 分钟 → **Claude Opus 4.6 718.8 分钟（≈12 小时）**；GPT-5.1 Codex Max 223.7 分钟
- METR 在 2025 论文里把趋势归因于 "logical reasoning, tool use, reliability, and self-awareness" 的改进——注意这是**混合归因**

⚠️ 读法提醒：这些 p50 之所以能跨代比较，是因为 METR 对每个模型基本固定了 scaffold。**这恰恰说明 time horizon 测的是 model+scaffold 合体**，不能当纯模型指标读。METR 官方也标注静态图表反映的是 2025 年发布时的数据状态。

#### Anthropic《Effective harnesses for long-running agents》（2025-11-26）

- 问题：跨多个 context window 保持进展。**「compaction 并不足够」**——Opus 4.5 在 Claude Agent SDK 上循环跑，只给 "build a clone of claude.ai" 这种高层 prompt，仍然做不出生产级 web app
- 两个失败模式：① 试图一次做完 → 中途 context 耗尽，下一 session 接手半成品；② 项目后期 agent 看到已有进展就**宣布完成**
- 解法：**initializer agent**（写 `init.sh`、`claude-progress.txt`、初始 git commit、200+ 条功能清单 JSON）+ **coding agent**（每次 session 只做一个 feature、结束前提交 git + 写进度）
- 迭代细节：用 JSON 而非 Markdown 存功能清单，「因为模型不太会不当修改或覆盖 JSON 文件」（弱证据，但很具体）
- **关键观测**：Claude 会改代码、会写单测、会 curl 测试，但**意识不到功能没有端到端跑通**；给它浏览器自动化工具（Puppeteer MCP）做人类式测试后性能**显著提升**
- 文章自己留的开放问题：「单个通用 coding agent 跨 context 是否最优，还是多 agent 架构更好——尚不清楚」

#### Anthropic《Harness design for long-running application development》（2026-03-24）

目前最直白的「harness 决定上限」案例：

- **单 agent vs 三 agent（planner / generator / evaluator）**（同一句 prompt，Opus 4.5）：Solo **20 分钟 / $9**；Full harness **6 小时 / $200**（>20× 成本）。作者原话："harness 贵了 20 多倍，但输出质量的差异一眼可见"。Solo 产出的游戏**核心功能根本不响应输入**；harness 版能真正玩
- 进化路线：Opus 4.6 发布后**移除 sprint 分解**（4.5 时代的必要分解，4.6 不再需要），改到每次 run 末尾单次 QA，成本从 $200 降到 **$124.70 / 3h50m**，builder 连续跑 2h07m 不需要分解
- **「context anxiety」**（Sonnet 4.5 特有）：模型接近它以为的 context 上限时**提前收尾**。compaction 保留连续性但不给干净起点，**context anxiety 仍在**；需要 **context reset**（清空上下文 + 结构化交接）。Opus 4.5 大体消除了该行为，因此 V2 harness 完全弃用 context reset
- **自我评估的失败**：让 agent 评自己的活，它会「自信地夸赞明显平庸的作品」。**把干活和评判的 agent 分开**是强杠杆——但分离本身不消除偏袒，「调出一个怀疑型 evaluator 比让 generator 批判自己容易得多」
- 实操细节：evaluator 拿到 Playwright MCP 亲自点页面打分；每条 criterion 有**硬阈值**，任一条不达标该 sprint 直接 FAIL

#### AHE：harness 演化的自动化方向（arXiv:2604.25850）

- Terminal-Bench 2 上，GPT-5.4 从最小 seed 出发演化 10 轮：69.7% → **77.0%**，超过人工设计的 Codex-CLI（71.9%）
- 冻结后**零演化**迁移到另 3 个模型家族：+5.1 ~ +10.1pp
- 迁移到 SWE-bench-Verified：最高总成功率，且比 seed **少用 12% token**
- 关键机制：每次编辑带「自我声明的预测」，下轮用**任务级结果**验证
- ⚠️ 单次 campaign，**不是可重复性证据**

### 3.4 组件级证据

#### (a) 工具描述质量 —— 强证据

Anthropic 多 agent 研究系统里做了一个 **tool-testing agent**：给它一个有缺陷的 MCP 工具，它反复试用并**重写工具描述**以规避失败。据此改进后的描述，让后续 agent 的**任务完成时间下降 40%**。（https://www.anthropic.com/engineering/built-multi-agent-research-system）

#### (b) 工具数量 —— 中证据

arXiv:2605.24660（*How Many Tools Should an LLM Agent See? A Chance-Corrected Answer*），用 chance-corrected 的 Bits-over-Random 指标覆盖 20–3,251 个工具：

- BFCL（370 工具）：学到的自适应策略平均只展示 **7 个**工具，覆盖率 90.3%，接近固定展示 50 个的 90.8%
- **下游验证（Claude Sonnet 4.6）**：更短的自适应列表使模型选对工具的能力从「恒定展示 5 个」的 **87.1% 提升到 93.1%**；在「正确工具存在但未排第一」的中等难度题上差距更大：**60.9% → 76.8%**
- ToolBench（3,251 工具）：固定 5 个的总体覆盖率更高（64.7% vs 61.9%），但**在难查询上完全找不到**（正确工具排第 6–20 名），靠更深搜索的 agent 在这些题上拿到 16.7%
- 原文："In tool selection, **extra candidates can actively hurt LLM accuracy**."

⚠️ 注意这条证据的适用边界：它研究的是**从大注册表里检索工具**。wyj-code 的 MCP 工具正是这个形态（ToolSearch 检索式暴露），**已经踩在正确的位置上**；但它 20 个「恒定可见」的内置工具是另一回事——那是「固定展示 20 个」，正好落在「恒定展示 5 个 → 87.1%」的反面。

#### (c) 上下文压缩 —— 强证据（双向）

arXiv:2608.06503（*Toward Reliable Context Compression for Long-Horizon Agents*）：

- **summary-based compaction 并不一致优于 FIFO 截断**：在 16K / 8K / 4K 预算下 FIFO 持平或略优；差异只在最紧预算出现（summary **72.8%** vs FIFO **42.2%**）
- 压缩会**削弱近期交互的影响**，增加 blocked actions、重复探索、跨运行不稳定性
- 作者称之为 "execution instability"

配套：ACON / *Optimizing Context Compression for Long-horizon LLM Agents*（arXiv:2510.00615）；*Lost in Compaction: Measuring Information Loss in LLM Context Summaries*。

#### (d) 子 agent / 多 agent 划分

Anthropic 多 agent 研究系统（2025-06-13）：

- **Opus 4 lead + Sonnet 4 subagents，在内部 research eval 上比单 agent Opus 4 高出 90.2%**
- BrowseComp 上三个因素解释了 **95%** 的表现方差，其中**token 用量单独解释 80%**；另两个是工具调用数与模型选择
- 成本代价：agent 大约用 **4×** 于聊天的 token，多 agent 约 **15×**；并行化（lead 起 3–5 个 subagent + subagent 3+ 工具并行）**把复杂查询的研究时间最多砍 90%**
- **明确的反例条件**：需要所有 agent 共享同一上下文、或 agent 间强依赖的领域**不适合**多 agent；「大多数编码任务可并行的部分比研究任务少」；「LLM agent 目前不擅长实时协调与委派」

Anthropic 长程应用（2026-03）补充：planner 去掉后 generator 会「欠定范围」——planner 仍有价值。

#### (e) 动作级验证 —— 强证据（Mid-Harness）

arXiv:2609.39982：

- TMAX-9B 生成器 + TerminalBench-Lite：base agent Pass@1 **50.00%** → 用 GPT-5.6 Sol 作 verifier、N=8 → **68.03%**（N=4 为 64.63%）
- **但弱验证下加宽候选几乎无用**：zero-shot listwise，N=4→8 只从 49.32% → 51.02%
- **pairwise 验证机制最好**：54.76% Pass@1 / 71.43% Pass@3；把 GPT-5.6 Sol 的 pairwise 响应蒸馏进 9B verifier → **57.14% / 75.51%**

**这条是「先建 verifier，再建 sampler」的定量依据**：多候选采样只在有真 oracle 的地方有回报。

---

## 4. 反面证据：harness 也会降低模型能力

这一章与第 3 章同等重要。全章结论来自 Anthropic 2026-03、NLAH 论文与 HAL。

### 4.1 负收益模块

| 模块 | 负收益场景 | Δ |
|---|---|---|
| verifier | SWE 上不涨反跌 | **−0.8pp** |
| verifier | OSWorld 上完全无效 | **0.0pp** |
| 多候选搜索 | OSWorld | **−5.6pp** |

**解读**：多加 verifier 并非总是好事。**若局部验收对象与最终验收对象不对齐，多加 verifier 反而降分**。

### 4.2 提高 reasoning effort 不是单调增益

HAL 的 21,730 次 rollout 显示：**提高 reasoning effort 在多数运行中降低准确率**。（arXiv:2510.11977 / https://hal.cs.princeton.edu）

### 4.3 没有 oracle 就没有自我反思的收益

- 无外部 ground truth 的内在自我纠错会降低推理质量（GSM8K 95.5 → 91.5 → 89.0）
- 有 oracle 时 self-evolution 才可能 +4.8pp
- **验证质量本身是模型绑定的**（arXiv:2310.01798 / 2609.39982 / 2603.25723）

### 4.4 元原则：组件 = 对「模型做不到什么」的假设

Anthropic 2026-03 原文：

> "every component in a harness encodes an assumption about what the model can't do on its own, and those assumptions are worth stress testing, both because they may be incorrect, and because they can quickly go stale as models improve."

配套的三条实践教训：

1. **harness 的收益会随模型变强而压缩**——在 frontier 上，100 行循环可能优于重型 harness
2. **harness 组件要「一次只改一个」地消融**——Anthropic 的粗剪枝（直接砍掉 sprint 构造）**没能复现**原性能，因为不知道哪些组件承重
3. **每次新模型发布后重审 harness**：剥掉不再 load-bearing 的组件，再补新的

### 4.5 更多结构 ≠ 更好

arXiv:2603.25723 RQ2 的判据可以直接当 wyj-code 的组件准入标准：**每加一个 harness 模块，先问它收紧的是「中间行为 → 验收门」的路径，还是只加了一层本地流程层。** dynamic orchestration / multi-candidate search 在 OSWorld 上是负收益。

### 4.6 完整的设计原则清单

从 §3–§4 提炼，每条注明证据强度。**这张表是第 7 章路线图的理论依据**。

| # | 原则 | 强度 |
|---|---|---|
| 1 | 把 harness 当独立工程对象：有自己的台账、评测与消融 | 中 |
| 2 | 把确定性留给代码，把判断留给模型（结构化 JSON 输出、严格 schema 能降低控制噪声） | 强 |
| 3 | 先建 verifier，再建 sampler；没有 oracle 不要做 best-of-N | 强 |
| 4 | 验收必须独立于产出（generator / evaluator 分离），并给硬阈值 | 中 |
| 5 | 工具描述是被低估的杠杆（质量 → 完成时间 −40%） | 强 |
| 6 | 工具数量要克制：更多候选会主动损害准确率 | 中 |
| 7 | 自评不可信；分离 generator/evaluator 是强杠杆，但要对齐验收对象 | 中 |
| 8 | 没有 oracle 就没有自我反思的收益 | 强 |
| 9 | 不要把「更多结构」当「更好」 | 强 |
| 10 | 上下文卫生 + 压缩要配验证；summary 压缩在中预算下不优于 FIFO | 强 |
| 11 | 长程任务必须有持久化的、可寻址的状态载体 | 中–强 |
| 12 | 多 agent 只在「可真正并行 + 信息量超单上下文 + 需与众多工具交互」时用 | 中 |
| 13 | 并行化是最高杠杆之一（复杂查询时间最多降 90%） | 中 |
| 14 | 委派必须给足四要素：objective / 输出格式 / 工具与来源指引 / 任务边界 | 中 |
| 15 | 子 agent 产出写文件（artifact），只回传轻量引用 | 中 |
| 16 | 不要用「加基准」消解 harness 噪声——harness 贡献非零均值 | 中 |
| 17 | harness 组件要一次只改一个地消融 | 中 |
| 18 | 每次新模型发布后重审 harness，允许删除失效组件 | 中 |
| 19 | harness 演化可以自动化（AHE：+7.3pp，可跨模型迁移） | 强 |
| 20 | 在模型-动作边界分配 test-time compute 有效，但强依赖验证器质量 | 强 |
| 21 | 「增加思考/推理预算」不是单调增益 | 中 |
| 22 | 更好的 harness 既可能吸收模型差异，也可能暴露它 | 强 |
| 23 | 长程能力的「模型绑定」部分无法用 scaffold 补 | 中 |
| 24 | 优先建 verifier，再建 sampler | 中 |
| 25 | 端到端评估优于逐轮评估（离散 checkpoint，而非验证过程步骤） | 中 |
| 26 | 小样本快速迭代：20 个真实 query 就能看出 30%→80% 的 prompt 改动效果 | 中 |

---

## 5. wyj-code harness 现状盘点（代码层事实）

### 5.1 系统提示词

| 项 | 位置 | 事实 |
|---|---|---|
| `MAIN` 主静态提示词 | `crates/core/src/prompts.rs:8-64` | **57 行**，10 个小节：Tone and style / Proactiveness / Following conventions / Asking the user / Skills / Tool usage policy / Context management / Memory / Doing tasks |
| `<env>` 环境块 | `prompts.rs:198-209` | 会话内恒定字段（cwd/home/git/platform/today/model/language） |
| `SKILL_CATALOG_PLACEHOLDER` | `prompts.rs:139` | 单条 skill 描述截断上限 240 字符（`:158`） |
| `COMPUTER_USE_HINT` | `prompts.rs:373` | **单行约 16KB 英文**，computer-use 全套工作流约定 |
| `AUTO_CLASSIFIER` | `prompts.rs:290-313` | Auto 模式分类器 system prompt，输出裸 JSON |
| `SUBAGENT_GENERAL/EXPLORE/PLAN` | `prompts.rs:423 / 425 / 427` | 三个子 agent 模板，各自单行长文本 |
| `COMPACT_SYSTEM` + `compact_instructions()` | `prompts.rs:436 / 447-477` | 摘要用，固定 8 小节 |
| `current_tool_availability_block()` | `prompts.rs:624-645` | 每轮列出本次可直接调用的工具 + 被折叠来源 |

**stable / volatile 两段契约**在 `crates/api/src/provider.rs:28-59`：`SystemPrompt { stable, volatile }`，`combined()` 在 `:47-54`，辅助 LLM 调用统一走 `stable_only()` `:38-43`。

- **stable 段** = `MAIN` + `<env>` + 跨会话记忆快照 + 项目规约（CLAUDE.md）祖先链 reminder，装配于 `crates/core/src/agent.rs:1501-1523`
- **volatile 段** = 三段固定顺序：模型兼容 suffix → 工具可用性名单 → 回复语言提醒，拼装于 `agent.rs:1166-1201`

**wyj-code 已对齐的一条重要外部经验**：`crates/core/CLAUDE.md:104` 记录的 stable/volatile 拆分动机，与 Anthropic 的 prompt cache 断点纪律一致；`crates/core/CLAUDE.md:187` 记录的「lazy schema 折叠必须在提示词里点名」，与 arXiv:2605.24660 的「工具必须可被发现」是同一件事。

### 5.2 工具集合与可见性

**注册路径**：
- `ToolRegistry::standard()`（`crates/tools/src/registry.rs:34-54`）：9 个 —— Bash、BashOutput、KillShell、Read、Write、Edit、Glob、Grep、WebFetch
- `build_tool_registry()`（`crates/cli/src/assembly.rs:59-107`）追加：CodeSearch、Memory、ContextRecall、TodoWrite（配置门控）、AskQuestion、WebSearch（后端门控）、computer / window_capture / app_computer（平台+配置门控）、Agent、MCP 工具

**可见性机制**（`crates/core/src/agent.rs:1073-1095`）：
- `enable_lazy_tools(core_tools, threshold, top_k, sticky_turns)`：工具总数 ≤ `threshold`（`crates/config/src/lib.rs:579` 默认 **12**）时不启用折叠
- 启用后，**恒定可见集合** = `core_tools`（`assembly.rs:256` 的 Read/Glob/Grep/AskQuestion/Memory）∪ `ALWAYS_VISIBLE_TOOL_SCHEMAS`（`agent.rs:162-193`）
- `ALWAYS_VISIBLE_TOOL_SCHEMAS` 实际条目 **20 个**：Read、Glob、Grep、CodeSearch、Bash、BashOutput、KillShell、Edit、Write、WebFetch、WebSearch、AskQuestion、Memory、ContextRecall、Agent、ExitPlanMode、TodoWrite、window_capture、app_computer、computer
- 其余（MCP 工具等）进 `LazyToolState`，靠 ToolSearch 命中后**永久暴露**（`agent.rs:1070-1072` 注释：schema 一旦发给模型就不得收回，收回会让 tools 数组中途缩水进而作废整段 prompt cache）

⚠️ **注意这里的实际效果**：因为内置工具几乎必然超过阈值 12，所以**绝大多数会话里模型看到的工具 schema 恒为 20 个 + ToolSearch = 21 个**，MCP 工具折叠。这与 arXiv:2605.24660 所批评的「恒定展示 N 个」是同一形态，只是 N=20 而非 5。

**工具描述**（`crates/tools/src/descriptions.rs`，全部为英文常量，符合「模型侧提示词走英文」的既有约定）：
- `READ` / `WRITE` / `EDIT` / `GLOB` / `GREP` / `WEBFETCH` / `WEBSEARCH` / `TODO_WRITE` / `ASK_QUESTION` / `EXIT_PLAN_MODE`：各为**单行**
- `BASH`（`:12`）：单行，内含沙箱、120s/600s 超时、`run_outside_sandbox` 一次性审批、30KB 截断等约 10 条规程
- `SUB_AGENT_TEMPLATE`（`:29`）：多段长文本
- `COMPUTER_CUSTOM_TEMPLATE`（`:50`）：**超长单行**
- 非集中管理的内联描述：`CodeSearch`（`crates/core/src/code_index.rs:310`）、`ToolSearch`（`crates/core/src/tool_search.rs:339`）

**「模型看不到的东西」清单**（各工具的输出截断阈值）：

| 位置 | 阈值 |
|---|---|
| `crates/tools/src/bash.rs:13` | Bash 输出 head+tail 截断 `MAX_OUTPUT = 30_000` |
| `crates/tools/src/read.rs:12-13` | Read `MAX_LINES = 2000` / `MAX_BYTES = 200_000` |
| `crates/tools/src/glob.rs:12` | Glob `MAX_RESULTS = 1000` |
| `crates/tools/src/webfetch.rs:14` | WebFetch `MAX_CONTENT = 50_000` |
| `crates/tools/src/context_recall.rs:41` | ContextRecall `DEFAULT_MAX_BYTES = 64KB` |
| `crates/tools/src/handoff.rs:222` | 子 agent 结论 `CONCLUSION_MAX_BYTES = 8000` |
| `crates/core/src/verify.rs:53` | verify 输出 `DEFAULT_MAX_OUTPUT_BYTES = 4_000` |
| `crates/tools/src/trace.rs:24-35` | trace 输入 16KB / 输出头 20KB + 尾 10KB |

### 5.3 上下文管理

**双阈值 + 清理优先于摘要**（`crates/core/src/agent.rs:1903-1925`）：
1. 超软阈值 → `elide_stale_tool_results`（外部化清理）
2. 仍超标 → `compact_session_until_fit`（模型摘要）

- 硬阈值 = `effective_window - compact_trigger_buffer(...)`
- `compact_trigger_buffer(cw, max_tokens) = max(max_tokens + 8192, cw/10)`（`crates/core/src/compact.rs:78-82`）
- 清理白名单 `ELIDABLE_TOOLS`（`crates/core/src/context_edit.rs:56-65`）：Read、Grep、Glob、CodeSearch、Bash、WebFetch、WebSearch、Agent + `mcp__` 前缀；**永不清理** Write / Edit / NotebookEdit / BashOutput
- `ElideOptions::default()`（`:90-98`）：`protect_recent = 3`、`batch_size = 10`、`min_bytes = 2000`
- 占位符 `[context-edited]` + `cas://<hash>`，原文进 CAS，模型可用 `ContextRecall` 取回（`context_edit.rs:191-303`）
- 摘要保留预算 `compact_keep_budget(cw) = (cw/8).clamp(8000, 128000)`（`crates/core/src/compact.rs:28-30`）；摘要自身上限 `min(cw/10, 16000)`（`:519`）
- 收敛：`MAX_COMPACT_PASSES = 3`、`MIN_PROGRESS_RATIO = 0.05`（`compact.rs:683-685`）

**这条链在外部证据上的位置**：它同时命中了 §3.4c 的「压缩要配验证」（wyj-code 有 `ContextAudit` 统一口径，方向对）与 §3.3 Anthropic 的「compaction 并不足够」（**缺持久化状态载体**，见 G3）。

### 5.4 主循环与工具并行

- 循环起点 `crates/core/src/agent.rs:1610`；轮数上限默认 **200**，thinking 模型 **32**（`:1585-1590`，判据 `helpers::max_turns_for_model`）
- 流重试 `MAX_STREAM_RETRIES = 2`（`:2039`）
- 并行分组（`:2792-2828`）：`parallel_safe()` 或（`parallel_safe_with_peer()` 且不需要弹窗）；叠加模型能力 `capabilities.parallel_tool_calls.value`；并发组 `join_all`、串行组顺序执行，两组 `tokio::join!` 同时跑，结果按原始下标保序回填
- **错误回灌措辞**（模型实际看到的 JSON）：
  - `{"error":"tool_schema_not_exposed","tool":…,"instruction":"Call ToolSearch for the needed capability, then retry on the next turn."}`（`:2512-2516`）
  - 参数校验失败时**回写模型原始参数**而非占位符（`:2557-2594`，注释解释占位符会让模型连续 5 轮重发同一个错参数）
  - `{"error":"tools_closed_turn_budget_exhausted",…}`（`:2711-2719`）
  - `{"error":"tool_call_incomplete",…,"instruction":"The response ended before this tool call completed. Nothing was executed. Retry it, or proceed without it."}`（`:2942-2950`）
- **「半成品不入历史」**：流失败即丢弃整轮缓冲（`:1698-1701`）；只有 thinking 的截断回复不入历史（`:2637-2639`）；`persist_cap` 截断只改副本不改 `session.messages`（`:2028-2037`，理由是历史是 prompt cache 前缀）

**这条「回写原始参数而非占位符」的设计，是外部证据里「降低控制噪声」的同款做法**（§3.1 H1→H2 的增益机制之一）——wyj-code 在这一点上已经走在正确的路上。

### 5.5 确定性规则 vs 模型判断的边界

`crates/core/src/auto_rules.rs` 分两层（`:1-12`）：

**A. 确定性规则层**（本地正则/前缀，零延迟、可单测）：
- `HARD_DENY_PATTERNS`（`:138-222`）**12 条**：`pipe_to_shell`、`critical_path_removal`、`force_push`、`git_discard`（6 个 needle）、`commit_amend`、`infra_destroy`、`escalate_privilege`、`disk_destructive`、`credential_paths`、`unapproved_agent_loop`、`shell_history_wipe`
- `ALLOW_READ_ONLY_PREFIXES`（`:229-247`）**17 个**只读命令前缀
- `ALLOW_LOCKFILE_INSTALL`（`:250-258`）8 个 lockfile 安装命令
- 受保护目录 `permission::PROTECTED_DIR_NAMES`（`.ssh`/`.aws`/`.gnupg`/`.kube`/`.docker`）

**B. 分类器规则文本层**（英文自然语言常量，喂给分类器模型）：
- `DEFAULT_HARD_DENY`（`:22-36`）**12 条**
- `DEFAULT_SOFT_DENY`（`:38-59`）**13 条**
- `DEFAULT_ALLOW`（`:61-90`）**15 条**
- `DEFAULT_ENVIRONMENT`（`:92-97`）**5 条**

**结论：这一层不需要动。** 它符合 §4.6 原则 2「把确定性留给代码，把判断留给模型」——12 条 hard_deny 是安全边界（不该交给模型判断），分类器层是「结构化输出 + 自然语言规则」降低控制噪声的典范实现。

### 5.6 长程状态载体

| 载体 | 特性 | 局限 |
|---|---|---|
| 会话文件（`SessionFile`） | 完整 transcript + 落盘 | **会被压缩改写**——压缩后进度信息变成模型自写摘要 |
| TodoWrite | 唯一穿透压缩的显式进度载体 | 每轮重放进上下文；实测 132 次调用 **100% 独占一个 LLM 往返**、**59% 会话未收尾**（`crates/core/CLAUDE.md:116-118`） |
| git | 外部、可寻址、跨 session | 提交粒度 ≠ 任务进度 |
| checkpoint（`CheckpointKind`） | 回合级快照 | 粒度是回合，不是「任务完成了哪些条目」 |

**对照 Anthropic 的做法**（§3.3）：`init.sh` + `claude-progress.txt` + **200+ 条功能清单 JSON** + 每 session 结束前提交 git + 写进度。wyj-code 有 git 与 checkpoint，**缺「可寻址的结构化进度文件」**。

### 5.7 既有自测数据（引自 `crates/core/CLAUDE.md`）

这些是 wyj-code 难得的实证积累，但**都是行为统计，不是任务成功率**：

| 数据 | 位置 | 含义 |
|---|---|---|
| 102 个历史会话中 27 个用到 TodoWrite、132 次调用全部独占一个 LLM 往返、0 次与真实工作工具同轮发出、最密会话累计重复 prefill 5.2 MB、59% 会话结束时 todo 未收尾 | `crates/core/CLAUDE.md:116-118` | 「TodoWrite 是唯一穿透压缩的进度载体」这个假设**部分不成立**：它太贵、且经常没收尾 |
| 285 次调用的会话：收尾指令在第 196 轮**完整送达**（523 字、位置正确），模型接下来的 thinking 里**零次提到轮数**；285 条 assistant 消息里 **282 条只有 1 个 tool_use** | `crates/core/CLAUDE.md:248` | 「软性提醒在模型沉浸态里不产生约束力」；「`turn` 计的是 LLM 往返而不是工作量」 |
| 压缩保留上限 32K → 128K 后，1M 窗口下旧值把 900K 压成约 48K（压缩比 18:1），实测「压缩后模型明显失忆、重新 grep 刚 grep 过的目录」 | `crates/core/src/compact.rs:22-27` | 压缩比本身是有害的 |

**这三条数据是本文最有价值的本地证据**，它们恰好指向 G3（状态载体）与 G6（完成门禁）。

---

## 6. 差距分析：wyj-code 的 6 个具体缺口

> 每条统一格式：**现象（file:line）→ 外部证据 → 根因 → 影响面 → 可回退的改法**

### G1 无闭合验证回路 —— 严重度：高

**现象**
- `crates/core/src/verify.rs:44`：`VERIFY_TRIGGER_TOOLS = ["Edit", "Write", "NotebookEdit", "MultiEdit"]` —— **不含 Bash**
- `crates/core/src/verify.rs:23` 的零假设原则：项目**未声明** `verify.command` 就完全不跑
- 没有任何环节在「模型宣布任务完成」时介入

**外部证据**
- §3.1：H2→H3 的增益来源明确是**闭合验证与恢复回路**——这是三个 harness 配置之间最大的一段差距
- §3.3：Anthropic 观察到的核心失败模式是「Claude **意识不到功能没有端到端跑通**」；给它浏览器自动化工具做人类式测试后性能**显著提升**
- §3.3：「让 agent 评自己的活，它会自信地夸赞明显平庸的作品」；**generator / evaluator 分离**是强杠杆，但「调出一个怀疑型 evaluator 比让 generator 批判自己容易得多」
- §4.1：**verifier 未必是正收益**——若验收对象与最终验收对象不对齐，多加 verifier 反而降分（SWE −0.8pp / OSWorld 0.0pp）

**根因**
wyj-code 把「验证」理解成「编译通过」，而不是「任务达成的判据」。二者不是一回事：`cargo check` 通过 ≠ 功能跑通；模型自述「已完成」≠ 达成。缺的是 **contracts**（§2.2 三元形式化里的第二项）。

**影响面**
- 复杂任务的最高频失败不是「不会写」，而是**不知道写错了**——这正是 `verify.rs:1-10` 自己写下的判断。wyj-code 在**编译类**上补上了这条回路，但在**行为类 / 端到端类 / GUI 类**任务上完全空白。
- `crates/tools/src/computer*` 系列的存在意味着 wyj-code 能做 GUI 任务，但 GUI 任务**没有**任何验收机制（Anthropic 恰恰指出浏览器/真实交互验证是提升最明显的那一环）。

**可回退的改法**（详见 §7）
1. **收尾门禁**：模型宣布完成时，若 TodoWrite 存在未收尾项 → 框架介入（**不是改提示词，是改状态机**）
2. **独立验收者**：新增只读 evaluator 子 agent 类型（与 generator 分离），产出「逐条 criterion 通过/失败 + 硬阈值」
3. **criterion 可配置**：项目在 `.wyj-code/settings.json` 声明验收清单，未声明则完全不启用（延续 `verify.rs` 的零假设风格）

### G2 无 harness 评测基线 —— 严重度：高

**现象**
- 现有的全部量化数据是**行为统计**（轮数、调用分布、token 重复量、todo 收尾率，见 §5.7），**没有一项是任务成功率**
- `crates/cli/src/review_cmd.rs` 是确定性 diff 扫描器（`ReviewFinding` 全部由规则产生，**不调用 LLM**），不能充当模型能力的评测器
- 因此无法回答：「这次 prompt / 工具 / 规则改动让模型更强还是更弱？」

**外部证据**
- §2.4：只跑一个 scaffold 的分数只能归因到「配置」——**没有分离出的模型基线，「wyj-code 让模型更强」这类命题严格说不成立**
- §3.3：AHE 从最小 seed 演化 10 轮，Terminal-Bench 2 上 69.7% → 77.0%，且冻结后可**零演化跨模型家族迁移**（+5.1~+10.1pp）——harness 优化是**可测量、可自动化**的
- §4.6 原则 26：Anthropic 的经验是 **20 个真实 query 就能看出 30%→80% 的 prompt 改动效果**，不要等几百个 case
- §4.4 原则 17：组件要**一次只改一个**地消融——这要求有可重复的评测口径，否则消融结果不可归因

**根因**
wyj-code 的开发节奏是「改一个点 → 跑 `cargo test` → 提交」，`cargo test` 验证的是**代码正确性**，不是**模型能力**。两者被混为一谈。

**影响面**
所有 §7 的改进项都依赖 G2。没有评测集，任何 harness 改动都只能靠感觉判断——而 §4.1 的证据恰恰说明**很多 harness 改动是负收益**。

**可回退的改法**：见 §8 的完整规格（20 个任务的小规模集）。

### G3 长程状态靠压缩硬扛 —— 严重度：中高

**现象**
- §5.6 的四个状态载体里，唯一穿透压缩的 TodoWrite 实测「100% 独占一个 LLM 往返、59% 未收尾」
- 压缩把历史替换成**模型自写摘要**（`crates/core/src/compact.rs:497`），进度信息随之退化为摘要里的一句话
- 保留预算上限 128K 仍是「摘要 + 一小段原文」，`compact.rs:22-27` 记录的 18:1 压缩比实测会让模型「重新 grep 刚 grep 过的目录」

**外部证据**
- §3.3 Anthropic：**「compaction 并不足够」**——Opus 4.5 循环跑只给高层 prompt，仍做不出生产级 web app；解法是 initializer + `claude-progress.txt` + **200+ 条功能清单 JSON** + 每 session 结束前 git commit + 写进度
- §4.6 原则 11：长程任务必须有**持久化的、可寻址的**状态载体（file-backed state 在 OSWorld +5.5pp / SWE +1.6pp）
- §3.4c：summary-based compaction 在中等预算下**不优于 FIFO 截断**，且会削弱近期交互的影响、增加 blocked actions 与重复探索

**根因**
wyj-code 的上下文管理是**「把历史压小」**的思路，而 Anthropic 的思路是**「把状态外置」**。前者是连续量优化，后者是拓扑改变。

**影响面**
长任务（跨十几个文件、几十轮工具、中途被压缩打断）——这恰恰是 wyj-code 自己在 `crates/core/CLAUDE.md:116` 声明要服务的场景。§5.7 的第 3 条实测数据（压缩后模型失忆、重复劳动）就是这一缺口的直接症状。

**可回退的改法**
- 新增**文件化进度载体**（如 `.wyj-code/progress.md` 或 JSON）：模型可写、框架可在压缩时**优先保留**（不依赖摘要）、跨 session 保留
- 格式用 JSON 而非 Markdown（Anthropic 明确观察：「模型不太会不当修改或覆盖 JSON 文件」——弱证据但具体）
- 压缩时把该文件内容**作为独立消息保留**，不进摘要

### G4 常驻工具偏多 + 工具描述质量 —— 严重度：中

**现象**
- 恒定可见 20 个工具 schema（§5.2），与 arXiv:2605.24660 批评的「恒定展示 5 个」是同一形态
- `descriptions.rs` 的 `BASH`（`:12`）、`COMPUTER_CUSTOM_TEMPLATE`（`:50`）是超长单行；`prompts.rs:373` 的 `COMPUTER_USE_HINT` 是约 16KB 单行

**外部证据**
- §3.4b：自适应展示 ~7 个工具覆盖 90.3%；**恒定展示 5 个时选对率 87.1%，自适应列表 93.1%**，中等难度题差距 60.9% → 76.8%；「extra candidates can actively hurt LLM accuracy」
- §3.4a：Anthropic 的 tool-testing agent **重写工具描述**后，后续 agent 任务完成时间 **−40%**
- §3.2：SWE-agent 的**接口设计消融单项值 10.7pp**

**根因（这里要给出与外部证据不同的一层判断）**
外部证据批评的是「从大注册表里同时展示很多工具」。wyj-code 的 20 个是**恒定可见的核心执行面**，且每个都有具体的血泪史：
- `ContextRecall` 必须常驻 —— 否则清理产生的 `cas://` 引用成死路，清理变不可逆（`agent.rs:176-178`）
- `TodoWrite` 必须常驻 —— 否则模型根本不知道它存在（`crates/core/CLAUDE.md:122` 记录的 v0.2.30 修复）
- computer 三件套必须常驻 —— 否则国内模型把「当前不可见」误判为「本会话未注册」，直接拒绝 GUI 任务（`agent.rs:187-192`）

所以**不能照搬「砍到 7 个」**。正确做法是：逐条复审「这条常驻的理由，在**当前**模型能力下是否还成立」，而不是拍一个数字。§4.4 的元原则说的正是这件事——**这些理由都是关于「模型做不到什么」的假设，会随模型变强而过期**。

**可回退的改法**
- **逐条复审 20 个常驻项**，每条记录「它编码的假设」+「该假设现在是否还成立」+「若移除，模型会怎样」（这一栏就是 G5 的台账）
- 工具描述**结构化**（分段而非单行：何时用 / 何时不用 / 边界与失败模式 / 例子），而不是继续加长
- 建 **tool-testing 回路**：用一个故意有缺陷的工具测模型是否会正确调用，据此迭代描述（Anthropic 的原始做法）

### G5 「默认委派」假设未复审 + 无 harness 组件台账 —— 严重度：中

**现象**
- `crates/core/src/prompts.rs:48`：「**Delegate to subagents proactively — it is the default, not a last resort.**」
- `crates/tools/src/descriptions.rs:29` 的 SubAgent 模板里又重复了一遍同样的措辞（「Delegate to subagents proactively — it is the default, not a last resort.」）
- 两处措辞完全相同，说明这是**一条被写死两次的假设**
- 全仓库没有任何地方记录「哪个 harness 组件编码了什么假设」

**外部证据**
- §3.4d：多 agent 的收益需要三个条件同时成立——**可真正并行 + 信息量超单上下文 + 需与众多工具交互**；而 Anthropic 明确写出负面条件：**「大多数编码任务可并行的部分比研究任务少」**，且「LLM agent 目前不擅长实时协调与委派」
- §3.4d：多 agent 代价约 **15×** token
- §4.4：**每个 harness 组件都编码了一个关于「模型做不到什么」的假设**，这些假设要持续重新审计
- §3.3：Anthropic 砍掉 sprint 分解后成本 $200 → $124.70

**根因**
「默认委派」是一个**关于「主 agent 一次读不了那么多文件」的历史假设**。它编码在提示词里、被复制了两份、且**没有任何撤销机制**——模型变强之后它只会继续生效。

**影响面**
对**编码类任务**（wyj-code 的主战场），「委派一个子 agent」的成本是：额外 1 次 LLM 往返起（子 agent 有自己完整的 system prompt + 工具 schema）、子 agent 的结论回传要过 `CONCLUSION_MAX_BYTES = 8000` 的截断、且丢失中间探索过程（`descriptions.rs:29`：「Intermediate tool calls, file reads, and exploration output stay inside the subagent and do NOT pollute your main conversation」）。若委派不划算，这三笔都是净损耗。

**可回退的改法**
- 把「默认委派」改为**按可并行性判定**的措辞（默认开关保持旧值，新行为可选）
- **建立 harness 组件台账**：每个组件（每个提示词段落、每条规则、每个工具常驻项）记录「它编码的假设」+「证据来源」+「复审触发条件（模型升级 / 评测结果）」。台账本身不改行为，是 §4.4 原则 1 的落地

### G6 无完成门禁 + 无 oracle 感知的采样分配 —— 严重度：中

**现象**
- TodoWrite 记录了 `pending` / `in_progress` / `completed`（`crates/tools/src/descriptions.rs:22`），但**没有任何机制读取它**来决定「模型能否宣布完成」
- §5.7 实测：**59% 的会话结束时 todo 未收尾**
- `crates/tools/src/sub_agent.rs` 有 `subagent_report_body`（`:prompts.rs:515-520`）与未完成交接的 `subagent_handoff_body`（`:prompts.rs:532-555`）——**子 agent 有「未完成时必须交接」的机制，主 agent 没有**

**外部证据**
- §3.3：Anthropic 的做法是**每条 criterion 有硬阈值，任一条不达标该 sprint 直接 FAIL**；且失败模式之一正是「项目后期 agent 看到已有进展就宣布完成」
- §4.1 / §3.4e：**弱验证下加宽候选几乎无用**（N=4→8 只 +1.7pp）；强 verifier 下 N=8 可把 9B 从 50.00% 提到 68.03%。→ **先建 verifier，再建 sampler**
- §4.6 原则 25：端到端评估优于逐轮评估——判「是否达到正确最终状态」，而不是「是否按预设步骤走」

**根因**
TodoWrite 被设计成**给用户看的进度面板**（这是它的原始定位，`crates/core/CLAUDE.md:122` 描述的正是「任务列表面板永远不出现」的 bug），而不是**给框架看的完成判据**。两者需要的精度完全不同：面板容忍 59% 未收尾，门禁不能容忍。

**可回退的改法**
- 收尾门禁（与 G1 的改法 1 同一个机制）：模型宣布完成 → 检查是否有未收尾 todo / 未通过的 criterion → 未通过则**要求继续**而不是结束回合
- **oracle 感知的采样分配**：只在存在真 oracle（项目声明的 `verify.command`、或评测集判据）时才启用 best-of-N；没有 oracle 时**明确不启用**（避免 §4.1 的负收益）

---

## 7. 改进路线图

> **总原则（对应 §4.6 原则 1 与 §4.4）**：任何一项都**不得改变现有默认行为**。新增 `[harness]` 配置块，所有开关的默认值 = 现行行为；任何一项都可以单独回退而不影响其它项。
> **前置依赖**：P0-3（台账）与 §8（评测集）应该先落地——否则后面每一项都只能靠感觉判断，而 §4.1 证明了很多 harness 改动是负收益。
> **实施记录**：P0-1 已于 2026-10-21 落地（见 §7.2）。实现与下表的原始设想有一处**有意偏离**，理由见 §7.2。

### P0 —— 承重墙

| ID | 名称 | 改动面 | 开关位置 | 验证方式 | 回退 | 状态 |
|---|---|---|---|---|---|---|
| P0-1 | **收尾验收闭环**：模型宣布完成时，框架检查未收尾 todo / 未达标 criterion，未通过则要求继续 | `crates/core/src/agent.rs` 收尾判定 + `crates/core/src/verify.rs` 扩展 | `[harness].completion_gate = false`（默认关） | 评测集「宣布完成但未达标」任务的成功率 | 关开关即回到现行行为 | ✅ 已实施（§7.2）；⚠️ **首轮实测零收益**（§7.4） |
| P0-2 | **独立验收者**：新增只读 evaluator 子 agent 类型（与 generator 分离），逐条 criterion 硬阈值判定 | `crates/core/src/agent_def.rs` + `crates/tools/src/sub_agent.rs` | `[harness].evaluator.enabled = false` | 对照实验：有/无 evaluator 的成功率与 token 成本 | 同上 | ⬜ 未开始 |
| P0-3 | **harness 组件台账**：`/doctor` 增加「组件 → 它编码的假设 → 证据来源 → 复审触发条件」清单 | 新增 `crates/core/src/harness_ledger.rs` + `/doctor` 面板 | 默认开（**纯观测，不改行为**） | 台账覆盖率达 100%（每个提示词段落 / 每条规则 / 每个常驻工具项都有条目） | — | ⬜ 未开始 |
| P0-4 | **评测基线**（§8 规格） | `benchmarks/`（复用既有骨架，扩到 20 任务） | 独立脚本，不进主二进制 | — | — | ✅ 已实施（2026-10-21，见 §7.3） |

### P1 —— 高价值可回退项

| ID | 名称 | 改动面 | 开关 | 验证方式 |
|---|---|---|---|---|
| P1-1 | **文件化进度载体**：`.wyj-code/progress.md`（或 JSON），压缩时作为独立消息保留、跨 session 保留 | `crates/core/src/compact.rs` + 新增读写工具 | `[harness].progress_file = false` | 长程任务（>50 轮）的中途压缩后重复劳动率 |
| P1-2 | **常驻工具逐条复审**：20 → 目标 ≤12，每条记录移除后的可发现性判据 | `crates/core/src/agent.rs:162-193` | `[harness].lazy_core_tools` 白名单（默认 = 现行 20 项） | 工具选择准确率（§3.4b 的口径）+ 各工具实际调用率 |
| P1-3 | **委派策略可选化**：把「默认委派」改为按可并行性判定的措辞 | `crates/core/src/prompts.rs:48` + `descriptions.rs:29` | `[harness].delegation_policy = "default"`（可选 `"parallel-aware"`） | 编码类任务的成功率 / token 成本对比 |
| P1-4 | **oracle 感知的采样分配**：仅在有真 oracle 时启用 best-of-N | `crates/core/src/agent.rs` | `[harness].best_of_n = 0`（默认关） | 有 oracle 任务的 Pass@k 曲线（§3.4e 口径） |

### P2 —— 打磨项

| ID | 名称 | 改动面 | 验证方式 |
|---|---|---|---|
| P2-1 | **工具描述结构化**：`descriptions.rs` 的超长单行改为「何时用 / 何时不用 / 边界与失败模式 / 例子」四段 | `crates/tools/src/descriptions.rs` | tool-testing 回路（Anthropic 原始做法）测得的任务完成时间（目标：−20% 以上） |
| P2-2 | **`COMPUTER_USE_HINT` 拆段**：约 16KB 单行拆为结构化短段 + 失败案例表 | `crates/core/src/prompts.rs:373` | GUI 任务成功率 + 提示词 token 占用 |
| P2-3 | **完成门禁的 criterion 化**：项目可在 `.wyj-code/settings.json` 声明验收清单（延续 `verify.rs` 的零假设风格） | `crates/core/src/verify.rs` 扩展 | 有/无 criterion 声明的任务成功率 |

### P3 —— 探索项

| ID | 名称 | 依据 |
|---|---|---|
| P3-1 | **AHE 式自动演化 harness**：从最小 seed 出发，每次编辑带「自我声明的预测」，下轮用任务级结果验证 | §3.3 AHE：+7.3pp，冻结后可零演化跨模型迁移，且少用 12% token |
| P3-2 | **端到端 checkpoint 评估**：按 Anthropic 建议，用离散 checkpoint 判「是否达到正确最终状态」而非「是否按步骤走」 | §4.6 原则 25 |
| P3-3 | **context anxiety 检测**：识别「接近窗口上限时模型提前收尾」的行为（Anthropic 在 Sonnet 4.5 上观察到） | §3.3 |

### 7.1 明确**不**建议改动的部分

| 项 | 理由 |
|---|---|
| `auto_rules.rs` 的 12 条 hard_deny | 安全边界，不该交给模型判断（§4.6 原则 2） |
| 分类器四档规则（12/13/15/5 条） | 「结构化输出 + 自然语言规则」降低控制噪声的典范（§3.1 H1→H2 机制） |
| stable/volatile 两段拆分 | 已对齐 Anthropic 的 prompt cache 纪律 |
| 清理优先于摘要 + CAS 外部化 | 方向正确；§3.4c 的证据支持「清理」优于「摘要」，wyj-code 已先做清理 |
| 压缩请求复用主对话前缀 | 与 §3.4c「压缩要保前缀」一致，且 DeepSeek 缓存命中价是 miss 的 1/50 |
| 错误回灌时回写模型原始参数 | 正是 §3.1「降低控制噪声」的做法 |

### 7.2 实施记录：P0-1 收尾验收闭环（2026-10-21）

**已落地**。新增 `crates/core/src/acceptance.rs`（判据类型 + 加载 + 判定 + 证据报告），收尾门禁接在 `crates/core/src/agent.rs` 的 `if !has_tool_calls && !got_injection {` 块顶部、`break` 之前。

**与原始设想的一处有意偏离**：开关没有放在 `config.toml` 的 `[harness]` 块，而是放在**项目级 `.wyj-code/settings.json` 的 `[acceptance]` 块**。理由是安全边界：`command` 判据会被 `sh -c` 直接执行，与 `verify.command` **同级**的任意命令执行面，因此它必须过 `core::hooks_trust` 信任门控——而那道门控只认 `settings.json` 这一套来源。放进 `config.toml`（全局、跨项目、无信任门控）会开一个后门。

因此 `core::hooks_trust` 的指纹键从 `["hooks", "verify"]` 扩到 `["hooks", "verify", "acceptance"]`。**向后兼容**：指纹只纳入「文件里确实存在」的键，既有项目没有 `[acceptance]` 块时指纹逐字节不变，已批准的记录不受影响；一旦新增该块则指纹变化 → 需要重新批准（多了一份能执行命令的配置就该重新授权）。测试 `the_fingerprint_is_unchanged_when_acceptance_is_absent` 钉住这条。

**实现要点**（与 §6 G1/G6 的对应）：

| 原文设想 | 实际实现 | 理由 |
|---|---|---|
| 框架检查未收尾 todo **或** 未达标 criterion | **只做 criterion**（todo 门禁留待 P0-2 一起做） | TodoWrite 的「完成」是模型自己写的，粒度不足以当判据；先做框架能判的那一半 |
| 独立验收者（evaluator） | **未做**，留 P0-2 | §3.3 明确 evaluator 必须与 generator 分离；这一刀不碰 agent 定义体系 |
| 回灌「证据」而非评语 | 已做：命令退出码 + 输出尾部、命中的 `文件:行号` | §3.1 的「结构化错误反馈」机制 |
| 每项单独开关、可回退 | 已做：`enabled: false` 或判据为空 = 门禁完全不参与 | 零假设 |
| —— | **追加**：判据配错时**恒判不通过**（fail-closed）；路径越出工作目录判为无效；Plan 模式跳过；预算耗尽按「未完成」交付；声明了但解析失败时收尾明确报给用户 | 静默关掉门禁比没有门禁更危险 |

**验证**：`cargo test -p wyj-core` 554 passed（新增 23 项：17 项 `acceptance::tests` + 2 项 `hooks_trust::tests` + 4 项 agent 级门禁行为）；`cargo clippy -p wyj-core -p wyj-code -p wyj-i18n --all-targets -- -D warnings` clean。

**尚未验证的部分（诚实记录）**：§8 的评测集还没建，所以「这个门禁到底让任务成功率提升了多少」**目前没有任何数据**。本文所有关于「G1 是瓶颈」的判断仍是推断。P0-4（评测集）落地后需要回头做一次有/无门禁的对照——注意 §4.1 的教训：**verifier 未必是正收益**，若验收对象与最终验收对象不对齐，这条也会是负的。

### 7.3 实施记录：P0-4 评测基线（2026-10-21）

**已落地**，但**复用了既有骨架而不是新建 `evals/harness/`**。仓库里本来就有 `benchmarks/`（4 个任务 + jsonl + `compare.sh`），功能形态与 §8 的规格几乎一致。再起一套等于让「哪套是权威」变成不确定——这是 `crates/core/src/lib.rs` 对 `interfaces` 模块写下的同一条理由。所以扩它。

**实际落点**：

| §8 规格 | 实际实现 | 差异说明 |
|---|---|---|
| `evals/harness/` 新目录 | `benchmarks/` | 复用既有骨架 |
| 20 任务，五类各 4 个 | 20 任务：T1/T2/T4 各 5 个，T3 4 个，T5 1 个 | 按现有 fixture 能自然产出的形态分配，不硬凑数字 |
| 判定三分（自动 oracle / 预设 rubric / 行为指标） | **全部自动 oracle** | 20 个任务里没有需要人工打分的；rubric 留到任务集扩到「主观质量」类时再说 |
| `scorers/oracle.rs` | `tasks/*/check.sh`（bash + python3） | 判据是 shell 级的，不需要编译进二进制 |
| `run.rs` | `benchmarks/run.sh` | 同上 |
| 主指标 pass@1 | `success_rate`（通过数/总数） | `compare.sh` 按 task 聚合重复记录 |
| 消融协议（一次一个组件 / ≥3 重复 / ≥2 模型） | `--repeat N` + `--label` + `--acceptance` | 重复与配置标签已实现；多模型仍需手工换 profile |

**新增的两个关键机制**：

1. **`verify-tasks.sh`（任务集自检，零 API 费用）**——验证三件事：注入 bug 后 `cargo test` 必须变红；未修复时 `check.sh` 必须失败；恢复 fixture 源码后 `check.sh` 必须通过。少了任何一条，任务测的都是「什么都不做也能过」或「永远过不了」。**这一步比跑批本身更重要**：判定写错的话，跑一百次也只会得到一个稳定但错误的数字。20 个任务全部通过自检。

2. **`gate_blocks` 指标**——从 stdout 里数门禁实际拦截次数。这是验证 P0-1 是否真的生效的硬指标：配置里写了 `[acceptance]` 不等于门禁跑过（可能因信任门控被跳过、可能因 Plan 模式被绕过、可能判据恒过）。只看「成功率」分不出这些情况。

**端到端验证（真实 CLI，非单测）**：

用一个必然失败的判据（要求 `REQUIRED.md` 存在）跑一次真实会话，观察到完整闭环：

```
5 个模块：geometry、report、stats、text、vector。
[验收未通过：1/1 条判据不满足（must-exist），失败证据已回灌给模型让它继续修]
  → 模型：需要创建 REQUIRED.md … 先看看有没有其他线索，比如 .wyj-code/settings.json
  → Read / Read / Bash / Write
已创建 `/tmp/gate-e2e/REQUIRED.md`
[验收通过：1 条判据全部满足]
```

这条日志同时证明了四件事：门禁在收尾时跑、失败证据进了历史、模型据此改变了行为、通过后正常收尾。

**顺带修的既有 bug**：`benchmarks/tasks/01-fix-bug/setup.sh` 原来用 `sed -i ''`（BSD sed 专有语法），在 Linux CI 上会失败或产生多余文件。已改为 python3（`compare.sh` 本来就依赖它）。

**仍然没有的**：跨模型基线（只有本机那一个端点）、大仓库任务（fixture 只有 5 个文件）、语义级判定（T3/T5 是关键词级的）。这三条都写在 `benchmarks/README.md` 的「已知限制」里。

### 7.4 第一批实测数据（2026-10-21）：门禁在饱和基线上一分未得

这是本文最重要的一节——**它否定了 §6 G1 的一个隐含假设**。

**实测设置**：20 个任务 + 1 个门禁专项任务，模型 `MiniMax-M3.1-Flash`（reasoning_effort = high），
门禁开/关各跑一遍（各 20 条有效记录）。

| 指标 | gate-off | gate-on | 差异 |
|---|---|---|---|
| 任务成功率 | **100%**（20/20） | **100%**（20/20） | **=** |
| `gate_blocks`（门禁实际拦截次数） | 0 | **0** | — |
| api_calls（均值） | 6.3 | 6.7 | +6% |
| output_tokens（均值） | 2179.8 | 2255.6 | +3% |
| duration_secs（均值） | 23.5 | 24.7 | +5% |

**结论一：在这批任务上，门禁没有可测量的收益——因为一次都没触发过。**
`gate_blocks` 全为 0 意味着：模型每次都在**第一次**宣布完成时就已满足全部判据。
门禁代码路径确实执行了（真实 CLI 验证过 `[验收通过：2 条判据全部满足]`），但它
只是一个恒真检查，等于零成本零收益。

**结论二：20 个任务全部通过，基线已经饱和。** 这不是好消息——**饱和的基线测不出任何改进**。
`success_rate` 恒为 100%，任何 harness 改动在这个指标上都是 `=`。真正还有分辨力的
只剩 token / 轮数这类成本指标（而它们被模型随机性主导，单次运行不可信）。

**结论三：G1「无闭合验证回路是瓶颈」这个判断，在饱和任务集上不成立。**
§6 G1 的证据是「Anthropic 观察到模型意识不到功能没跑通」——那是**长程、跨 context window、
真实项目**的任务形态。当前 fixture 只有 5 个文件、单轮内可完成，模型不会在这里失去状态。
**门禁的价值区间不在这里。**

**结论四（方法论）**：这正是 §4.1 警告过的事——**verifier 未必是正收益**。当时引用的是
NLAH 论文里 verifier 在 SWE 上 −0.8pp 的数字；现在 wyj-code 自己的数据给出了同向的
初步印证：在模型已经能做对的区间里，加一层验收只会增加 +6% 的往返，不改变结果。

**下一步该怎么改这批任务（而不是改门禁）**：

| 方向 | 具体做法 | 为什么 |
|---|---|---|
| **提高难度直到基线掉到 60–80%** | 加长 fixture（20+ 文件、真实模块依赖）、加跨文件耦合、加「必须读规约才知道的要求」 | 100% 的基线没有分辨力；60–80% 才能测出 ±5pp |
| **加入「模型倾向提前收尾」的任务形态** | 需求里埋多步骤验收项（实现 + 测试 + 文档 + 迁移），少做一步仍能编译 | 这是门禁唯一能体现价值的形态 |
| **加长程任务** | 要求跨越多个文件的重构 + 中途必然触发压缩 | G3（状态载体）也只能在这里测出来 |
| **成本指标改成多次重复的分布** | `--repeat 5` 起，报中位数与四分位 | 单次 token 差异在 ±30% 噪声里 |

**这次实测的价值不是「证明了门禁有用」，而是「用数据否定了它当前的无用区间」**——
而这正是 §6 G2 要建基线的理由：没有它，我们会把「门禁没触发」误读成「门禁有效」。

---

## 8. Phase 2 规格：20 任务 harness 评测集

> 本章是**规格**，本次不实施。目标：让「模型能力」与「harness 能力」在 wyj-code 里可分离、可测量、可消融。

### 8.1 为什么是 20 个

- Anthropic 经验：20 个真实 query 就能看出 30%→80% 的 prompt 改动效果（§4.6 原则 26）
- 论文的方差分解（§3.1）用 100 题 + 每格 2 次重复跑出 7.8× 的比值；20 题 × 每组 3 次重复是**能负担得起的最小版**
- 20 题 × 3 重复 × N 个配置 = 60N 次跑批，必须能自动化，否则不可持续

### 8.2 任务集构成

五类各 4 个，覆盖 wyj-code 的实际使用形态：

| 类 | 数量 | 任务形态 | 典型验收方式 |
|---|---|---|---|
| **T1 多文件功能实现** | 4 | 在本仓库（或一个 fixture 仓库）加一个跨 3–5 个文件的功能 | 单测通过 + 文件存在断言 |
| **T2 bug 定位修复** | 4 | 注入一个已知 bug，要求定位并修复 | 回归测试由红转绿 |
| **T3 仓库理解问答** | 4 | 「X 是怎么实现的 / 在哪调用 / 为什么这么设计」 | **rubric + 人工快判**（比对 `file:line` 引用是否正确） |
| **T4 长程重构** | 4 | 跨 10+ 文件的重构，**必然触发压缩** | 单测通过 + 「压缩后是否重复劳动」的行为指标 |
| **T5 中文 commit 生成** | 4 | 改代码并写中文 commit message | 提交约定检查（正则 + 人工快判） |

**fixture 仓库**：不要在 wyj-code 主仓上跑评测（会污染工作区）。建议每个任务一个独立的临时 git 仓库，初始 commit 固定。

### 8.3 判定方式（三分）

| 类型 | 判定 | 适用 |
|---|---|---|
| **自动 oracle** | 脚本断言：退出码、产物存在、单测通过、文件内容匹配 | T1 / T2 / T4 / T5 的一部分 |
| **半自动 rubric** | 固定评分表（0/1 或 1–5）+ 人工打分，评分表**预先写死**并入版本控制 | T3 / T5 的 commit 质量 |
| **行为指标** | 从 trace 里自动提取，不需要额外标注 | T4 专测：压缩次数、重复 Read 同一文件的次数、平均轮数 |

**关键设计**：T3/T5 的 rubric **必须预先写死**。事后调整 rubric 是 harness 评测最常见的作弊路径。

### 8.4 指标

**主指标**：任务成功率（pass@1）

**副指标**（用来解释主指标为什么变）：

| 指标 | 从哪来 | 为什么重要 |
|---|---|---|
| 平均 LLM 往返数 | trace | 呼应 §5.7「`turn` 计的是 LLM 往返而不是工作量」 |
| 总 token（输入 + 输出 + cache_read） | usage | 呼应「多 agent ≈15× token」的代价 |
| **工具误选率** | trace 里工具名 + 参数 | 呼应 §3.4b 的「选对工具的能力」；wyj-code 特有：还要统计**折叠工具被检索但未调用**的比例 |
| 首次通过率（不重试） | trace | 区分「模型一次做对」与「框架兜住了」 |
| 压缩触发次数 / 压缩后重复劳动率 | trace + 日志 | 呼应 G3 |
| 宣布完成但未达标率 | 收尾判定 | 直接量 G1 / G6 |
| **prompt 指纹** | `crates/core/src/prompt_fingerprint`（已有） | 每轮记录 system + tools 两段的 sha256，确保不同组跑的确实是不同的 harness |

### 8.5 消融协议

- **一次只改一个组件**（§4.4 原则 17）。Anthropic 的粗剪枝之所以没复现原性能，就是因为不知道哪些组件承重。
- 每组 **≥3 次重复**（§3.1 的方差分解说明单次运行不可信）
- 固定：任务顺序、模型、端点、随机种子（若模型支持）、Docker/fixture 环境、步数与超时预算
- 记录每次运行的完整 trace，失败案例**逐个归档**（Anthropic 的经验：小样本下看失败案例比看聚合分数更能指导下一轮改动）
- **多模型对照**：至少 2 个模型（不同厂商/协议路径），以验证 §2.4 要求的「multiple models per scaffold」

### 8.6 目录与格式建议

```
evals/harness/
├── tasks.yaml            # 20 个任务的定义（id / 类型 / fixture / prompt / oracle / rubric）
├── fixtures/             # 各任务的初始仓库快照
├── scorers/
│   ├── oracle.rs         # 自动判定
│   ├── rubric.md         # 人工评分表（预先写死）
│   └── metrics.rs        # trace → 副指标
├── run.rs                # 跑批：串行/并行、重复次数、配置矩阵
└── README.md             # 如何加一个任务、如何加一个消融配置
```

配置走 `~/.wyj-code/settings.json` 的新 `[harness_eval]` 块（与 `verify` 同源的信任门控）。

### 8.7 与既有资产的关系

- **复用 `crates/core/src/prompt_fingerprint`**：每轮已经采集 system + tools 两段指纹，评测时直接落盘，保证「配置 A vs 配置 B」的差异是可归因的
- **复用 `crates/tools/src/trace.rs`**：已有输入/输出截断策略（`:24-35`），评测时按需调大
- **复用 `crates/cli/src/usage_cmd.rs`** 的 usage 统计口径：token 指标不另造一套
- **不新建第二套 trace 格式**

---

## 9. 信息缺口与后续工作

### 9.1 未找到可靠证据 / 未充分核实的部分

明确列出，不编造：

1. **「工具数量超过 10–15 个就明显掉点」这一具体阈值** —— 只在二手博客见到，**未找到一手论文/官方基准支撑**。可核实的一手结论是 arXiv:2605.24660 的「更短的自适应列表更好」。**本报告不引用「10–15」这个数字。**
2. **「计划-执行分离」（plan-then-execute）的独立受控消融** —— 未找到。现有证据是 Anthropic 的单案例（去掉 planner → generator 欠定范围），属案例级。
3. **「上下文噪音 / 指令注入的剂量-反应曲线」** —— 未找到系统性研究。
4. **Huang et al. (arXiv:2310.01798) 的 GSM8K 95.5→91.5→89.0** —— 经二手综述核实，**未直接抓取原文**。
5. **METR Time Horizon 2.0 / 2026 年最新方法学** —— 只见到第三方转述与站点存在性，未抓一手方法学文档。
6. **Terminal-Bench 官方 leaderboard 的「同模型 × 多 harness」完整对照表** —— 未取得结构化数据；部分数字来自第三方聚合博客，**强度弱**，且与官方当前版本口径可能不一致。
7. **LangChain / LangGraph / Aider / SWE-agent 官方对 harness 的定义文档** —— 未逐一抓取；LangChain 的 "Anatomy of an Agent Harness" 只在第三方镜像见到。
8. **Anthropic《Building a C compiler with a team of parallel Claudes》《Effective context engineering for AI agents》《Code execution with MCP》** —— 未抓取正文。
9. **Thoughtworks Böckeler 的 martinfowler.com 原文** —— 仅经 Wikipedia 引用得知其 inner/outer harness 与 guides/sensors 框架，**未直接抓取**。
10. **OpenAI《Harness engineering》全文** —— 仅凭搜索摘要。
11. **METR time horizon 与 harness 的交互项** —— METR 每个模型用的 scaffold 不同，未找到官方对「同模型换 scaffold 后 time horizon 变化多少」的公开量化。

### 9.2 本文结论的复核方法（每条如何被证伪）

| 结论 | 如何证伪 |
|---|---|
| G1 无闭合验证回路是瓶颈 | 用 §8 评测集跑 T1/T4；开/关 P0-1、P0-2 对比成功率与 token。**已做首轮（§7.4）：基线饱和，门禁零触发 → 在当前任务集上该结论不成立**，需先提高任务难度再测 |
| G2 缺评测基线 | 建立 §8 后，所有后续结论都必须能被它检验。**已完成（§7.3）**，并立刻产出了 §7.4 的反证 |
| G3 长程状态载体不足 | T4 任务在压缩点前后统计「重复 Read 同一文件次数」 |
| G4 常驻工具偏多 | 用 §8 统计 20 个常驻工具各自的实际调用率；对调用率长期为 0 的项做移除消融 |
| G5 「默认委派」是负收益 | T1/T2（编码类）对比 `delegation_policy = "default"` vs `"parallel-aware"` 的成功率与 token |
| G6 无完成门禁 | 统计「宣布完成但未达标率」在开/关门禁下的变化 |

### 9.3 后续工作建议顺序

1. **P0-3 台账**（纯观测，零风险，立即可做，且是后续所有工作的索引）
2. **§8 评测集**（P0-4）—— 没有它，后面全是盲改
3. **P0-1 / P0-2 验收闭环**—— 收益预期最高（§3.1 的 H2→H3 机制）
4. **P1-1 文件化进度载体** —— 独立可做，不依赖验收闭环
5. **P1-2 / P1-3 逐条复审** —— 需要台账与评测集就位
6. **P2 描述打磨** —— 需要 tool-testing 回路，收益确定但边际

---

## 10. 参考资料

### 10.1 一手论文

| 论文 | 链接 | 本文引用处 |
|---|---|---|
| Harness 方差分解（3×3 受控实验） | arXiv:2605.23950 | §3.1 / §3.2 / §4.5 |
| Natural-Language Agent Harnesses（三元形式化） | arXiv:2603.25723 | §2.2 / §4.1 / §4.5 |
| What Does an LLM-Agent Leaderboard Rank Actually Compare? | arXiv:2609.07785 | §2.4 |
| How Many Tools Should an LLM Agent See? | arXiv:2605.24660 | §3.4b |
| Toward Reliable Context Compression for Long-Horizon Agents | arXiv:2608.06503 | §3.4c |
| Optimizing Context Compression for Long-horizon LLM Agents (ACON) | arXiv:2510.00615 | §3.4c |
| Mid-Harness: Scaling Actions Between Model and Harness | arXiv:2609.39982 | §3.4e / §4.1 |
| HAL（SWE-bench Verified Mini，reasoning effort 非单调） | arXiv:2510.11977 / https://hal.cs.princeton.edu | §4.2 |
| AHE（自动演化 harness） | arXiv:2604.25850 | §3.3 / §7 P3-1 |
| SWE-agent | arXiv:2405.15793 | §3.2 |
| Self-Refine / 自反思与 oracle 依赖 | arXiv:2310.01798（**未直接抓取**） | §4.3 |

### 10.2 官方工程博客与数据源

| 来源 | 链接 |
|---|---|
| Anthropic — Effective harnesses for long-running agents（2025-11-26） | https://www.anthropic.com/engineering/effective-harnesses-for-long-running-agents |
| Anthropic — Harness design for long-running application development（2026-03-24） | https://www.anthropic.com/research/harness-design-long-running-apps |
| Anthropic — Building a multi-agent research system（2025-06-13） | https://www.anthropic.com/engineering/built-multi-agent-research-system |
| METR — Time Horizons | https://metr.org/time-horizons |
| METR — Measuring AI Ability to Complete Long Tasks（2025-03-19） | https://metr.org/blog/2025-03-19-measuring-ai-ability-to-complete-long-tasks |
| OpenAI — Harness engineering（**仅摘要**） | https://openai.com/index/harness-engineering |
| Wikipedia — Agent harness（**间接引用 martinfowler.com**） | https://en.wikipedia.org/wiki/Agent_harness |
| LangChain — Anatomy of an Agent Harness（**仅第三方镜像**） | https://blog.langchain.com/ |
| mini-swe-agent（100 行循环） | https://github.com/SWE-agent/mini-swe-agent |

### 10.3 wyj-code 内部代码引用

| 主题 | 位置 |
|---|---|
| 主系统提示词 | `crates/core/src/prompts.rs:8-64`（`MAIN`）、`:198-209`（`<env>`）、`:373`（`COMPUTER_USE_HINT`）、`:436`（`COMPACT_SYSTEM`）、`:447-477`（`compact_instructions`） |
| 两段 system 契约 | `crates/api/src/provider.rs:28-59` |
| volatile 尾部拼装 | `crates/core/src/agent.rs:1166-1201` |
| 恒定可见工具清单 | `crates/core/src/agent.rs:162-193`（20 项） |
| lazy 折叠接线 | `crates/core/src/agent.rs:1073-1095`、`crates/cli/src/assembly.rs:243-263` |
| 折叠阈值 | `crates/config/src/lib.rs:579`（默认 12） |
| 主循环 / 并行分组 / 错误回灌 | `crates/core/src/agent.rs:1610`、`:2792-2828`、`:2512-2516`、`:2557-2594`、`:2711-2719`、`:2942-2950` |
| 轮数上限 | `crates/core/src/agent.rs:1585-1590`（200 / thinking 模型 32） |
| 上下文压缩 | `crates/core/src/compact.rs:28-30`、`:78-82`、`:519`、`:683-685` |
| 上下文清理 / CAS | `crates/core/src/context_edit.rs:56-65`、`:90-98`、`:191-303` |
| 编辑后自动校验 | `crates/core/src/verify.rs:44`（触发工具）、`:53`（输出上限）、`:114-132`（配置加载） |
| 确定性规则 / 分类器规则 | `crates/core/src/auto_rules.rs:22-98`（分类器四档）、`:138-222`（12 条 hard_deny）、`:229-258`（白名单） |
| 工具描述 | `crates/tools/src/descriptions.rs`（`BASH`:12、`SUB_AGENT_TEMPLATE`:29、`COMPUTER_CUSTOM_TEMPLATE`:50） |
| 工具输出截断 | `crates/tools/src/bash.rs:13`、`read.rs:12-13`、`glob.rs:12`、`webfetch.rs:14`、`context_recall.rs:41`、`handoff.rs:222` |
| 委派判据 | `crates/core/src/prompts.rs:48`、`crates/tools/src/descriptions.rs:29` |
| 本地 review（确定性，不调 LLM） | `crates/cli/src/review_cmd.rs` |
| 既有自测数据 | `crates/core/CLAUDE.md:104`、`:116-118`、`:122`、`:187`、`:248` |

---

## 附：一句话总结

**Harness 不是「模型的包装纸」，而是与权重正交、独立且经常更大的能力变量**——受控实验里 harness 方差可达模型方差的 7.8 倍并造成 6/9 的模型排名反转；但它也不是万能的，长程自主性是模型绑定的硬边界。

**对 wyj-code 的具体结论**：底子已经不差，缺的不是更多提示词，而是**两块承重墙**——① 一个「模型宣布完成 → 框架独立验收」的闭合回路（G1），② 一个能分离模型与 harness 的任务成功率评测集（G2）。其余四项（长程状态载体、常驻工具与描述质量、委派假设的复审、完成门禁与 oracle 感知的采样）都是这两块建好之后的自然产物。

**最重要的元结论**：每个 harness 组件都编码了一个关于「模型做不到什么」的假设。wyj-code 目前的组件假设散落在提示词里、复制了两份、没有台账、也没有撤销机制。**把它们显式化、编号、附上证据来源和复审触发条件，本身就是第一项改进。**
