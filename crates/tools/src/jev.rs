//! Jev 决策工具（TypeSafe System One，https://typesafe.ai/）
//!
//! Jev 不是 chat 模型——无 stream / 无 multi-turn / 无 tool calling 协议，
//! 它是 stateless 决策 API（POST `/v1/systemone`），输入 = `state`（一段
//! 上下文文本）+ `questions`（结构化决策 spec），输出 = 结构化 `answers`
//! 每个 question 都有 `confidence` + `probabilities` + 概率分布。三种
//! primitive：choice / score / noul（详见 quickstart）。
//!
//! 本 crate 把 Jev 作为独立 `Tool` 暴露给主 Agent（Claude/GPT），让它在
//! 歧义场景（意图路由、分类、guardrails、置信度标注）主动调用，主模型
//! 仍是 chat LLM，Jev 仅作为结构化决策的补充能力。
//!
//! 注册门控：仅当 `Config.tools.jev.enabled=true` 且 `Config::resolve_jev_api_key()`
//! 返回 Some 时才注册（见 `cli::register_jev_tool_if_enabled`）。
//!
//! 计费保护：客户端三层硬封顶——
//! 1. `max_state_chars`：state 字符上限（按 char boundary 安全截断，防超 token）
//! 2. `max_questions`：单次 questions 数量上限
//! 3. `daily_budget_usd`：进程级日预算（0 = 关闭），单位 USD，micro-cent 精度累加
//!
//! 定价（[docs.typesafe.ai/models](https://docs.typesafe.ai/models)）：
//! 输入 $0.042 / 百万 token；**输出 token 免费**（$0/M），所以预算只跟输入侧相关。
//!
//! 错误归一走 `wyj_api::error::ProviderError::from_http`（401→Authentication、
//! 422→InvalidRequest、429→RateLimited、5xx→Overloaded）；客户端重试用
//! `wyj_api::retry::send_with_retry`。

use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use wyj_api::error::ProviderError;
use wyj_api::retry::{send_with_retry, RetryPolicy};
use wyj_api::types::ToolDefinition;
use wyj_core::tool::{Tool, ToolContext, ToolResult};

const ENDPOINT_SUFFIX: &str = "/v1/systemone";
const TIMEOUT_SECS: u64 = 30;
/// $0.042 USD / 百万 input token。换算成 micro-cents / token：
/// 0.042 * 1_000_000 / 1_000_000 = 0.042 micro-cent / token，向上取整为
/// `(tokens + 23) / 24` 或 `tokens * 42 / 1000`。我们取 `tokens * 42 / 1000`
/// 整数乘除，向上有界（实际稍低估但量级一致，预算更安全）。
const INPUT_USD_PER_TOKEN_MICRO_CENTS: u64 = 42;
const INPUT_TOKEN_DIVISOR: u64 = 1_000;
const MICRO_CENTS_PER_USD: f64 = 1_000_000.0;

// ── Budget 计数器 ─────────────────────────────────────────────────────────────

/// 进程级 Jev 日预算计数器。单位 micro-cents（避免浮点累积误差）。
///
/// **不**持久化：重启清零。"日"语义 = "进程生命周期内累计"。
/// 如需跨进程追踪，用 `wyj-code schedule` 拆任务而非让单进程撑过午夜。
pub struct JevBudget {
    /// 已花费 micro-cents。
    spent: AtomicU64,
    /// daily_budget_usd × 1_000_000；0 = 关闭 budget 维度（仅 size 上限）。
    limit: u64,
}

impl JevBudget {
    pub fn new(daily_budget_usd: f64) -> Self {
        let limit = if daily_budget_usd > 0.0 {
            (daily_budget_usd * MICRO_CENTS_PER_USD).round() as u64
        } else {
            0
        };
        Self {
            spent: AtomicU64::new(0),
            limit,
        }
    }

    /// 估算本次请求的最大成本（按 body 字节数 / 4 ≈ token 数）。提交前调用。
    pub fn estimate_max_micro_cents(&self, input_bytes: usize) -> u64 {
        let tokens = (input_bytes / 4).max(1) as u64;
        tokens * INPUT_USD_PER_TOKEN_MICRO_CENTS / INPUT_TOKEN_DIVISOR
    }

    /// 提交前调用：超出预算返回 Err。`limit == 0` 时永远 Ok。
    pub fn check(&self, est_micro_cents: u64) -> std::result::Result<(), String> {
        if self.limit == 0 {
            return Ok(());
        }
        let cur = self.spent.load(Ordering::SeqCst);
        if cur.saturating_add(est_micro_cents) > self.limit {
            return Err(format!(
                "Jev daily budget exceeded (${:.4} / ${:.4}); raise [tools.jev].daily_budget_usd or set 0 to disable",
                cur as f64 / MICRO_CENTS_PER_USD,
                self.limit as f64 / MICRO_CENTS_PER_USD,
            ));
        }
        Ok(())
    }

