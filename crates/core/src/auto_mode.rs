//! Auto 模式的判定闸门（对齐 Claude Code 的 auto mode）。
//!
//! 与手动模式「逐次弹窗」的区别只在一个位置：**谁来回答"这一步要不要问人"**。
//! Auto 模式把这个问题交给一条固定顺序的流水线：
//!
//! 1. 确定性 hard_deny —— 命中即拒，不发模型请求；
//! 2. 确定性 allow —— 命中即放行，不发模型请求；
//! 3. 模型分类器 —— 前两步都没结论时才付一次模型往返，让它按 `soft_deny` /
//!    `allow` / `environment` 规则做语义判定（"这次 `git push` 到底该不该做"）。
//!
//! 前两步刻意做成**同步、纯本地、可单测**：高频动作（读文件、跑测试）根本不该
//! 等一次网络往返，而真正危险的少数命令必须无论端点多慢都拦得住。
//!
//! 失败策略按上游语义 fail-closed：分类器请求超时 / 返回非法内容 / 空响应，一律
//! 判为拒绝。只有一种例外是 `AskUser`——配置显式关掉分类器时，那是用户自己选的
//! 偏好，不是故障，此时交回人工确认。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use wyj_api::{
    provider::{Provider, RequestOptions},
    types::{ContentBlock, Message, Role},
    SystemPrompt,
};

use crate::auto_rules::{
    self, DEFAULT_ALLOW, DEFAULT_ENVIRONMENT, DEFAULT_HARD_DENY, DEFAULT_SOFT_DENY,
};

/// 分类器输出 token 上限。
///
/// **不要按"判定只需一个极短的 JSON"来定这个数**：2026-10 实测，adaptive thinking
/// 模型（MiniMax-M3.1-Flash-Preview 之类）会在字段缺省时**自行开启思考**，
/// 而且**物理上拒绝关闭思考**：`thinking.type="disabled"` 会被 400 拒
/// （`requires adaptive thinking`）。`text_only()` 虽然写了
/// `thinking_switch="disabled"`，但 `api::thinking::apply_anthropic_thinking`
/// 正是为了避开这个 400 而把 `disabled` 降级成「不发 thinking 字段」——于是端点
/// 仍按默认（开）思考，辅助调用的 token 预算被思考吃光。
///
/// 所以唯一可调的旋钮就是这个预算。256 远远不够：实测真实分类器 prompt
/// （AUTO_CLASSIFIER + 四组规则 + 命令全文）下思考量 1105–2395 字符，256 会被
/// 思考吃光 → `stop_reason=MaxTokens` 且可见文本为空 → `parse_classifier_output`
/// 失败 → fail-closed，于是**每条需要过闸门的命令都被拒**，看起来像随机抽风
/// （短的 `ls -la` 有时能挤进去，长的必然失败）。
///
/// 2048 是实测余量：贴近 `ACTION_SUMMARY_LIMIT` 上限的长命令在 2048 下思考
/// 1972 字符、判定正常、端到端 5.5s。注意 `max_tokens` 只是上限不是预付，
/// 思考短的请求不会多消耗 token，所以给足余量没有成本。
const CLASSIFIER_MAX_TOKENS: u32 = 2048;

/// 送给分类器的动作描述上限，避免一条巨长命令把提示词顶爆。
const ACTION_SUMMARY_LIMIT: usize = 2000;

/// 送给分类器的用户目标上限。
const USER_GOAL_LIMIT: usize = 4000;

/// 分类器规则文本的总量上限：规则来自配置文件，写多了既贵又会稀释判定质量。
const RULES_LIMIT: usize = 12_000;

/// 用户在配置里写下的规则（`[auto_mode]` 段），未展开 `$defaults`。
#[derive(Debug, Clone, Default)]
pub struct AutoRuleOverrides {
    pub hard_deny: Vec<String>,
    pub soft_deny: Vec<String>,
    pub allow: Vec<String>,
    pub environment: Vec<String>,
}

impl AutoRuleOverrides {
    /// 展开 `$defaults` 得到最终规则集，并对「整段替换掉内置安全规则」告警。
    ///
    /// 告警而非拒绝：`allow` / `environment` 整段替换通常正是用户想要的（不想继承
    /// 一堆用不上的规则），但 `soft_deny` / `hard_deny` 丢掉内置项会实打实放宽
    /// 拦截面，这种配置值得在启动日志里喊一声。
    pub fn resolve(&self) -> ResolvedRules {
        ResolvedRules {
            hard_deny: splice(&self.hard_deny, DEFAULT_HARD_DENY, "hard_deny"),
            soft_deny: splice(&self.soft_deny, DEFAULT_SOFT_DENY, "soft_deny"),
            allow: splice(&self.allow, DEFAULT_ALLOW, "allow"),
            environment: splice(&self.environment, DEFAULT_ENVIRONMENT, "environment"),
        }
    }
}

