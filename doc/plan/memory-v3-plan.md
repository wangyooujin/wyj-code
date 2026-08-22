# Memory v3 整改实施计划

日期：2026-08-20

## 目标

把跨会话记忆从“后台从成功 Episode 猜经验并整段注入”改成可检索、可追溯、可纠正的 claim 系统。框架只负责作用域、来源、时间、TTL、脱敏、原子写入、冲突与批准边界；模型通过工具自主决定何时搜索、阅读、写入、关联和探索。

`stock2` 的首要验收场景是：用户或工具确认当前持仓后退出，在新会话从项目根或任意子目录启动，询问“重新分析持仓”或“继续”，能够召回最新持仓、历史变更、来源和观察时间，不恢复已经清仓的旧状态。

## 架构边界

- Memory v3：instruction、preference、fact、mutable_state、event、workflow、hypothesis、reference。
- Evolution：继续记录 Episode，并治理 Rule/Skill 候选、批准、回滚；Memory v3 启用时进入 governance-only 分析，不生成或注入普通 Evolution Memory v2。
- CLAUDE.md/AGENTS.md：保留显式项目指令加载，不承担频繁变化的持仓、历史事件和外部观测数据。
- `/memory`：控制 Memory v3 的使用和生成，并展示真实 v3 索引；`/evolve` 只控制进化治理。

## 数据模型与安全约束

每条 claim 包含：稳定 ID、类型、global/project/workspace 作用域、标题、正文、实体标识、标签、来源类型与定位、观察时间、有效期、证据、置信度、状态、supersedes/superseded_by 和审计时间。

- mutable_state 必须有来源定位和观察时间。
- hypothesis 必须有 TTL；过期后不参与召回。
- external 来源可以写 fact/event/reference/hypothesis，但外部 instruction/preference/workflow 拒绝入库。
- 用户纠正或新的同主题 mutable_state 自动 supersede 旧记录，不原地覆盖历史。
- 当前工具目录、权限模式、临时网络/DNS/环境变量等易变运行态不允许成为长期记忆。
- 写入前统一做 secret redaction；记录与 supersede 都写审计日志。

## 存储与项目身份

- 使用本地 JSON claim 文件和内存 BM25 风格索引，不依赖外部 embedding API；写入采用临时文件加 rename。
- global、project、workspace 分目录存储；项目只读取显式加入的共享 workspace。
- 非 Git 项目向上寻找 `.wyj-code/project.toml` 作为稳定根；CLI 增加 `--project-root` 进程级显式覆盖。
- `stock2` 项目清单加入共享 workspace `a-share`。代码事实保持 project scope，通用交互偏好写 global，持仓与交易规则写 `a-share`。

## 检索与注入

- 英文/数字标识按 token，中文按 unigram/bigram/trigram 建索引，实体代码与精确短语额外加权。
- 查询由当前请求和最近若干用户任务合成；“继续/再分析/重新来”等短续接词不会单独检索。
- 自动注入仅返回少量相关 active claim，并展示 ID、类型、作用域、来源、观察时间和过期时间。
- 更深的历史、证据和相邻事实由模型调用 `Memory` 工具探索，不把固定流程写死在 harness。

## 耐久后台任务

每轮完成后先把待提取会话证据原子写入项目队列，再启动单 worker 消费。任务包含 attempts、last_error 和状态；进程退出时 pending/running 任务在下次打开时恢复为 pending。提取结果仍经过与显式工具写入相同的验证、脱敏和 supersede 规则。

## 验收

- `/memory` 开关实际同时控制 v3 recall 与 extraction。
- 中文“重新分析持仓”命中持仓状态；“继续”能利用最近任务上下文。
- 新的用户纠正 supersede 旧状态，旧记录仍可审计但不参与默认召回。
- mutable_state 缺少时间/来源时拒绝；hypothesis 无 TTL 时拒绝。
- external fact 带 TTL 可保存，external instruction 被拒绝。
- global/project/workspace 隔离正确。
- `stock2/` 与 `stock2/analysis/` 解析为同一项目。
- 从项目外目录传入 `--project-root /Volumes/WD_APFS/stock2` 时仍使用 `stock2` 身份，不混入调用目录的 project scope。
- pending job 在 store reopen 后仍存在并可继续消费。
- Evolution governance-only 仍保存 Episode，重复工作流/失败模式仍能积累为 Skill/Rule 候选，但不会新增普通 Memory v2。
- `stock2` 跨会话回放得到最新持仓及其 provenance，不恢复已清仓旧持仓。