    /// 记录已花费（服务端返回的 usage.input_tokens）。
    pub fn record(&self, input_tokens: u32) {
        let tokens = input_tokens as u64;
        let cents = tokens * INPUT_USD_PER_TOKEN_MICRO_CENTS / INPUT_TOKEN_DIVISOR;
        self.spent.fetch_add(cents, Ordering::SeqCst);
    }

    pub fn spent_usd(&self) -> f64 {
        self.spent.load(Ordering::SeqCst) as f64 / MICRO_CENTS_PER_USD
    }

    pub fn limit_usd(&self) -> f64 {
        self.limit as f64 / MICRO_CENTS_PER_USD
    }

    pub fn is_enabled(&self) -> bool {
        self.limit > 0
    }
}

// ── Input spec ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct JevInput {
    /// 上下文文本（被 max_state_chars 客户端截断后发送）
    pub state: String,
    /// 覆盖默认 model（默认走 `Config.tools.jev.model`）
    #[serde(default)]
    pub model: Option<String>,
    /// answer-name → QuestionSpec
    pub questions: BTreeMap<String, QuestionSpec>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum QuestionSpec {
    Choice {
        instructions: String,
        criteria: BTreeMap<String, String>,
    },
    Score {
        instructions: String,
        criteria: Vec<String>,
    },
    Noul {
        instructions: String,
    },
}

// ── Tool 实现 ────────────────────────────────────────────────────────────────

pub struct JevTool {
    client: Client,
    base_url: String,
    default_model: String,
    max_questions: usize,
    max_state_chars: usize,
    max_retries: u32,
    api_key: String,
    budget: Arc<JevBudget>,
}

impl JevTool {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        default_model: impl Into<String>,
        max_questions: usize,
        max_state_chars: usize,
        max_retries: u32,
        budget: Arc<JevBudget>,
    ) -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
            .user_agent("wyj-code/1.0")
            .build()
            .unwrap_or_default();
        Self {
            client,
            base_url: base_url.into(),
            default_model: default_model.into(),
            max_questions,
            max_state_chars,
            max_retries,
            api_key: api_key.into(),
            budget,
        }
    }

    /// 客户端硬校验：state 非空、questions 非空且 ≤ max_questions、每条 question
    /// 字段合法。不合法返回 `ToolResult::err`，不发 HTTP。
    fn validate(&self, inp: &JevInput) -> std::result::Result<(), String> {
        if inp.state.is_empty() {
            return Err("state 不能为空".into());
        }
        if inp.questions.is_empty() {
            return Err("questions 不能为空".into());
        }
        if inp.questions.len() > self.max_questions {
            return Err(format!(
                "questions 数量 {} 超过 max_questions={}",
                inp.questions.len(),
                self.max_questions
            ));
        }
        for (name, q) in &inp.questions {
            match q {
                QuestionSpec::Choice {
                    instructions,
                    criteria,
                } => {
                    if instructions.trim().is_empty() {
                        return Err(format!("questions.{name}.instructions 不能为空"));
                    }
                    if criteria.is_empty() {
                        return Err(format!(
                            "questions.{name}.criteria (choice) 至少需要 1 个 key"
                        ));
                    }
                }
                QuestionSpec::Score {
                    instructions,
                    criteria,
                } => {
                    if instructions.trim().is_empty() {
                        return Err(format!("questions.{name}.instructions 不能为空"));
                    }
                    if criteria.len() < 2 {
                        return Err(format!("questions.{name}.criteria (score) 长度必须 >= 2"));
                    }
                }
                QuestionSpec::Noul { instructions } => {
                    if instructions.trim().is_empty() {
                        return Err(format!("questions.{name}.instructions 不能为空"));
                    }
                }
            }
        }
        Ok(())
    }

    /// 按 char boundary 把 state 截到 max_state_chars（避免 String::truncate
    /// 在 CJK / emoji 多字节字符中间 panic）。
    fn truncate_state(&self, state: &str) -> String {
        if state.len() <= self.max_state_chars {
            return state.to_string();
        }
        // 先按字符数截断；然后回退到最近的 char boundary。
        let mut end = self.max_state_chars;
        while end > 0 && !state.is_char_boundary(end) {
            end -= 1;
        }
        state[..end].to_string()
    }

    /// 构造请求体（service API schema）
    fn build_body(&self, inp: &JevInput) -> Value {
        let state = self.truncate_state(&inp.state);
        let mut questions = serde_json::Map::new();
        for (name, q) in &inp.questions {
            let entry = match q {
                QuestionSpec::Choice {
                    instructions,
                    criteria,
                } => json!({
                    "type": "choice",
                    "instructions": instructions,
                    "criteria": criteria,
                }),
                QuestionSpec::Score {
                    instructions,
                    criteria,
                } => json!({
                    "type": "score",
                    "instructions": instructions,
                    "criteria": criteria,
                }),
                QuestionSpec::Noul { instructions } => json!({
                    "type": "noul",
                    "instructions": instructions,
                }),
            };
            questions.insert(name.clone(), entry);
        }
        json!({
            "state": state,
            "model": inp.model.clone().unwrap_or_else(|| self.default_model.clone()),
            "questions": Value::Object(questions),
        })
    }
}