fn splice(configured: &[String], defaults: &[&'static str], name: &str) -> Vec<String> {
    if !configured.is_empty() && !auto_rules::declares_defaults(configured) {
        tracing::warn!(
            rule_set = name,
            "autoMode.{name} 配置里没有 \"$defaults\"，内置规则已被整段替换"
        );
    }
    auto_rules::splice_defaults(configured, defaults)
}

/// 展开后的最终规则集。
#[derive(Debug, Clone, Default)]
pub struct ResolvedRules {
    pub hard_deny: Vec<String>,
    pub soft_deny: Vec<String>,
    pub allow: Vec<String>,
    pub environment: Vec<String>,
}

/// Auto 模式的闸门配置（由 CLI/TUI 从 `[auto_mode]` 段装配）。
#[derive(Debug, Clone)]
pub struct AutoGateConfig {
    /// 分类器总开关。关掉后只剩确定性规则层，拿不准的一律交回人工确认。
    pub enabled: bool,
    /// 分类器单次请求超时。超时按 fail-closed 判拒绝。
    pub timeout: Duration,
    /// 连续被分类器拒绝多少次后自动降级为逐次人工确认。
    pub max_consecutive_blocks: u32,
    /// 整个会话累计被拒多少次后自动降级。
    pub max_total_blocks: u32,
    /// true 时所有 shell 命令一律送审，不走确定性 allow 白名单（更安全但更慢）。
    pub classify_all_shell: bool,
    pub rules: ResolvedRules,
}

impl Default for AutoGateConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            timeout: Duration::from_secs(30),
            max_consecutive_blocks: 3,
            max_total_blocks: 20,
            classify_all_shell: false,
            rules: ResolvedRules::default(),
        }
    }
}

/// 一次待判定的动作。
#[derive(Debug, Clone)]
pub struct AutoReviewRequest {
    pub tool: String,
    /// 展示用的动作摘要（Bash 是完整命令，Edit/Write 是目标路径）。
    pub action_summary: String,
    pub input: Value,
    pub cwd: PathBuf,
    /// 用户这一轮原始请求，分类器据此判断「这一步是否在请求范围内」。
    pub user_goal: String,
}

/// 闸门判定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoVerdict {
    /// 自动放行。
    Allow,
    /// 直接拒绝。`reason` 会回灌给模型，让它换路子而不是重试。
    Deny { rule: String, reason: String },
    /// 交回人工确认（配置关掉了分类器，或根本没配分类器）。
    AskUser { reason: String },
}

/// 会话内的拦截计数。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AutoCounters {
    pub consecutive: u32,
    pub total: u32,
    /// 已因连续/累计超阈值而自动降级。
    pub degraded: bool,
}

/// Auto 模式闸门。
#[async_trait]
pub trait AutoGate: Send + Sync {
    /// 判定一次工具调用是否可以不经人工确认直接执行。
    async fn review(&self, request: &AutoReviewRequest) -> AutoVerdict;
    /// 当前拦截计数（供 UI 显示与降级判定）。
    fn counters(&self) -> AutoCounters;
    /// 是否已因连续或累计超阈值而应当停用分类器、逐次人工确认。
    fn should_degrade(&self) -> bool;
    /// 用户在人工确认里批准了一次，清空连续计数。
    fn note_human_approval(&self);
    /// 停用模型分类器：此后只剩确定性规则层，拿不准的一律交回逐次弹窗。
    /// 幂等。**只停用语义判定，不动 `hard_deny`**。
    fn disable_classifier(&self);
    /// 恢复模型分类器。幂等。
    fn enable_classifier(&self);
    /// 分类器当前是否可用（UI 展示「已降级为逐次确认」用）。
    fn classifier_enabled(&self) -> bool;
}

/// 分类器请求的记账：与主会话的 `Session.total_*` 分开统计。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AutoClassifierUsage {
    pub requests: u32,
    pub input_tokens: u32,
    pub output_tokens: u32,
}

/// 生产实现：确定性规则层 + 可选模型分类器。
pub struct AutoGateImpl {
    config: AutoGateConfig,
    /// `None` = 没配分类器（例如 `classifier_profile` 指向不存在的分组），
    /// 此时闸门退化为「确定性规则 + 其余交回人工」。
    classifier: Option<Arc<dyn Provider>>,
    /// 运行时停用位。`config.enabled` 是启动期配置、这个是运行期可逆的开关：
    /// 连续/累计拦截超阈值时停用它（降级为逐次确认），用户在界面上再切回
    /// auto 时重新打开。两者的效果都是「只剩确定性规则 + 弹窗」，但只有这个
    /// 开关能在不重启进程的前提下恢复。
    classifier_off: AtomicBool,
    counters: Mutex<AutoCounters>,
    usage: Mutex<AutoClassifierUsage>,
    /// 用户这一轮的原始请求，由 Agent 在回合入口写入。
    user_goal: Mutex<String>,
}

impl AutoGateImpl {
    pub fn new(config: AutoGateConfig, classifier: Option<Arc<dyn Provider>>) -> Self {
        Self {
            config,
            classifier,
            classifier_off: AtomicBool::new(false),
            counters: Mutex::new(AutoCounters::default()),
            usage: Mutex::new(AutoClassifierUsage::default()),
            user_goal: Mutex::new(String::new()),
        }
    }