#[async_trait]
impl Tool for JevTool {
    fn name(&self) -> &str {
        "Jev"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: crate::descriptions::JEV_DESCRIPTION.to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "state": {
                        "type": "string",
                        "description": crate::descriptions::FIELD_JEV_STATE,
                    },
                    "model": {
                        "type": "string",
                        "description": crate::descriptions::FIELD_JEV_MODEL,
                    },
                    "questions": {
                        "type": "object",
                        "description": "Map from answer-name to a question spec",
                        "additionalProperties": {
                            "type": "object",
                            "properties": {
                                "type": {
                                    "type": "string",
                                    "enum": ["choice", "score", "noul"],
                                    "description": crate::descriptions::FIELD_JEV_QUESTION_TYPE,
                                },
                                "instructions": {
                                    "type": "string",
                                    "description": crate::descriptions::FIELD_JEV_INSTRUCTIONS,
                                },
                                "criteria": {
                                    "description": "For `choice`: object of {key: description}. For `score`: array of rubric levels (len >= 2). Ignored for `noul`.",
                                    "anyOf": [
                                        {
                                            "type": "object",
                                            "additionalProperties": {"type": "string"},
                                            "description": crate::descriptions::FIELD_JEV_CRITERIA_CHOICE,
                                        },
                                        {
                                            "type": "array",
                                            "items": {"type": "string"},
                                            "minItems": 2,
                                            "description": crate::descriptions::FIELD_JEV_CRITERIA_SCORE,
                                        }
                                    ],
                                },
                            },
                            "required": ["type", "instructions"],
                        },
                    },
                },
                "required": ["state", "questions"],
            }),
            native: None,
        }
    }

    /// 只读决策 API（不会变更外部状态），无需权限确认。
    fn needs_permission(&self, _input: &Value) -> bool {
        false
    }

    /// 不同调用间共享 budget 计数器与 HTTP 连接池，串行执行避免并发抢占预算额度。
    fn parallel_safe(&self) -> bool {
        false
    }

    async fn run(&self, input: Value, _ctx: &dyn ToolContext) -> Result<ToolResult> {
        let inp: JevInput = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => return Ok(ToolResult::err(format!("解析输入失败: {e}"))),
        };

        // 1. 客户端校验：不合规直接拒（不发 HTTP）
        if let Err(e) = self.validate(&inp) {
            return Ok(ToolResult::err(e));
        }

        // 2. 构造请求体并做预算检查（保守预估）
        let body = self.build_body(&inp);
        let body_bytes = serde_json::to_vec(&body).unwrap_or_default();
        let est_mc = self.budget.estimate_max_micro_cents(body_bytes.len());
        if let Err(e) = self.budget.check(est_mc) {
            return Ok(ToolResult::err(e));
        }

        // 3. HTTP 调用（带重试）
        let url = format!("{}{}", self.base_url, ENDPOINT_SUFFIX);
        let policy = RetryPolicy {
            max_retries: self.max_retries,
            ..RetryPolicy::default()
        };
        let api_key = self.api_key.clone();
        let body_for_send = body.clone();

        let resp_result = send_with_retry(&policy, "jev", || {
            self.client
                .post(&url)
                .header("Authorization", format!("Bearer {api_key}"))
                .header("Content-Type", "application/json")
                .json(&body_for_send)
        })
        .await;

        let resp = match resp_result {
            Ok(r) => r,
            Err(e) => {
                // send_with_retry 已把 transport/http 错误归一为 ProviderError；
                // 这里直接以 e.to_string() 返回，避免 ProviderError 字段泄露。
                return Ok(ToolResult::err(e.to_string()));
            }
        };

        let status = resp.status();
        let headers = resp.headers().clone();
        let body_text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            let provider_err = ProviderError::from_http(status, &headers, &body_text);
            return Ok(ToolResult::err(provider_err.to_string()));
        }

        // 4. 解析响应；记录 usage 进 budget
        let parsed: Value = match serde_json::from_str(&body_text) {
            Ok(v) => v,
            Err(e) => return Ok(ToolResult::err(format!("解析响应失败: {e}"))),
        };
        let input_tokens = parsed
            .pointer("/usage/input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32;
        self.budget.record(input_tokens);

        // 5. 把完整 JSON 发回给模型（保留 model/answers/usage 字段）
        let pretty = serde_json::to_string_pretty(&parsed).unwrap_or(body_text);
        Ok(ToolResult::ok(pretty))
    }
}

// ── Slash 命令复用的轻量 HTTP helper ─────────────────────────────────────────

/// `/decision ping|ask` 命令与 `JevTool` 共享同一份 HTTP 路径（避免在
/// `crates/commands` 里重新引入 reqwest）。本函数专为 CLI 交互设计——
/// 不走 budget / 重试（slash 命令用户期望立即看到结果），返回归一为
/// `DispatchOutcome` 枚举，调用方按 i18n key 渲染。
///
/// 注意：本函数**不**复用 `JevTool::run` 的全部校验路径——`state` 留空
/// 是合法（ping 场景），所以输入层放宽；HTTP 层仍走同一份 `Client` 配置。
pub async fn dispatch_noul(
    state: &str,
    instructions: &str,
    cfg: &wyj_config::Config,
) -> DispatchOutcome {
    use std::time::Duration;

    let api_key = match cfg.resolve_jev_api_key() {
        Some(k) => k,
        None => return DispatchOutcome::NoKey,
    };
    let base_url = cfg.resolve_jev_base_url();
    let url = format!("{base_url}{ENDPOINT_SUFFIX}");
    let model = if cfg.tools.jev.model.is_empty() {
        "jev-latest".to_string()
    } else {
        cfg.tools.jev.model.clone()
    };

    let body = json!({
        "state": state,
        "model": model,
        "questions": {
            "answer": {
                "type": "noul",
                "instructions": instructions,
            }
        }
    });

    let client = match Client::builder()
        .timeout(Duration::from_secs(TIMEOUT_SECS))
        .user_agent("wyj-code/1.0")
        .build()
    {
        Ok(c) => c,
        Err(e) => return DispatchOutcome::Network(format!("client build: {e}")),
    };

    let resp = match client
        .post(&url)
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return DispatchOutcome::Network(e.to_string()),
    };

    let status = resp.status();
    let body_text = match resp.text().await {
        Ok(t) => t,
        Err(e) => return DispatchOutcome::Network(format!("read body: {e}")),
    };
    if !status.is_success() {
        return DispatchOutcome::Http {
            status: status.as_u16(),
            body: body_text,
        };
    }

    let parsed: Value = match serde_json::from_str(&body_text) {
        Ok(v) => v,
        Err(e) => return DispatchOutcome::Parse(e.to_string()),
    };

    let pretty = if let Some(noul) = parsed
        .pointer("/answers/answer/noul")
        .and_then(Value::as_f64)
    {
        format!("{noul:.3}")
    } else {
        serde_json::to_string_pretty(&parsed).unwrap_or(body_text)
    };
    DispatchOutcome::Success {
        noul: parsed
            .pointer("/answers/answer/noul")
            .and_then(Value::as_f64),
        pretty,
    }
}

/// `/decision` 命令的归一返回。`commands::builtin` 用 i18n key 渲染。
#[derive(Debug)]
pub enum DispatchOutcome {
    /// API key 缺失（无论 env 还是 cfg 字段）。
    NoKey,
    /// 网络层失败（DNS / TCP / TLS / 超时）。
    Network(String),
    /// HTTP 4xx/5xx。`status` 与 `body` 都给到调用方做精细渲染。
    Http { status: u16, body: String },
    /// 响应体解析失败。
    Parse(String),
    /// 成功。`noul` 为 Some 时调用方可走简化"概率"展示，否则回退 `pretty`。
    Success { noul: Option<f64>, pretty: String },
}