    /// 分类器当前不可用的原因；`None` 表示可用。
    ///
    /// 三个来源各有各的成因，UI 上报「已降级」与排查时不能混为一谈：配置里
    /// 显式关掉、`classifier_profile` 写错导致没建成 Provider、以及运行期
    /// 因连续拦截超阈值被停用。
    fn classifier_unavailable(&self) -> Option<&'static str> {
        if !self.config.enabled {
            return Some("配置已关闭 auto 分类器（[auto_mode] enabled = false）");
        }
        if self.classifier.is_none() {
            return Some("未配置可用的分类器 Profile");
        }
        if self.classifier_off.load(Ordering::Relaxed) {
            return Some("分类器连续拦截过多，已停用");
        }
        None
    }

    /// 记录本轮用户请求。分类器判断「是否超出请求范围」时需要它。
    pub fn set_user_goal(&self, goal: &str) {
        if let Ok(mut slot) = self.user_goal.lock() {
            *slot = goal.to_string();
        }
    }

    pub fn usage(&self) -> AutoClassifierUsage {
        self.usage.lock().map(|u| *u).unwrap_or_default()
    }

    /// 按当前计数判断是否应当自动降级（连续超阈值，或累计超阈值）。
    ///
    /// 幂等：一旦越过阈值就恒为 true，由调用方决定「切换模式」这个副作用只做一次。
    pub fn should_degrade(&self) -> bool {
        let counters = self.counters.lock().map(|c| *c).unwrap_or_default();
        counters.consecutive >= self.config.max_consecutive_blocks
            || counters.total >= self.config.max_total_blocks
    }

    fn note_block(&self) {
        if let Ok(mut counters) = self.counters.lock() {
            counters.consecutive += 1;
            counters.total += 1;
            if counters.consecutive >= self.config.max_consecutive_blocks
                || counters.total >= self.config.max_total_blocks
            {
                counters.degraded = true;
            }
        }
    }

    /// 判定结果的统一落地：放行清零连续计数（用户目标里点名批准过的动作被模型确认
    /// 安全，不该让计数继续往上累），拒绝才计入降级预算，交回人工两者都不计。
    fn apply_verdict(&self, verdict: AutoVerdict) -> AutoVerdict {
        match &verdict {
            AutoVerdict::Allow => self.note_human_approval(),
            AutoVerdict::Deny { .. } => self.note_block(),
            AutoVerdict::AskUser { .. } => {}
        }
        verdict
    }

    /// 确定性放行判定：命中返回 `Some(依据)`，未命中返回 `None`。
    ///
    /// 覆盖三类：工作目录内的文件写、只读/构建/依赖安装类 shell 命令。
    fn deterministic_allow(&self, request: &AutoReviewRequest) -> Option<String> {
        match request.tool.as_str() {
            // 受保护路径已在 `PermissionPolicy::evaluate` 里先行 deny，
            // 走到这里只可能是普通文件写。
            "Write" | "Edit" | "NotebookEdit" => {
                let raw = request
                    .input
                    .get("file_path")
                    .or_else(|| request.input.get("notebook_path"))
                    .and_then(Value::as_str)?;
                let target =
                    crate::permission::safe_resolve_write_target(&request.cwd, raw).ok()?;
                if is_within_cwd(&target, &request.cwd) {
                    Some(format!("工作目录内文件写：{}", target.display()))
                } else {
                    None
                }
            }
            "Bash" => {
                let command = request.input.get("command").and_then(Value::as_str)?;
                if self.config.classify_all_shell {
                    return None;
                }
                auto_rules::match_allow(command).map(|basis| format!("命中允许规则：{basis}"))
            }
            _ => None,
        }
    }

    /// 确定性拒绝判定：命中返回 `Some(依据)`。
    fn deterministic_deny(&self, request: &AutoReviewRequest) -> Option<(String, String)> {
        let command = match request.tool.as_str() {
            "Bash" => request.input.get("command").and_then(Value::as_str),
            _ => None,
        }?;
        let pattern = auto_rules::match_hard_deny(command)?;
        Some((
            pattern.id.to_string(),
            format!("命中禁止规则：{}", pattern.summary),
        ))
    }

    fn rules_prompt(&self) -> String {
        let rules = &self.config.rules;
        let mut out = String::new();
        for (title, items) in [
            ("Environment", &rules.environment),
            ("Hard deny (unconditional)", &rules.hard_deny),
            (
                "Soft deny (clearable only by explicit user intent)",
                &rules.soft_deny,
            ),
            ("Allow", &rules.allow),
        ] {
            if items.is_empty() {
                continue;
            }
            out.push_str(&format!("\n## {title}\n"));
            for item in items {
                out.push_str("- ");
                out.push_str(item);
                out.push('\n');
            }
        }
        clip(&out, RULES_LIMIT)
    }

    /// `retry` 为真时追加一条格式提醒。分类器对同一份输入是确定性的，原样重发
    /// 大概率复现同一个坏输出；必须让第二次请求的输入**真的不同**（多一句
    /// "只吐 JSON"），重试才可能翻盘。
    fn build_prompt(&self, request: &AutoReviewRequest, retry: bool) -> String {
        // 必须读 `request.user_goal`（调用方每轮填好），不是 `self.user_goal`。
        //
        // 后者是接线 bug 的产物：`set_user_goal` 全仓零调用方，而 Agent 一直
        // 在 `AutoReviewRequest.user_goal` 里认真填了本轮请求。结果是
        // `<user_request>` **恒为字面量 `(unknown)`**。而分类器 prompt 的第 2
        // 层（soft deny 可被「用户明确要求」豁免）与第 4 层（explicit user
        // intent）都要求「用户的请求直接具体地描述了这个动作」——读不到用户
        // 说了什么，它对任何需要判断的动作只能按「不确定就 deny」走。
        //
        // 回退到 `self.user_goal` 是为了不破坏显式设置过它的调用方（目前没有，
        // 但它是 pub API）。
        let request_goal = request.user_goal.trim();
        let stored_goal = self
            .user_goal
            .lock()
            .ok()
            .map(|guard| guard.trim().to_string());
        let goal = if !request_goal.is_empty() {
            request_goal
        } else {
            match stored_goal.as_deref() {
                Some(stored) if !stored.is_empty() => stored,
                _ => "(unknown)",
            }
        };
        let mut out = String::new();
        out.push_str("<user_request>\n");
        out.push_str(&clip(goal, USER_GOAL_LIMIT));
        out.push_str("\n</user_request>\n\n<action>\ntool: ");
        out.push_str(&request.tool);
        out.push('\n');
        out.push_str(&clip(&request.action_summary, ACTION_SUMMARY_LIMIT));
        out.push_str("\n</action>\n");
        if retry {
            out.push_str(
                "\n<format_note>The previous attempt returned no parseable JSON. \
                 Reply with the single JSON object and nothing else: \
                 no reasoning, no code fence, no preamble.\n",
            );
        }
        out.push_str(&self.rules_prompt());
        out
    }

    async fn call_classifier(&self, request: &AutoReviewRequest) -> Result<AutoVerdict, String> {
        self.call_classifier_attempt(request, false).await
    }

    /// 一次分类器请求。`retry` 同时放大输出预算：空响应最常见的成因就是
    /// `stop_reason=MaxTokens`（思考吃掉预算、可见文本为空），第一次用
    /// `CLASSIFIER_MAX_TOKENS`，第二次翻倍给内容留出余地。
    async fn call_classifier_attempt(
        &self,
        request: &AutoReviewRequest,
        retry: bool,
    ) -> Result<AutoVerdict, String> {
        let provider = self
            .classifier
            .as_ref()
            .ok_or_else(|| "未配置分类器".to_string())?;
        let messages = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: self.build_prompt(request, retry),
            }],
        }];
        let system = SystemPrompt::stable_only(crate::prompts::AUTO_CLASSIFIER);
        let max_tokens = if retry {
            CLASSIFIER_MAX_TOKENS * 2
        } else {
            CLASSIFIER_MAX_TOKENS
        };
        let options = RequestOptions::text_only(max_tokens);
        let call = provider.complete(&system, &messages, &[], &options);
        let result = tokio::time::timeout(self.config.timeout, call)
            .await
            .map_err(|_| format!("分类器请求超时（{}s）", self.config.timeout.as_secs()))?
            .map_err(|error| format!("分类器请求失败：{error}"))?;

        if let Ok(mut usage) = self.usage.lock() {
            usage.requests += 1;
            usage.input_tokens += result.input_tokens;
            usage.output_tokens += result.output_tokens;
        }

        let raw = result
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        match parse_classifier_output(&raw) {
            Ok(verdict) => Ok(verdict),
            Err(error) => {
                // 解析失败一律 fail-closed，但**必须留下线索**：只报「无法解析」的话，
                // 用户既改不了模型也改不了提示词，只能看着操作一次次被拦。
                // 所以把截断后的原始输出与 stop_reason 一并带进错误里。
                tracing::warn!(
                    %error,
                    stop_reason = ?result.stop_reason,
                    raw = %clip(&raw, 400),
                    "分类器输出无法解析"
                );
                Err(format!(
                    "{error}（模型返回：{}；stop_reason={:?}）",
                    clip(&raw, 160),
                    result.stop_reason
                ))
            }
        }
    }
}