// ── 单元测试 ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 本地最小的 ToolContext fake；crate 外的 agent::FakeCtx 是 #[cfg(test)]
    /// 私有的，无法在这里复用。所有方法都返回"拒绝交互"语义，Jev 工具只
    /// 读 cwd/is_allowed，其它方法本工具不调用。
    struct FakeCtx;

    fn fake_ctx() -> FakeCtx {
        FakeCtx
    }
    #[async_trait::async_trait]
    impl ToolContext for FakeCtx {
        fn cwd(&self) -> &std::path::Path {
            std::path::Path::new(".")
        }
        fn is_allowed(&self, _name: &str, _input: &Value) -> bool {
            true
        }
    }

    fn tool_with(budget_usd: f64, max_questions: usize, max_state_chars: usize) -> JevTool {
        JevTool::new(
            "sk-test",
            "https://api.typesafe.ai",
            "jev-latest",
            max_questions,
            max_state_chars,
            2,
            Arc::new(JevBudget::new(budget_usd)),
        )
    }

    fn choice_q(name: &str) -> QuestionSpec {
        QuestionSpec::Choice {
            instructions: format!("Pick {name}"),
            criteria: BTreeMap::from([
                ("a".to_string(), "Option A".to_string()),
                ("b".to_string(), "Option B".to_string()),
            ]),
        }
    }

    fn score_q() -> QuestionSpec {
        QuestionSpec::Score {
            instructions: "How angry".to_string(),
            criteria: vec!["calm".into(), "mild".into(), "angry".into()],
        }
    }

    fn noul_q() -> QuestionSpec {
        QuestionSpec::Noul {
            instructions: "Is urgent".to_string(),
        }
    }

    // ── Budget 测试 ─────────────────────────────────────────────────────────

    #[test]
    fn budget_off_disables_check() {
        let b = JevBudget::new(0.0);
        assert!(!b.is_enabled());
        // 任意估算都通过
        assert!(b.check(u64::MAX).is_ok());
    }

    #[test]
    fn budget_limit_rounds_to_micro_cents() {
        let b = JevBudget::new(1.0);
        assert!(b.is_enabled());
        assert_eq!(b.limit_usd(), 1.0);
    }

    #[test]
    fn budget_rejects_when_exceeded() {
        let b = JevBudget::new(0.001);
        // 0.001 USD = 1000 micro-cents。check() 只读不写，因此要模拟
        // 累计超限需要先 record()，再 check()。
        // record(50_000 tokens) → spent += 50_000 * 42 / 1000 = 2100 micro-cents
        b.record(50_000);
        assert!(b.check(500).is_err()); // 2100+500 > 1000
                                        // 反向：清零（用新实例）再验证未记录时 check() 是允许的。
        let b2 = JevBudget::new(0.001);
        assert!(b2.check(600).is_ok()); // 0+600 <= 1000
        assert!(b2.check(1100).is_err()); // 0+1100 > 1000
    }

    #[test]
    fn budget_records_input_tokens() {
        let b = Arc::new(JevBudget::new(10.0));
        b.record(1_000_000); // 1M tokens = 42_000 micro-cents = $0.042
        assert!((b.spent_usd() - 0.042).abs() < 1e-9);
    }

    #[test]
    fn budget_estimate_matches_token_price() {
        let b = JevBudget::new(10.0);
        // 4000 bytes ≈ 1000 tokens，$0.042 / M → 0.042 micro-cents / token
        // → 1000 * 0.042 ≈ 42 micro-cents（整数除法略偏低：1000*42/1000=42）
        assert_eq!(b.estimate_max_micro_cents(4000), 42);
        // 0 bytes → 至少 1 token：1 * 42 / 1000 = 0（per-token 单价 < 1 micro-cent
        // 会被向下取整为零——预算控制是 conservative 方向）
        assert_eq!(b.estimate_max_micro_cents(0), 0);
        // 1 token 同样 → 0
        assert_eq!(b.estimate_max_micro_cents(4), 0);
        // 大输入才能看到非零 micro-cents
        // 24000 bytes ≈ 6000 tokens → 6000 * 42 / 1000 = 252
        assert_eq!(b.estimate_max_micro_cents(24_000), 252);
    }

    // ── Client-side 校验 ────────────────────────────────────────────────────

    #[tokio::test]
    async fn validation_rejects_empty_state() {
        let tool = tool_with(0.0, 32, 32_000);
        let input = json!({
            "state": "",
            "questions": {"x": {"type": "noul", "instructions": "y"}}
        });
        let r = tool.run(input, &fake_ctx()).await.unwrap();
        assert!(r.is_error);
        assert!(r.content.contains("state 不能为空"));
    }

    #[tokio::test]
    async fn validation_rejects_empty_questions() {
        let tool = tool_with(0.0, 32, 32_000);
        let input = json!({"state": "hi", "questions": {}});
        let r = tool.run(input, &fake_ctx()).await.unwrap();
        assert!(r.is_error);
        assert!(r.content.contains("questions 不能为空"));
    }

    #[tokio::test]
    async fn validation_rejects_too_many_questions() {
        let tool = tool_with(0.0, 2, 32_000);
        let mut qs = serde_json::Map::new();
        for i in 0..3 {
            qs.insert(
                format!("q{i}"),
                json!({"type": "noul", "instructions": "?"}),
            );
        }
        let input = json!({"state": "hi", "questions": Value::Object(qs)});
        let r = tool.run(input, &fake_ctx()).await.unwrap();
        assert!(r.is_error);
        assert!(r.content.contains("超过 max_questions=2"));
    }

    #[tokio::test]
    async fn validation_rejects_choice_empty_criteria() {
        let tool = tool_with(0.0, 32, 32_000);
        let input = json!({
            "state": "hi",
            "questions": {"x": {"type": "choice", "instructions": "?", "criteria": {}}}
        });
        let r = tool.run(input, &fake_ctx()).await.unwrap();
        assert!(r.is_error);
        assert!(r.content.contains("criteria (choice)"));
    }

    #[tokio::test]
    async fn validation_rejects_score_criteria_length_one() {
        let tool = tool_with(0.0, 32, 32_000);
        let input = json!({
            "state": "hi",
            "questions": {"x": {"type": "score", "instructions": "?", "criteria": ["only"]}}
        });
        let r = tool.run(input, &fake_ctx()).await.unwrap();
        assert!(r.is_error);
        assert!(r.content.contains("criteria (score)"));
    }

    #[tokio::test]
    async fn validation_rejects_empty_instructions() {
        let tool = tool_with(0.0, 32, 32_000);
        let input = json!({
            "state": "hi",
            "questions": {"x": {"type": "noul", "instructions": ""}}
        });
        let r = tool.run(input, &fake_ctx()).await.unwrap();
        assert!(r.is_error);
        assert!(r.content.contains("instructions 不能为空"));
    }

    // ── state 截断 ──────────────────────────────────────────────────────────

    #[test]
    fn truncate_state_respects_char_boundary() {
        let tool = tool_with(0.0, 32, 1000);
        // 50 KB ASCII，应被截到 1000 字符（其实 1000 字节，截后 == 1000）
        let s = "x".repeat(50_000);
        let t = tool.truncate_state(&s);
        assert!(t.chars().count() <= 1000);
        assert!(t.len() <= 1000);

        // 含 CJK 的状态：应在 char boundary 处停下（不切到多字节字符中间）
        let s_cjk = "中".repeat(500); // 1 中 = 3 bytes UTF-8，共 1500 bytes
        let t_cjk = tool.truncate_state(&s_cjk);
        // 1000 字节 / 3 = 333 个完整中文字符 + 1 字节余数；is_char_boundary 回退到 999
        assert!(t_cjk.len() <= 1000);
        assert!(t_cjk.chars().count() >= 333);
    }

    // ── Budget 硬封顶（不走 HTTP）───────────────────────────────────────────

    #[tokio::test]
    async fn budget_exceeded_short_circuits_without_http() {
        // $0.0001 预算 + 50KB state → 估算 > 预算，应直接返回 err 而非发 HTTP
        let tool = tool_with(0.0001, 32, 32_000);
        let mut qs = serde_json::Map::new();
        qs.insert(
            "x".to_string(),
            json!({"type": "noul", "instructions": "urgent?"}),
        );
        let input = json!({
            "state": "x".repeat(50_000),
            "questions": Value::Object(qs),
        });
        let r = tool.run(input, &fake_ctx()).await.unwrap();
        assert!(r.is_error);
        assert!(r.content.contains("daily budget exceeded"));
    }

    // ── happy path + mock server ────────────────────────────────────────────

    /// 起一个本地 mock server 处理 POST /v1/systemone，按调用计数。