#[async_trait]
impl AutoGate for AutoGateImpl {
    async fn review(&self, request: &AutoReviewRequest) -> AutoVerdict {
        // 顺序有讲究：两条确定性规则都排在「分类器是否可用」**之前**。
        //
        // 旧实现把 `!config.enabled` 的检查放在最前面，于是配置里关掉分类器之后
        // `curl|bash` / `git reset --hard` 这类 hard_deny 也一并失效——只剩弹窗，
        // 用户按一个 y 就能放过去。停用分类器降级的应当是**语义判定**这一层，
        // 不是安全底线：hard_deny 必须无论端点多慢、配置怎么写都拦得住。
        if let Some((rule, reason)) = self.deterministic_deny(request) {
            self.note_block();
            return AutoVerdict::Deny { rule, reason };
        }

        if let Some(reason) = self.deterministic_allow(request) {
            tracing::trace!(tool = %request.tool, %reason, "auto 确定性放行");
            // 确定性放行必须同样清空连续计数。它与 `apply_verdict(Allow)`
            // 走的是同一个语义——一次被确认安全的动作——之前这里直接 return
            // 绕过了记账，于是 `deny → 只读命令 → deny → 只读命令 → deny`
            // 依然凑满连续阈值。对 GUI 任务尤其致命：截图/列举元素这类**本来
            // 就是**合法的只读调用，却一点都断不了连续计数。
            self.note_human_approval();
            return AutoVerdict::Allow;
        }

        // 分类器不可用不是故障也不是否决：用户自己的配置、或运行期降级都走这里，
        // 交回逐次弹窗。无交互面（headless / 子 Agent）时 `confirm_tool`
        // fail-closed，与直接拒绝等价，不存在静默放行的口子。
        if let Some(cause) = self.classifier_unavailable() {
            return AutoVerdict::AskUser {
                reason: format!("{cause}，该操作需人工确认"),
            };
        }

        match self.call_classifier(request).await {
            Ok(verdict) => self.apply_verdict(verdict),
            // 第一次拿不到判定：换个（加提示、加预算的）输入重试一次。
            Err(_) => match self.call_classifier_attempt(request, true).await {
                Ok(verdict) => {
                    tracing::info!("分类器首次调用失败，重试后拿到判定");
                    self.apply_verdict(verdict)
                }
                // 两次都拿不到判定 → **交回人工**，而不是直接拒绝。
                //
                // 这是本项目里 fail-closed 唯一一处刻意的例外：闸门的判定主体是模型，
                // 模型没答上来时，"拒绝"与"人工确认"对用户的差别只是多按一次 y，却让
                // 一次端点抖动（空响应 / MaxTokens）变成一次操作失败——实测 auto 模式
                // 的拒绝里空响应占了一大片。它也**不该消耗降级预算**（`note_block`）：
                // 模型从未表达否决意图，计成"连续拦截"会让闸门白白退回 manual。
                //
                // 安全侧没有松：AskUser 最终落到 `ToolCtx::confirm_tool`，无交互面
                // （headless / 子 Agent）时它 fail-closed，行为与旧的直接拒绝一致，
                // 不存在静默放行的口子。
                Err(reason) => {
                    tracing::warn!(%reason, "分类器两次都不可用，交回人工确认");
                    AutoVerdict::AskUser {
                        reason: format!("自动判定不可用，已交回人工确认（{reason}）"),
                    }
                }
            },
        }
    }