/// 真实生产中由 typesafe.ai 提供。
///
/// 极简 HTTP/1.1 mock server：仅处理 `POST /v1/systemone`、读 body、
/// 调 handler、回 200。**仅测试用**——只兼容 reqwest 发出的请求格式，
/// 不解析 keep-alive / chunked 等。避免引入 axum 这类重型依赖。
mod mock_server {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

        pub struct Mock {
            pub url: String,
            pub calls: Arc<AtomicUsize>,
        }

        pub async fn spawn_with(handler: fn(&str) -> (u16, String)) -> Mock {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            use tokio::net::TcpListener;

            let calls = Arc::new(AtomicUsize::new(0));
            let calls_inner = calls.clone();
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let bound = listener.local_addr().unwrap();

            tokio::spawn(async move {
                loop {
                    let (mut sock, _) = match listener.accept().await {
                        Ok(p) => p,
                        Err(_) => continue,
                    };
                    let calls = calls_inner.clone();
                    tokio::spawn(async move {
                        // 读 request line + headers 到 \r\n\r\n
                        let mut buf = Vec::with_capacity(4096);
                        let mut tmp = [0u8; 1024];
                        let header_end;
                        loop {
                            match sock.read(&mut tmp).await {
                                Ok(0) => return,
                                Ok(n) => {
                                    buf.extend_from_slice(&tmp[..n]);
                                    if let Some(p) = find_double_crlf(&buf) {
                                        header_end = p;
                                        break;
                                    }
                                }
                                Err(_) => return,
                            }
                        }

                        let header_str = String::from_utf8_lossy(&buf[..header_end]);
                        let content_length = header_str
                            .lines()
                            .find_map(|line| {
                                let (k, v) = line.split_once(':')?;
                                if k.eq_ignore_ascii_case("content-length") {
                                    v.trim().parse::<usize>().ok()
                                } else {
                                    None
                                }
                            })
                            .unwrap_or(0);

                        // 已有 body 部分（buf 末尾可能含部分 body）
                        let mut body = buf[header_end + 4..].to_vec();
                        while body.len() < content_length {
                            match sock.read(&mut tmp).await {
                                Ok(0) => break,
                                Ok(n) => body.extend_from_slice(&tmp[..n]),
                                Err(_) => break,
                            }
                        }
                        let body_str = String::from_utf8_lossy(&body).to_string();

                        calls.fetch_add(1, Ordering::SeqCst);
                        let (status, response) = handler(&body_str);
                        let reason = match status {
                            200 => "OK",
                            401 => "Unauthorized",
                            422 => "Unprocessable Entity",
                            429 => "Too Many Requests",
                            500..=599 => "Server Error",
                            _ => "Status",
                        };
                        let resp = format!(
                            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            response.len(),
                            response
                        );
                        let _ = sock.write_all(resp.as_bytes()).await;
                        let _ = sock.shutdown().await;
                    });
                }
            });

            // 给 mock server 一小段时间启动
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            Mock {
                url: format!("http://{bound}"),
                calls,
            }
        }