    fn counters(&self) -> AutoCounters {
        self.counters.lock().map(|c| *c).unwrap_or_default()
    }

    fn should_degrade(&self) -> bool {
        self.should_degrade()
    }

    fn note_human_approval(&self) {
        if let Ok(mut counters) = self.counters.lock() {
            counters.consecutive = 0;
        }
    }

    fn disable_classifier(&self) {
        self.classifier_off.store(true, Ordering::Relaxed);
    }

    fn enable_classifier(&self) {
        self.classifier_off.store(false, Ordering::Relaxed);
        if let Ok(mut counters) = self.counters.lock() {
            // `total` 也必须清：它只增不减，而 `should_degrade` 是
            // `consecutive >= 连续阈值 || total >= 累计阈值`。只清 consecutive
            // 的话，若这次降级是由 total 触发的，`/mode auto` 之后
            // `should_degrade()` **立刻仍为 true**——用户看到的现象是
            // 「明明重新选了 auto，状态栏还是显示已降级」。
            counters.consecutive = 0;
            counters.total = 0;
            counters.degraded = false;
        }
    }

    fn classifier_enabled(&self) -> bool {
        self.classifier_unavailable().is_none()
    }
}

/// 按 `[auto_mode]` 配置段装配闸门。
///
/// 分类器 Provider 的选取：`classifier_profile` 留空就用 `provider`（主模型），
/// 填了就按名从 `config.profiles` 里单独建一个（适合用小模型降本）。按名找不到
/// 或构建失败时**降级为无分类器**而不是 panic——那时 auto 模式退化成"确定性规则
/// + 其余逐次弹窗"，仍然是一个正确（只是更啰嗦）的会话。
pub fn build_gate(config: &wyj_config::Config, provider: Arc<dyn Provider>) -> Arc<dyn AutoGate> {
    let cfg = &config.auto_mode;
    let classifier: Option<Arc<dyn Provider>> = {
        let name = cfg.classifier_profile.trim();
        if name.is_empty() {
            Some(provider)
        } else {
            match config
                .profiles
                .iter()
                .find(|p| p.name == name)
                .and_then(|p| wyj_api::build_provider_from_profile(p, None).ok())
            {
                Some(provider) => Some(provider),
                None => {
                    tracing::warn!(
                        profile = name,
                        "autoMode.classifier_profile 指向的分组不存在或构建失败，\
                         auto 模式退回逐次确认"
                    );
                    None
                }
            }
        }
    };
    Arc::new(AutoGateImpl::new(
        AutoGateConfig {
            enabled: cfg.enabled,
            timeout: Duration::from_secs(cfg.timeout_secs.max(1)),
            max_consecutive_blocks: cfg.max_consecutive_blocks.max(1),
            max_total_blocks: cfg.max_total_blocks.max(1),
            classify_all_shell: cfg.classify_all_shell,
            rules: AutoRuleOverrides {
                hard_deny: cfg.hard_deny.clone(),
                soft_deny: cfg.soft_deny.clone(),
                allow: cfg.allow.clone(),
                environment: cfg.environment.clone(),
            }
            .resolve(),
        },
        classifier,
    ))
}

/// 按字节上限截断并回退到 char boundary，避免中文/emoji 被切在半个字符上。
fn clip(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut out = String::with_capacity(max_bytes + 3);
    out.push_str(&text[..crate::textutil::floor_char_boundary(text, max_bytes)]);
    out.push('…');
    out
}

fn is_within_cwd(target: &Path, cwd: &Path) -> bool {
    let cwd = crate::project::project_root(cwd);
    let target = target.to_path_buf();
    let cwd = cwd.to_path_buf();
    // 任一侧解析失败时退化为字面比较：解析不出真实路径不代表就一定在工作目录内，
    // 此时返回 false 交给分类器判，而不是直接放行。
    let normalized =
        |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let target = normalized(&target);
    let cwd = normalized(&cwd);
    target == cwd || target.starts_with(&cwd)
}

#[derive(Debug, Clone, serde::Deserialize)]
struct ClassifierOutput {
    verdict: String,
    #[serde(default)]
    rule: String,
    #[serde(default)]
    reason: String,
}