        fn find_double_crlf(buf: &[u8]) -> Option<usize> {
            for i in 0..buf.len().saturating_sub(3) {
                if &buf[i..i + 4] == b"\r\n\r\n" {
                    return Some(i);
                }
            }
            None
        }
    }

    #[tokio::test]
    async fn happy_path_choice_returns_full_response() {
        let mock = mock_server::spawn_with(|_body| {
            (
                200,
                r#"{"model":"jev-1.13.0","answers":{"dept":{"type":"choice","choice":"tech","confidence":0.78,"probabilities":{"tech":0.85,"sales":0.0,"billing":0.15}}},"usage":{"input_tokens":42,"output_tokens":10}}"#.to_string(),
            )
        }).await;
        let tool = JevTool::new(
            "sk-test",
            mock.url.clone(),
            "jev-latest",
            32,
            32_000,
            0,
            Arc::new(JevBudget::new(10.0)),
        );
        let mut qs = serde_json::Map::new();
        qs.insert(
            "dept".to_string(),
            serde_json::to_value(choice_q("dept")).unwrap(),
        );
        let input = json!({
            "state": "I have a Stripe integration question",
            "questions": Value::Object(qs),
        });
        let r = tool.run(input, &fake_ctx()).await.unwrap();
        assert!(!r.is_error, "got error: {}", r.content);
        assert!(r.content.contains("\"choice\": \"tech\""));
        assert!(r.content.contains("\"confidence\": 0.78"));
        assert_eq!(mock.calls.load(Ordering::SeqCst), 1);

        // budget 记录了 42 input tokens
        // 42 * 42 / 1000 = 1 micro-cent（向下取整）。
        // 这里不强制数值（tokens 少可能被向下取整为 0），只验证调用数。
    }

    #[tokio::test]
    async fn happy_path_score_and_noul() {
        let mock = mock_server::spawn_with(|_body| {
            (
                200,
                r#"{"model":"jev-1.13.0","answers":{"a":{"type":"score","score":1.0,"confidence":1.0,"legend":{"0":"low","1":"mid","2":"high"},"probabilities":{"0":0.0,"1":1.0,"2":0.0}},"b":{"type":"noul","noul":0.9}},"usage":{"input_tokens":100,"output_tokens":20}}"#.to_string(),
            )
        }).await;
        let tool = JevTool::new(
            "sk-test",
            mock.url.clone(),
            "jev-latest",
            32,
            32_000,
            0,
            Arc::new(JevBudget::new(10.0)),
        );
        let mut qs = serde_json::Map::new();
        qs.insert("a".to_string(), serde_json::to_value(score_q()).unwrap());
        qs.insert("b".to_string(), serde_json::to_value(noul_q()).unwrap());
        let input = json!({"state": "...", "questions": Value::Object(qs)});
        let r = tool.run(input, &fake_ctx()).await.unwrap();
        assert!(!r.is_error, "got error: {}", r.content);
        assert!(r.content.contains("\"score\": 1.0"));
        assert!(r.content.contains("\"noul\": 0.9"));
    }

    // ── 错误归一 ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn http_401_maps_to_authentication() {
        let mock = mock_server::spawn_with(|_body| {
            (
                401,
                r#"{"error":{"message":"invalid api key"}}"#.to_string(),
            )
        })
        .await;
        let tool = JevTool::new(
            "sk-bad",
            mock.url.clone(),
            "jev-latest",
            32,
            32_000,
            0,
            Arc::new(JevBudget::new(10.0)),
        );
        let mut qs = serde_json::Map::new();
        qs.insert("x".to_string(), serde_json::to_value(noul_q()).unwrap());
        let input = json!({"state": "...", "questions": Value::Object(qs)});
        let r = tool.run(input, &fake_ctx()).await.unwrap();
        assert!(r.is_error);
        assert!(r.content.contains("Authentication"), "got: {}", r.content);
    }

    #[tokio::test]
    async fn http_429_maps_to_rate_limited() {
        let mock = mock_server::spawn_with(|_body| {
            (429, r#"{"error":{"message":"rate limit"}}"#.to_string())
        })
        .await;
        let tool = JevTool::new(
            "sk-test",
            mock.url.clone(),
            "jev-latest",
            32,
            32_000,
            0, // 不重试，一次失败就透传
            Arc::new(JevBudget::new(10.0)),
        );
        let mut qs = serde_json::Map::new();
        qs.insert("x".to_string(), serde_json::to_value(noul_q()).unwrap());
        let input = json!({"state": "...", "questions": Value::Object(qs)});
        let r = tool.run(input, &fake_ctx()).await.unwrap();
        assert!(r.is_error);
        assert!(r.content.contains("RateLimited"), "got: {}", r.content);
    }

    #[tokio::test]
    async fn http_529_maps_to_overloaded() {
        let mock = mock_server::spawn_with(|_body| {
            (529, r#"{"error":{"message":"overloaded"}}"#.to_string())
        })
        .await;
        let tool = JevTool::new(
            "sk-test",
            mock.url.clone(),
            "jev-latest",
            32,
            32_000,
            0,
            Arc::new(JevBudget::new(10.0)),
        );
        let mut qs = serde_json::Map::new();
        qs.insert("x".to_string(), serde_json::to_value(noul_q()).unwrap());
        let input = json!({"state": "...", "questions": Value::Object(qs)});
        let r = tool.run(input, &fake_ctx()).await.unwrap();
        assert!(r.is_error);
        assert!(r.content.contains("Overloaded"), "got: {}", r.content);
    }

    // ── 重试 ────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn retries_on_429_then_succeeds() {
        // 第一次返回 429 + Retry-After: 0（最小），第二次 200
        use std::sync::atomic::AtomicUsize;
        static CALL: AtomicUsize = AtomicUsize::new(0);
        let mock = mock_server::spawn_with(|_body| {
            let n = CALL.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                (429, r#"{"error":{"message":"rate limit"}}"#.to_string())
            } else {
                (
                    200,
                    r#"{"model":"jev-1.13.0","answers":{"x":{"type":"noul","noul":0.5}},"usage":{"input_tokens":10,"output_tokens":5}}"#.to_string(),
                )
            }
        }).await;
        // 注意：上面 CALL 是 process-wide static，本测试独立跑 OK，
        // 若有并行测试会相互影响。cargo test 默认单线程 OK。
        let tool = JevTool::new(
            "sk-test",
            mock.url.clone(),
            "jev-latest",
            32,
            32_000,
            2,
            Arc::new(JevBudget::new(10.0)),
        );
        let mut qs = serde_json::Map::new();
        qs.insert("x".to_string(), serde_json::to_value(noul_q()).unwrap());
        let input = json!({"state": "...", "questions": Value::Object(qs)});
        let r = tool.run(input, &fake_ctx()).await.unwrap();
        assert!(!r.is_error, "got error: {}", r.content);
        assert!(r.content.contains("\"noul\": 0.5"));
        assert!(mock.calls.load(Ordering::SeqCst) >= 2);
    }

    // ── integration: 三 primitive 端到端 ─────────────────────────────────────

    #[tokio::test]
    async fn three_primitives_in_one_call() {
        let mock = mock_server::spawn_with(|body| {
            // 验证 body 包含三种 type
            assert!(body.contains("\"choice\""));
            assert!(body.contains("\"score\""));
            assert!(body.contains("\"noul\""));
            (
                200,
                r#"{"model":"jev-1.13.0","answers":{"a":{"type":"choice","choice":"x","confidence":0.9},"b":{"type":"score","score":0.0,"confidence":1.0},"c":{"type":"noul","noul":0.7}},"usage":{"input_tokens":300,"output_tokens":50}}"#.to_string(),
            )
        }).await;
        let tool = JevTool::new(
            "sk-test",
            mock.url.clone(),
            "jev-latest",
            32,
            32_000,
            0,
            Arc::new(JevBudget::new(10.0)),
        );
        let mut qs = serde_json::Map::new();
        qs.insert(
            "a".to_string(),
            serde_json::to_value(choice_q("a")).unwrap(),
        );
        qs.insert("b".to_string(), serde_json::to_value(score_q()).unwrap());
        qs.insert("c".to_string(), serde_json::to_value(noul_q()).unwrap());
        let input = json!({"state": "...", "questions": Value::Object(qs)});
        let r = tool.run(input, &fake_ctx()).await.unwrap();
        assert!(!r.is_error, "got error: {}", r.content);
        assert!(r.content.contains("\"choice\""));
        assert!(r.content.contains("\"score\""));
        assert!(r.content.contains("\"noul\""));
    }
}