/// 解析分类器输出。容忍代码围栏与前后废话，只认第一个完整 JSON 对象。
///
/// 解析不出来就是 fail-closed 的触发条件之一，所以这里返回 `Err` 而不是
/// 「猜一个 allow」。
fn parse_classifier_output(raw: &str) -> Result<AutoVerdict, String> {
    let trimmed = raw.trim();
    let start = trimmed.find('{').ok_or("分类器未返回 JSON 对象")?;
    let end = trimmed.rfind('}').ok_or("分类器 JSON 不完整")?;
    if end <= start {
        return Err("分类器 JSON 不完整".to_string());
    }
    let parsed: ClassifierOutput = serde_json::from_str(&trimmed[start..=end])
        .map_err(|error| format!("分类器输出无法解析：{error}"))?;
    let reason = if parsed.reason.trim().is_empty() {
        "分类器未说明理由".to_string()
    } else {
        parsed.reason.trim().to_string()
    };
    match parsed.verdict.trim().to_ascii_lowercase().as_str() {
        "allow" | "approve" => Ok(AutoVerdict::Allow),
        "deny" | "block" | "reject" => Ok(AutoVerdict::Deny {
            rule: if parsed.rule.trim().is_empty() {
                "classifier".to_string()
            } else {
                parsed.rule.trim().to_string()
            },
            reason,
        }),
        other => Err(format!("分类器返回了未知判定：{other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use wyj_api::{
        provider::EventStream,
        types::{CompletionResult, StopReason, ToolDefinition},
    };

    /// 假分类器：按工具摘要里的标记返回预设结果，或直接失败。
    ///
    /// `alt_reply` 只在第二次及以后的调用生效，用来模拟"第一次吐了坏输出、
    /// 重试才给正经判定"这种真实端点抖动。
    struct FakeProvider {
        reply: Option<String>,
        alt_reply: Option<String>,
        fail: bool,
        delay: Duration,
        calls: AtomicU32,
        seen_max_tokens: Mutex<Vec<u32>>,
    }

    impl FakeProvider {
        fn allow() -> Arc<Self> {
            Arc::new(Self {
                reply: Some(r#"{"verdict":"allow","rule":"","reason":"in scope"}"#.into()),
                alt_reply: None,
                fail: false,
                delay: Duration::ZERO,
                calls: AtomicU32::new(0),
                seen_max_tokens: Mutex::new(Vec::new()),
            })
        }

        /// 第一次返回空文本（等价于 `stop_reason=MaxTokens` + 可见文本为空），
        /// 第二次返回给定回复。
        fn empty_then(reply: &str) -> Arc<Self> {
            Arc::new(Self {
                reply: None,
                alt_reply: Some(reply.to_string()),
                fail: false,
                delay: Duration::ZERO,
                calls: AtomicU32::new(0),
                seen_max_tokens: Mutex::new(Vec::new()),
            })
        }

        fn delayed(reply: &str, delay: Duration) -> Arc<Self> {
            Arc::new(Self {
                reply: Some(reply.to_string()),
                alt_reply: None,
                fail: false,
                delay,
                calls: AtomicU32::new(0),
                seen_max_tokens: Mutex::new(Vec::new()),
            })
        }

        fn deny(reason: &str) -> Arc<Self> {
            Arc::new(Self {
                reply: Some(format!(
                    r#"{{"verdict":"deny","rule":"soft_deny","reason":"{reason}"}}"#
                )),
                alt_reply: None,
                fail: false,
                delay: Duration::ZERO,
                calls: AtomicU32::new(0),
                seen_max_tokens: Mutex::new(Vec::new()),
            })
        }

        fn broken() -> Arc<Self> {
            Arc::new(Self {
                reply: None,
                alt_reply: None,
                fail: true,
                delay: Duration::ZERO,
                calls: AtomicU32::new(0),
                seen_max_tokens: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl Provider for FakeProvider {
        async fn complete(
            &self,
            _system: &SystemPrompt<'_>,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            options: &RequestOptions,
        ) -> anyhow::Result<CompletionResult> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Ok(mut seen) = self.seen_max_tokens.lock() {
                seen.push(options.max_tokens);
            }
            if self.fail {
                anyhow::bail!("boom");
            }
            // alt_reply 只在重试（第二次及以后）生效；没给就照常重复第一次的回复。
            let retry_reply = if self.calls.load(Ordering::SeqCst) > 1 {
                self.alt_reply.clone()
            } else {
                None
            };
            let text = retry_reply
                .or_else(|| self.reply.clone())
                .unwrap_or_default();
            tokio::time::sleep(self.delay).await;
            Ok(CompletionResult {
                content: vec![ContentBlock::Text { text }],
                stop_reason: StopReason::EndTurn,
                input_tokens: 10,
                output_tokens: 5,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
            })
        }

        async fn stream(
            &self,
            _system: &SystemPrompt<'_>,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            _options: &RequestOptions,
        ) -> anyhow::Result<EventStream> {
            // 分类器只走 `complete`；真走到 `stream` 说明测试假设变了，
            // 让它当场炸掉而不是静默返回一个空流。
            unreachable!("分类器只使用 complete 路径")
        }
    }

    fn bash_request(command: &str) -> AutoReviewRequest {
        AutoReviewRequest {
            tool: "Bash".to_string(),
            action_summary: command.to_string(),
            input: serde_json::json!({ "command": command }),
            cwd: std::env::temp_dir(),
            user_goal: "run the tests".to_string(),
        }
    }

    fn gate_with(config: AutoGateConfig, provider: Option<Arc<dyn Provider>>) -> AutoGateImpl {
        AutoGateImpl::new(config, provider)
    }

    #[test]
    fn classifier_budget_covers_thinking_on_adaptive_endpoints() {
        // 防回归：曾把这里设成 256，理由是"判定只需一个极短的 JSON"。但
        // adaptive thinking 端点在 thinking 字段缺省时自行开启思考，且物理上
        // 拒绝关闭（thinking.type=disabled 会被 400 拒），256 会被思考吃光 →
        // stop_reason=MaxTokens + 可见文本为空 → fail-closed → 命令被随机拒绝。
        // 实测思考量 1105–2395 字符，下限取 2048。
        const {
            assert!(
                CLASSIFIER_MAX_TOKENS >= 2048,
                "分类器预算不能低于 2048，否则 thinking 端点上会被思考吃空"
            )
        };
    }

    #[test]
    fn default_timeout_outlives_a_thinking_classifier_roundtrip() {
        // 同一次故障的第二个出口：超时与"解析失败"共用 fail-closed，用户看到
        // 的都是"命令被莫名拒绝"。实测单次往返 3.4–5.5s，15s 会被抖动撞到。
        assert!(
            AutoGateConfig::default().timeout >= Duration::from_secs(30),
            "分类器默认超时不能低于 30s，否则网络抖动就会把正常判定变成拒绝"
        );
    }

    #[test]
    fn parse_accepts_fenced_and_noisy_output() {
        let verdict = parse_classifier_output(
            "```json\n{\"verdict\":\"deny\",\"rule\":\"soft_deny\",\"reason\":\"超出请求范围\"}\n```",
        )
        .unwrap();
        assert!(matches!(verdict, AutoVerdict::Deny { .. }));

        let verdict =
            parse_classifier_output("Sure! {\"verdict\":\"allow\"} hope that helps").unwrap();
        assert_eq!(verdict, AutoVerdict::Allow);
    }

    #[test]
    fn parse_rejects_garbage_as_error() {
        assert!(parse_classifier_output("I think it is fine").is_err());
        assert!(parse_classifier_output("{\"verdict\":\"maybe\"}").is_err());
        assert!(parse_classifier_output("").is_err());
    }

    #[tokio::test]
    async fn hard_deny_blocks_without_calling_model() {
        let provider = FakeProvider::allow();
        let gate = gate_with(AutoGateConfig::default(), Some(provider.clone()));
        let verdict = gate.review(&bash_request("rm -rf /")).await;
        match verdict {
            AutoVerdict::Deny { rule, .. } => assert_eq!(rule, "critical_path_removal"),
            other => panic!("应被确定性规则拦下，实际 {other:?}"),
        }
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            0,
            "确定性拦截不应发模型请求"
        );
    }

    #[tokio::test]
    async fn read_only_shell_skips_model() {
        let provider = FakeProvider::deny("不该走到这里");
        let gate = gate_with(AutoGateConfig::default(), Some(provider.clone()));
        assert_eq!(
            gate.review(&bash_request("git status")).await,
            AutoVerdict::Allow
        );
        assert_eq!(
            gate.review(&bash_request("cargo test -p wyj-core")).await,
            AutoVerdict::Allow
        );
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn classify_all_shell_forces_model_review() {
        let provider = FakeProvider::deny("命令超出请求范围");
        let config = AutoGateConfig {
            classify_all_shell: true,
            ..Default::default()
        };
        let gate = gate_with(config, Some(provider.clone()));
        assert!(matches!(
            gate.review(&bash_request("git status")).await,
            AutoVerdict::Deny { .. }
        ));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn unknown_command_goes_to_classifier() {
        let provider = FakeProvider::allow();
        let gate = gate_with(AutoGateConfig::default(), Some(provider.clone()));
        assert_eq!(
            gate.review(&bash_request("npm run deploy:staging")).await,
            AutoVerdict::Allow
        );
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn classifier_failure_retries_then_falls_back_to_human() {
        // 回归：曾经两次都是硬拒（Deny{classifier_unavailable}），一次端点抖动
        // 就算"连续拦截"，几次抖动就把闸门打到 degraded 退回 manual。
        let provider = FakeProvider::broken();
        let gate = gate_with(AutoGateConfig::default(), Some(provider.clone()));
        match gate.review(&bash_request("npm run deploy:staging")).await {
            AutoVerdict::AskUser { reason } => {
                assert!(reason.contains("自动判定不可用"), "实际 {reason}");
            }
            other => panic!("分类器两次都失败应交回人工，实际 {other:?}"),
        }
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2, "应当重试一次");
        assert_eq!(
            gate.counters().total,
            0,
            "模型没给出判定不算拦截，不该消耗降级预算"
        );
        assert!(!gate.should_degrade());
    }

    #[tokio::test]
    async fn empty_classifier_response_is_retried_and_can_recover() {
        // 最常见的坏输出形态：stop_reason=MaxTokens + 可见文本为空。
        let provider = FakeProvider::empty_then(r#"{"verdict":"allow","rule":"","reason":"ok"}"#);
        let gate = gate_with(AutoGateConfig::default(), Some(provider.clone()));
        assert_eq!(
            gate.review(&bash_request("npm run deploy:staging")).await,
            AutoVerdict::Allow,
            "重试拿到判定就不该拒绝"
        );
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
        let seen = provider.seen_max_tokens.lock().unwrap().clone();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0], CLASSIFIER_MAX_TOKENS);
        assert_eq!(
            seen[1],
            CLASSIFIER_MAX_TOKENS * 2,
            "重试必须放大输出预算，否则 MaxTokens 会原样复现"
        );
    }

    #[tokio::test]
    async fn classifier_timeout_falls_back_to_human() {
        let provider = FakeProvider::delayed(r#"{"verdict":"allow"}"#, Duration::from_millis(200));
        let config = AutoGateConfig {
            timeout: Duration::from_millis(20),
            ..Default::default()
        };
        let gate = gate_with(config, Some(provider.clone()));
        assert!(matches!(
            gate.review(&bash_request("npm run deploy:staging")).await,
            AutoVerdict::AskUser { .. }
        ));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn disabled_classifier_falls_back_to_human() {
        let gate = gate_with(
            AutoGateConfig {
                enabled: false,
                ..Default::default()
            },
            Some(FakeProvider::allow()),
        );
        assert!(matches!(
            gate.review(&bash_request("npm run deploy:staging")).await,
            AutoVerdict::AskUser { .. }
        ));
        assert_eq!(gate.counters().total, 0, "交回人工不算拦截");
    }

    /// 停用分类器降级的是**语义判定**，不是安全底线。
    ///
    /// 旧实现把 `!config.enabled` 放在 `review()` 最前面，于是关掉分类器之后
    /// `rm -rf /` 只需要用户按一个 y 就能过去。这里把两条路径（配置关 /
    /// 运行期停）一起钉住：hard_deny 仍然无条件拦，其余交回人工。
    #[tokio::test]
    async fn disabling_the_classifier_never_drops_the_hard_deny_layer() {
        for off in [
            AutoGateConfig {
                enabled: false,
                ..Default::default()
            },
            AutoGateConfig::default(),
        ] {
            let provider = FakeProvider::allow();
            let gate = gate_with(off, Some(provider.clone()));
            let runtime_off = gate.classifier_enabled();
            if runtime_off {
                gate.disable_classifier();
            }
            assert!(!gate.classifier_enabled());

            // hard_deny：照拦，且不发模型请求。
            match gate.review(&bash_request("rm -rf /")).await {
                AutoVerdict::Deny { rule, .. } => assert_eq!(rule, "critical_path_removal"),
                other => panic!("停用分类器后仍必须硬拦，实际 {other:?}"),
            }
            // 只读 allow：照放。
            assert_eq!(
                gate.review(&bash_request("git status")).await,
                AutoVerdict::Allow
            );
            // 语义层：交回人工，不拒绝、也不计降级预算。
            assert!(matches!(
                gate.review(&bash_request("npm run deploy:staging")).await,
                AutoVerdict::AskUser { .. }
            ));
            assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
            assert_eq!(gate.counters().total, 1, "只有 hard_deny 计入拦截预算");
        }
    }

    /// `enable_classifier` 必须能真正恢复，而不只是把停用位翻回去——连续计数
    /// 与 `degraded` 标记一并清零，否则用户切回 auto 后闸门会立刻再次降级。
    #[tokio::test]
    async fn re_enabling_the_classifier_clears_the_degradation_budget() {
        let gate = gate_with(AutoGateConfig::default(), Some(FakeProvider::deny("越界")));
        for _ in 0..3 {
            gate.review(&bash_request("npm run deploy:prod")).await;
        }
        assert!(gate.should_degrade());

        gate.disable_classifier();
        gate.enable_classifier();
        assert!(gate.classifier_enabled());
        assert!(!gate.should_degrade());
        assert_eq!(gate.counters().consecutive, 0);
        // 恢复后确实重新走分类器（deny 而非 AskUser），证明没停在降级态。
        assert!(matches!(
            gate.review(&bash_request("npm run deploy:prod")).await,
            AutoVerdict::Deny { .. }
        ));
    }

    #[tokio::test]
    async fn consecutive_blocks_trigger_degradation() {
        let gate = gate_with(
            AutoGateConfig::default(),
            Some(FakeProvider::deny("超出请求范围")),
        );
        for _ in 0..3 {
            assert!(matches!(
                gate.review(&bash_request("npm run deploy:prod")).await,
                AutoVerdict::Deny { .. }
            ));
        }
        let counters = gate.counters();
        assert_eq!(counters.consecutive, 3);
        assert_eq!(counters.total, 3);
        assert!(counters.degraded);
        assert!(gate.should_degrade());
    }

    #[tokio::test]
    async fn total_blocks_trigger_degradation_across_runs() {
        let gate = AutoGateImpl::new(
            AutoGateConfig {
                max_consecutive_blocks: 100,
                max_total_blocks: 4,
                ..Default::default()
            },
            Some(FakeProvider::deny("超出请求范围")),
        );
        for _ in 0..2 {
            gate.review(&bash_request("npm run deploy:prod")).await;
            // 人工批准一次会清零连续计数，累计计数则保留。
            gate.note_human_approval();
        }
        assert_eq!(gate.counters().consecutive, 0);
        assert_eq!(gate.counters().total, 2);
        gate.review(&bash_request("npm run deploy:prod")).await;
        gate.note_human_approval();
        assert!(!gate.counters().degraded);
        gate.review(&bash_request("npm run deploy:prod")).await;
        assert!(gate.counters().degraded);
    }

    #[tokio::test]
    async fn local_file_write_is_allowed_without_model() {
        let provider = FakeProvider::deny("不该走到这里");
        let gate = gate_with(AutoGateConfig::default(), Some(provider.clone()));
        let cwd = std::env::temp_dir();
        let request = AutoReviewRequest {
            tool: "Write".to_string(),
            action_summary: format!("{}/notes.md", cwd.display()),
            input: serde_json::json!({ "file_path": cwd.join("notes.md").to_string_lossy() }),
            cwd: cwd.clone(),
            user_goal: "write notes".to_string(),
        };
        assert_eq!(gate.review(&request).await, AutoVerdict::Allow);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn overrides_splice_defaults_in_place() {
        let overrides = AutoRuleOverrides {
            allow: vec!["$defaults".into(), "允许推送到 staging".into()],
            ..Default::default()
        };
        let resolved = overrides.resolve();
        assert_eq!(resolved.allow.len(), DEFAULT_ALLOW.len() + 1);
        assert_eq!(resolved.allow.last().unwrap(), "允许推送到 staging");
        assert_eq!(resolved.hard_deny.len(), DEFAULT_HARD_DENY.len());
    }
}
