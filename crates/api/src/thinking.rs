//! Thinking / reasoning 适配层。
//!
//! 把 vendor × protocol × mode × effort 维度的所有差异集中在一个模块，让 provider
//! 实现（`anthropic.rs` / `openai.rs`）只需要调用一个统一的 dispatch 入口。
//!
//! ## 设计目标
//!
//! 1. **OpenAI 协议路径真的把 thinking 发出去**：之前 `OpenAIProvider::stream`
//!    显式忽略 `opts.thinking_budget`。这里按 vendor 适配各家字段名（DeepSeek
//!    `thinking.type` / Qwen `enable_thinking` / Doubao `thinking` 对象 / GLM
//!    `thinking.type` / Moonshot `thinking.type` 或 `reasoning_effort` / MiniMax
//!    `thinking.type`），所有 adapter 一次性集中维护。
//! 2. **Anthropic 兼容端点细粒度降级**：第三方兼容端点（GLM / MiniMax / Moonshot
//!    的 `/anthropic` 路径）默认不发 `interleaved-thinking` beta header，避免 400。
//!    官方 Anthropic 端点照发。
//! 3. **能力目录按 vendor 准确标注**：`ThinkingSpec::for_vendor` 是 catalog 阶段
//!    唯一的真值来源，`model_catalog.rs` 不再一刀切 `BudgetTokens`。
//!
//! ## 模块结构
//!
//! - `ReasoningEffort`：内部档位枚举（`low / medium / high / max / xhigh` 等），
//!   与各家 enum 值有简单映射，**不**对外暴露在 `ThinkingMode` / `ReasoningRequest` 里。
//! - `ThinkingSpec`：vendor 静态声明的 thinking 形态（含 user_can_disable、
//!   response_fields 等运行期提示）。
//! - `ThinkingAdapter`：trait，每个 vendor 一个 `&'static` 实例。`adapter_for()`
//!   按 `(wire_protocol, vendor)` 静态分派，避免 vtable 成本。
//! - `apply_*_thinking` 自由函数：provider 在序列化请求体后调用，把 thinking 参数
//!   写入 body / header。

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::capabilities::ModelIdentity;
use crate::provider::RequestOptions;
use wyj_config::WireProtocol;

/// thinking 字段名版本变体。国产 vendor 历史上多次调整字段名
/// (`thinking.type` vs `enable_thinking` vs `reasoning_effort` 等),不同子版本
/// 走不同发送策略。默认 `StandardV1` 与现有 OpenAI Chat Completions 主流
/// 协议一致；`--probe full` 主动探测时可升级到 ExperimentalV2。
///
/// `detect_vendor_variant` 是占位函数：当前所有已知 vendor 都标 `StandardV1`，
/// 阶段 7 结构预留为主——后续真实探测接入时只需扩 match 即可。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum VendorVariant {
    /// OpenAI Chat Completions 主流标准（默认）
    #[default]
    StandardV1,
    /// 探测发现需要兼容老版本字段（如 vendor 自定义变体）
    ExperimentalV2,
}

/// 按 vendor × model 名推导字段名版本。占位实现——目前所有已知 vendor 都
/// 走 StandardV1；阶段 7 预留接口，--probe full 实际接入探测后扩展。
pub fn detect_vendor_variant(vendor: &str, model: &str) -> VendorVariant {
    let _ = (vendor, model);
    VendorVariant::StandardV1
}

/// OpenAI-vendor `reasoning_effort` 字符串档位的统一表示。
/// 不暴露在 `ThinkingMode` 或 `ReasoningRequest` 里——`ReasoningRequest::Effort(String)`
/// 仍是 wire 协议，传字符串原值让各 vendor adapter 自己解读。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Minimal,
    Low,
    Medium,
    High,
    Max,
    XHigh,
    /// GLM / MiniMax M2.x 的 adaptive 模式（服务端按查询自动决定）
    Adaptive,
}

impl ReasoningEffort {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "minimal" => Some(Self::Minimal),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "max" => Some(Self::Max),
            "xhigh" => Some(Self::XHigh),
            "adaptive" => Some(Self::Adaptive),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Max => "max",
            Self::XHigh => "xhigh",
            Self::Adaptive => "adaptive",
        }
    }
}

// ── Vendor 静态声明 ─────────────────────────────────────────────────────────

/// thinking 控制形态。决定 adapter 在 body 里写哪些字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingControl {
    /// 完全不支持（如 Ollama 普通模型、custom vendor）
    Disabled,
    /// 仅 thinking.type 开关（GLM、Moonshot k2.5/2.6、MiniMax 等）
    SwitchOnly,
    /// thinking.type + budget_tokens（DeepSeek 弱支持；Doubao 标准形态）
    SwitchPlusBudget,
    /// thinking.type + reasoning_effort 顶层（DeepSeek 标准形态）
    SwitchPlusEffort,
    /// 仅 budget_tokens（Anthropic 原生 thinking）
    BudgetOnly,
    /// 仅 reasoning_effort 顶层（Moonshot k3、Qwen 互斥时的 effort 分支）
    EffortOnly,
}

/// 响应里实际会出现哪些 thinking 字段，决定 OpenAI provider 是否解析。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ThinkingResponseFields {
    /// `delta.reasoning_content` / `message.reasoning_content` 是否可能存在
    pub reasoning_content: bool,
    /// `delta.reasoning_details` / `message.reasoning_details`（MiniMax 特有）
    pub reasoning_details: bool,
    /// 是否需要把 `reasoning_content` 拼回多轮 messages（GLM/Qwen/DeepSeek 工具调用场景必须）。
    /// 由 `openai.rs::to_api_messages` 消费：`true` 时 assistant 消息带出
    /// `reasoning_content`（以及 MiniMax 的 `reasoning_details`），`false` 时字段
    /// 整个不出现——ollama / vllm 虽在响应里回填 reasoning_content，回传却会 400。
    pub echo_back_in_messages: bool,
}

/// Vendor × model 的静态声明。catalog 阶段一次性查表，进程内常量。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThinkingSpec {
    pub control: ThinkingControl,
    /// 用户 profile 显式关闭时，是否允许（false = vendor 强制开，profile 设 None 也开）
    pub user_can_disable: bool,
    /// 该 vendor 是否原生支持 token 预算（false = 只支持 type 开关或 effort 档）
    pub budget_tokens_supported: bool,
    /// 该 vendor 支持哪些 effort 档位（空切片 = 不支持 effort）
    pub effort_levels: &'static [ReasoningEffort],
    /// `budget_tokens` 与 `effort` 是否**可同时生效**（而非互斥）。
    ///
    /// 互斥是常态：Qwen 协议明确规定 `thinking_budget` 与 `reasoning_effort`
    /// 不能同传，DeepSeek 的 `reasoning_effort` 顶层字段与 `thinking.type` 也
    /// 各占一个旋钮。官方 Anthropic 是例外——文档明确说 effort 与 thinking 可以
    /// 一起用（Opus 4.5 上 effort 配合 `budget_tokens`），两个旋钮控制的是
    /// 不同维度：budget 限**思考**的 token 数，effort 限**整个响应**的投入档位。
    ///
    /// 置 true 时，`request_plan` 不会因为 budget 存在就短路掉 effort。
    pub budget_and_effort_coexist: bool,
    pub response_fields: ThinkingResponseFields,
    /// 用户传 `disabled` 时是否被静默忽略（true = MiniMax M2.x / DeepSeek reasoner）
    pub ignore_disabled_when_forced: bool,
}

impl ThinkingSpec {
    /// Vendor × wire × model 的唯一查表入口。
    pub fn for_vendor(vendor: &str, wire: &WireProtocol, model: &str) -> Self {
        match (
            wire,
            normalize_vendor(vendor),
            model.to_ascii_lowercase().as_str(),
        ) {
            // ── Anthropic 协议 ──
            (WireProtocol::AnthropicMessages, "anthropic", m) => Self::anthropic_native(m),
            (WireProtocol::AnthropicMessages, "minimax", m) => Self::minimax_anthropic(m),
            (WireProtocol::AnthropicMessages, _, _) => Self::anthropic_compatible(),

            // ── OpenAI 协议 + 各国产 vendor ──
            (WireProtocol::OpenAiChatCompletions, "openai", m) => Self::openai_official(m),
            (WireProtocol::OpenAiChatCompletions, "deepseek", m) => Self::deepseek(m),
            (WireProtocol::OpenAiChatCompletions, "alibaba", m) => Self::qwen(m),
            (WireProtocol::OpenAiChatCompletions, "zhipu", _) => Self::glm(),
            (WireProtocol::OpenAiChatCompletions, "volcengine", m) => Self::doubao(m),
            (WireProtocol::OpenAiChatCompletions, "moonshot", m) => Self::moonshot(m),
            (WireProtocol::OpenAiChatCompletions, "minimax", m) => Self::minimax(m),
            (WireProtocol::OpenAiChatCompletions, "ollama", m) => Self::ollama(m),
            (WireProtocol::OpenAiChatCompletions, "vllm", m) => Self::vllm(m),

            // ── OpenAI Responses / QwenNative / Gemini / 其它 vendor ──
            // 默认走 generic，不主动发 thinking；用户 profile 显式开启时由 adapter
            // 透传为 thinking.type=enabled（部分 Anthropic Responses 端点认得）。
            _ => Self::generic_openai(),
        }
    }

    // ── 各种 spec factory ──

    /// 官方 Anthropic（Claude）系列的 spec。
    ///
    /// 官方协议现在**原生支持 effort**（`output_config.effort`），档位与
    /// MiniMax 的 `/anthropic` 端点同构（low/medium/high/xhigh/max）——这是
    /// 2026 年 Anthropic 官方新增的能力，与 OpenAI 系的 `reasoning_effort`
    /// 语义对齐：**同一个 profile 字段 `reasoning_effort`，不论走 Anthropic
    /// 线还是 OpenAI 线都能生效**，不必为协议差异写两份配置。
    ///
    /// 档位按型号区分（官方文档「Effort levels」节）：
    /// * Opus 4.8 / 4.7 / 5 / 5.5、Sonnet 5 / 5.5、Haiku 5.5、Fable / Mythos 系
    ///   ——五档全量（low/medium/high/xhigh/max）；
    /// * Opus 4.6 / Sonnet 4.6 ——到 max 为止（无 xhigh）；
    /// * Opus 4.5 / Sonnet 4.5 ——到 high 为止（无 xhigh/max）。
    ///
    /// 旧型号（3.x / 4.0 等）不支持 effort，档位集合留空 → `request_plan`
    /// 把 profile 的 `reasoning_effort` 记成可见的 dropped，而不是发出去换 400。
    pub fn anthropic_native(model: &str) -> Self {
        Self {
            control: ThinkingControl::BudgetOnly,
            user_can_disable: true,
            budget_tokens_supported: true,
            effort_levels: anthropic_effort_levels(model),
            budget_and_effort_coexist: true,
            response_fields: ThinkingResponseFields {
                reasoning_content: false,
                reasoning_details: false,
                echo_back_in_messages: false,
            },
            ignore_disabled_when_forced: false,
        }
    }

    pub const fn anthropic_compatible() -> Self {
        // 第三方 Anthropic 兼容端点：默认不发 interleaved-thinking beta，
        // 但仍发 thinking 字段。budget_tokens 是 Anthropic 协议原生字段。
        Self {
            control: ThinkingControl::BudgetOnly,
            user_can_disable: true,
            budget_tokens_supported: true,
            effort_levels: &[],
            budget_and_effort_coexist: false,
            response_fields: ThinkingResponseFields {
                reasoning_content: false,
                reasoning_details: false,
                echo_back_in_messages: false,
            },
            ignore_disabled_when_forced: false,
        }
    }

    /// 官方 OpenAI（o 系列 / gpt-5 系）在 **Chat Completions** 上的 spec。
    ///
    /// 此前 `for_vendor` 没有 `"openai"` 臂，一律落到 `generic_openai()` 的
    /// `ThinkingControl::Disabled`，于是三层连锁失效：
    /// ① catalog 把 `capabilities.thinking` 标成 `Unsupported`；
    /// ② `agent.rs` 的 `blocked` 判据因此在**到达 provider 之前**就把
    ///    `reasoning_effort` 滤成 `None`；
    /// ③ 即便漏过 ②，`apply_thinking_to_openai_body` 也会在 `control ==
    ///    Disabled` 时直接 return。
    /// 结果：`provider = "openai"` 的 profile 配 `reasoning_effort` 是**死配置**，
    /// 只在日志里留一条 dropped。这与 DeepSeek / Qwen / MiniMax 上 effort 生效
    /// 形成尖锐对比，也是「两套 API 逻辑不一致」里最明显的一处。
    ///
    /// 形态上官方只有一个旋钮：顶层 `reasoning_effort`。**没有** `thinking.type`、
    /// 没有 `thinking_budget`、响应里也**不返回** `reasoning_content`（那是国产
    /// 兼容端点的约定），因此 `echo_back_in_messages` 必须为 false——照抄
    /// DeepSeek 臂把它打开，官方端点会因未知字段直接 400。
    ///
    /// 档位刻意**不收 `minimal`**：`gpt-5.1+` 明确不认它（官方文档「Reasoning
    /// effort」节），放进白名单等于预埋一个 400。`o1-mini` / `o3-mini` 早代
    /// 完全不支持 `reasoning_effort`，档位集合留空 → `request_plan` 记成可见的
    /// dropped，而不是发出去换 400。
    fn openai_official(model: &str) -> Self {
        let m = model.to_ascii_lowercase();
        let effort_levels: &'static [ReasoningEffort] =
            if m.contains("o1-mini") || m.contains("o3-mini") {
                // 早代（o1-mini / o3-mini）不支持 reasoning_effort。
                &[]
            } else if m.contains("codex-max") {
                // gpt-5.1-codex-max 是唯一支持 xhigh 的 codex 变体。
                &[
                    ReasoningEffort::Low,
                    ReasoningEffort::Medium,
                    ReasoningEffort::High,
                    ReasoningEffort::XHigh,
                ]
            } else {
                // 其余 o 系列 / gpt-5 系：low/medium/high。
                // `xhigh` / `max` 只在更新的型号上开放，且不同代际边界不一致——
                // 宁可少开两档（记成可见 dropped），也不发一个会 400 的档位。
                &[
                    ReasoningEffort::Low,
                    ReasoningEffort::Medium,
                    ReasoningEffort::High,
                ]
            };
        Self {
            // 官方没有 `thinking.type` 开关，只有 effort 一个旋钮。
            control: ThinkingControl::EffortOnly,
            // 不发字段 = 用端点默认，因此用户「关思考」在这条线上不成立
            // （若要关必须显式传 `"none"`，见 adapter）。
            user_can_disable: true,
            budget_tokens_supported: false,
            effort_levels,
            budget_and_effort_coexist: false,
            response_fields: ThinkingResponseFields {
                reasoning_content: false,
                reasoning_details: false,
                // 官方 Chat Completions 不返回、也不接受回传 reasoning_content。
                echo_back_in_messages: false,
            },
            ignore_disabled_when_forced: false,
        }
    }

    fn deepseek(model: &str) -> Self {
        let force_on =
            model.contains("reasoner") || model.contains("-r1") || model == "deepseek-r1";
        Self {
            control: ThinkingControl::SwitchPlusEffort,
            user_can_disable: !force_on,
            budget_tokens_supported: false,
            effort_levels: &[
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
                ReasoningEffort::Max,
            ],
            budget_and_effort_coexist: false,
            response_fields: ThinkingResponseFields {
                reasoning_content: true,
                reasoning_details: false,
                echo_back_in_messages: true,
            },
            ignore_disabled_when_forced: force_on,
        }
    }

    fn qwen(model: &str) -> Self {
        // -thinking 后缀强制开；qwen3-coder-plus 这类支持 thinking_budget；
        // reasoning_effort 与 thinking_budget 互斥（Qwen 协议约束）
        let force_on = model.contains("-thinking") || model.contains("qwq");
        Self {
            control: ThinkingControl::SwitchPlusBudget,
            user_can_disable: !force_on,
            budget_tokens_supported: true,
            effort_levels: &[
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
                ReasoningEffort::XHigh,
            ],
            budget_and_effort_coexist: false,
            response_fields: ThinkingResponseFields {
                reasoning_content: true,
                reasoning_details: false,
                echo_back_in_messages: true,
            },
            ignore_disabled_when_forced: force_on,
        }
    }

    fn glm() -> Self {
        Self {
            control: ThinkingControl::SwitchOnly,
            user_can_disable: true,
            budget_tokens_supported: false,
            effort_levels: &[],
            budget_and_effort_coexist: false,
            response_fields: ThinkingResponseFields {
                reasoning_content: true,
                reasoning_details: false,
                echo_back_in_messages: true,
            },
            ignore_disabled_when_forced: false,
        }
    }

    fn doubao(model: &str) -> Self {
        let force_on = model.contains("-thinking");
        Self {
            control: ThinkingControl::SwitchPlusBudget,
            user_can_disable: !force_on,
            budget_tokens_supported: true,
            effort_levels: &[],
            budget_and_effort_coexist: false,
            response_fields: ThinkingResponseFields {
                reasoning_content: true,
                reasoning_details: false,
                echo_back_in_messages: true,
            },
            ignore_disabled_when_forced: force_on,
        }
    }

    fn moonshot(model: &str) -> Self {
        if model.contains("k3") {
            // kimi-k3 走顶层 reasoning_effort
            Self {
                control: ThinkingControl::EffortOnly,
                user_can_disable: true,
                budget_tokens_supported: false,
                effort_levels: &[
                    ReasoningEffort::Low,
                    ReasoningEffort::High,
                    ReasoningEffort::Max,
                ],
                budget_and_effort_coexist: false,
                response_fields: ThinkingResponseFields {
                    reasoning_content: true,
                    reasoning_details: false,
                    echo_back_in_messages: true,
                },
                ignore_disabled_when_forced: false,
            }
        } else {
            // k2.5/2.6 走 thinking.type；强制思考的 -thinking/-code 后缀不认 disabled
            let force_on = model.contains("-thinking") || model.contains("-code");
            Self {
                control: ThinkingControl::SwitchOnly,
                user_can_disable: !force_on,
                budget_tokens_supported: false,
                effort_levels: &[],
                budget_and_effort_coexist: false,
                response_fields: ThinkingResponseFields {
                    reasoning_content: true,
                    reasoning_details: false,
                    echo_back_in_messages: true,
                },
                ignore_disabled_when_forced: force_on,
            }
        }
    }

    /// MiniMax 的 **`/anthropic` 线协议**（与 [`Self::minimax`] 的 OpenAI 线是两套
    /// 不同的端点形态，档位集合也不一致，不要互相复用）。
    ///
    /// `/anthropic` 端点只认 `thinking.type ∈ {省略, adaptive, disabled}`，思考深度
    /// 走 `output_config.effort`。因此四条与 `anthropic_compatible()` 相反：
    /// - `budget_tokens_supported: false`——`budget_tokens` 在这条线上没有语义，
    ///   不设 false 的话 `request_plan` 会把它当成合法 reasoning 参数放行，
    ///   adapter 端再丢一次，两层各记一次「被忽略」，更糟的是会把同一 profile 里的
    ///   `reasoning_effort` 一起连坐丢掉（见 `request_plan` 的 budget 兜底臂）。
    /// - `control: SwitchOnly`——catalog 据此把 `ThinkingMode` 标成 `Effort`，
    ///   `agent.rs` 才不会用 `ThinkingMode::BudgetTokens` 的判据把 thinking_budget 留下。
    /// - `user_can_disable` 只有 M3 为真（M3.1 发 `disabled` 直接 400、M2.x 发了
    ///   不生效），与 adapter 的降级策略同源。
    /// - `effort_levels` 仅 M3.1 有，且是端点文档的五档全量（含 OpenAI 线那边
    ///   刻意避开的 `medium` / `xhigh`）。
    fn minimax_anthropic(model: &str) -> Self {
        if is_minimax_m31_flash(model) {
            Self {
                control: ThinkingControl::SwitchOnly,
                user_can_disable: false,
                budget_tokens_supported: false,
                effort_levels: MINIMAX_M31_ANTHROPIC_EFFORT_LEVELS,
                budget_and_effort_coexist: false,
                // Anthropic 线协议下 `response_fields` 当前无消费者
                // （只有 `openai.rs::to_api_messages` 读它），仍按端点真实行为填写，
                // 以免将来接线时又出现「catalog 说一套、adapter 做另一套」。
                response_fields: ThinkingResponseFields {
                    reasoning_content: true,
                    reasoning_details: true,
                    echo_back_in_messages: true,
                },
                ignore_disabled_when_forced: true,
            }
        } else {
            let is_m2 = model.contains("m2") && !model.contains("m3");
            Self {
                control: ThinkingControl::SwitchOnly,
                user_can_disable: !is_m2,
                budget_tokens_supported: false,
                // M3 / M2.x 的 `/anthropic` 端点不认 output_config.effort，档位集合
                // 留空 = request_plan 把 profile 的 reasoning_effort 记成可见 dropped。
                effort_levels: &[],
                budget_and_effort_coexist: false,
                response_fields: ThinkingResponseFields {
                    reasoning_content: true,
                    reasoning_details: true,
                    echo_back_in_messages: true,
                },
                ignore_disabled_when_forced: is_m2,
            }
        }
    }

    fn minimax(model: &str) -> Self {
        // M2.x 强制思考（`thinking.type=adaptive`，忽略 user disabled）；
        // M3 起允许显式关闭、并支持 effort 档位（low / high / max，
        // 对齐 Moonshot k3 的档位集合，避开 server 端可能不认的 xhigh/adaptive）。
        let is_m2 = model.contains("m2") && !model.contains("m3");
        if is_m2 {
            Self {
                control: ThinkingControl::SwitchOnly,
                user_can_disable: false,
                budget_tokens_supported: false,
                effort_levels: &[],
                budget_and_effort_coexist: false,
                response_fields: ThinkingResponseFields {
                    reasoning_content: true,
                    reasoning_details: true,
                    echo_back_in_messages: true,
                },
                ignore_disabled_when_forced: true,
            }
        } else {
            Self {
                control: ThinkingControl::SwitchPlusEffort,
                user_can_disable: true,
                budget_tokens_supported: false,
                effort_levels: &[
                    ReasoningEffort::Low,
                    ReasoningEffort::High,
                    ReasoningEffort::Max,
                ],
                budget_and_effort_coexist: false,
                response_fields: ThinkingResponseFields {
                    reasoning_content: true,
                    reasoning_details: true,
                    echo_back_in_messages: true,
                },
                ignore_disabled_when_forced: false,
            }
        }
    }

    fn ollama(model: &str) -> Self {
        let supports_think = model.contains("think");
        if supports_think {
            Self {
                control: ThinkingControl::SwitchOnly,
                user_can_disable: true,
                budget_tokens_supported: false,
                effort_levels: &[],
                budget_and_effort_coexist: false,
                response_fields: ThinkingResponseFields {
                    reasoning_content: true,
                    reasoning_details: false,
                    echo_back_in_messages: false,
                },
                ignore_disabled_when_forced: false,
            }
        } else {
            Self::generic_openai()
        }
    }

    fn vllm(model: &str) -> Self {
        if model.contains("think") || model.contains("reasoning") {
            Self {
                control: ThinkingControl::SwitchOnly,
                user_can_disable: true,
                budget_tokens_supported: false,
                effort_levels: &[],
                budget_and_effort_coexist: false,
                response_fields: ThinkingResponseFields {
                    reasoning_content: true,
                    reasoning_details: false,
                    echo_back_in_messages: false,
                },
                ignore_disabled_when_forced: false,
            }
        } else {
            Self::generic_openai()
        }
    }

    const fn generic_openai() -> Self {
        Self {
            control: ThinkingControl::Disabled,
            user_can_disable: true,
            budget_tokens_supported: false,
            effort_levels: &[],
            budget_and_effort_coexist: false,
            response_fields: ThinkingResponseFields {
                reasoning_content: false,
                reasoning_details: false,
                echo_back_in_messages: false,
            },
            ignore_disabled_when_forced: false,
        }
    }
}

/// vendor 名归一（别名 → 规范名）。
///
/// `pub(crate)` 是因为 `openai.rs` 也要用它判「是否官方 OpenAI 端点」——
/// 别名表若在两处各写一份，`vendor = "OpenAI"` / `"openai"` 这种差异会让
/// 判据静默失效。
pub(crate) fn normalize_vendor(vendor: &str) -> &'static str {
    match vendor.trim().to_ascii_lowercase().as_str() {
        "glm" | "zhipu" | "zai" | "z.ai" => "zhipu",
        "kimi" | "moonshot" => "moonshot",
        "qwen" | "bailian" | "alibaba" => "alibaba",
        "doubao" | "volcengine" | "ark" => "volcengine",
        "minimax" => "minimax",
        "deepseek" => "deepseek",
        "anthropic" => "anthropic",
        "openai" => "openai",
        "ollama" => "ollama",
        "vllm" => "vllm",
        _ => "custom",
    }
}

/// 该 endpoint 是否接受 `reasoning_effort` 档位（不关心具体档位值）。
///
/// 唯一判据是 `ThinkingSpec::for_vendor(..).effort_levels`。`ModelCapabilities`
/// 里的 `ThinkingMode` 是**单值**标签（Qwen 同时支持 budget 与 effort，标签只能
/// 记一个），拿它判 effort 会同时产生两类错误：Qwen 被误判成"不支持 effort"，
/// GLM / Ollama / Moonshot k2 被误判成"支持 effort"。
pub fn supports_effort(identity: &ModelIdentity) -> bool {
    !ThinkingSpec::for_vendor(&identity.vendor, &identity.wire_protocol, &identity.model)
        .effort_levels
        .is_empty()
}

/// 把用户填的 effort 字符串收敛成该 vendor 真正支持的档位。
///
/// 返回 `None` 的两种情况都代表**不能上送**：vendor 整体不支持 effort，或这个
/// 具体档位不在 `effort_levels` 内（如给 DeepSeek 填 `xhigh`）。调用方应把它
/// 记成 `dropped_parameter`，不能静默丢弃。
///
/// catalog / agent 层 / provider 序列化 body 三处共用这一个判据，避免
/// "catalog 说支持、adapter 悄悄丢掉"这种自相矛盾。
pub fn normalize_effort(identity: &ModelIdentity, raw: &str) -> Option<&'static str> {
    let spec = ThinkingSpec::for_vendor(&identity.vendor, &identity.wire_protocol, &identity.model);
    let parsed = ReasoningEffort::parse(raw)?;
    spec.effort_levels
        .contains(&parsed)
        .then_some(parsed.as_str())
}

// ── ThinkingAdapter trait + dispatch ─────────────────────────────────────────

// ── Anthropic 线协议下的 thinking 字段 ─────────────────────────────────────

/// Anthropic 线协议（`/v1/messages`）下 thinking 相关字段的最终形态。
///
/// 与 OpenAI 路径不同，这里不直接 mutate 一个 `Map`：provider 侧的请求体是
/// `#[derive(Serialize)]` 的具名结构体（`ApiRequest`），字段是否出现由
/// `Option` 决定，因此让 adapter 产出一个补丁而不是副作用。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnthropicThinkingPatch {
    /// 请求体的 `thinking` 字段。官方 Anthropic 是
    /// `{"type":"enabled","budget_tokens":N}`；MiniMax 兼容端点是
    /// `{"type":"adaptive"}` / `{"type":"disabled"}`（**没有** budget 字段）。
    pub thinking: Option<Value>,
    /// `thinking.budget_tokens` 的**结构化**取值，供 provider 抬高 `max_tokens`
    /// （Anthropic 硬性要求 `budget_tokens < max_tokens`）。
    ///
    /// 之所以不从 `thinking` 这个 `Value` 里反解键名：`budget_tokens` 与 `max_tokens`
    /// 的联动是协议硬约束，却要经过一次字符串往返才能拿到——adapter 改个键名，
    /// 兜底会**静默失效**且没有任何告警，而后果是端点直接 400。显式字段让这件事
    /// 在类型上就可见。MiniMax 恒为 `None`（端点没有 budget 概念）。
    pub budget: Option<u32>,
    /// 请求体的 `output_config` 字段（MiniMax M3.1-Flash-Preview 的思考深度档位）。
    pub output_config: Option<Value>,
    /// 用户显式配置了但被端点硬约束否决、已静默忽略的字段名。
    /// 非空时 provider 应打一条 warn——静默丢弃会让用户以为配置生效了。
    pub ignored: Vec<&'static str>,
}

/// 各 vendor 的 thinking 应用策略。`&'static` 实例，dispatch 时零分配。
pub trait ThinkingAdapter: Send + Sync {
    /// 把 thinking 参数应用到 OpenAI 协议的请求 body。
    /// 仅在 control != Disabled 时调用；否则 body 不变。
    fn apply_openai(&self, body: &mut Map<String, Value>, opts: &RequestOptions);

    /// 计算 Anthropic 线协议（`/v1/messages`）下要发的 thinking 字段。
    /// 默认不主动发任何东西，由 provider 自行处理。
    ///
    /// `identity` 透传是为了让 adapter 能调 [`normalize_effort`]——档位判据只有
    /// `ThinkingSpec::effort_levels` 一处，adapter 私藏第二份白名单必然漂移。
    fn apply_anthropic(
        &self,
        identity: &ModelIdentity,
        opts: &RequestOptions,
    ) -> AnthropicThinkingPatch {
        let _ = (identity, opts);
        AnthropicThinkingPatch::default()
    }

    /// 是否在 Anthropic 兼容端点发 `interleaved-thinking` beta header。
    /// 官方 Anthropic 端点 → true；第三方兼容 → false。
    fn emit_interleaved_beta(&self, is_official_anthropic: bool) -> bool;
}

/// provider 侧统一入口：算出 Anthropic 线协议下要发的 thinking 字段。
pub fn apply_anthropic_thinking(
    identity: &ModelIdentity,
    opts: &RequestOptions,
) -> AnthropicThinkingPatch {
    adapter_for(identity).apply_anthropic(identity, opts)
}

/// Dispatch：`match (wire, vendor)` 静态分派到具体 adapter。
pub fn adapter_for(identity: &ModelIdentity) -> &'static dyn ThinkingAdapter {
    let wire = &identity.wire_protocol;
    let vendor = normalize_vendor(&identity.vendor);
    match (wire, vendor) {
        (WireProtocol::AnthropicMessages, "anthropic") => &AnthropicNativeAdapter,
        (WireProtocol::AnthropicMessages, "minimax") => {
            // MiniMax 的 `/anthropic` 端点不吃 Anthropic 原生的
            // `{type:"enabled", budget_tokens:N}` 形态：思考深度由
            // `output_config.effort` 档位控制，且不同代际对 `disabled` 的
            // 容忍度不同（见 `AnthropicMinimaxAdapter`）。
            if is_minimax_m31_flash(&identity.model) {
                static M31_ADAPTER: AnthropicMinimaxAdapter = AnthropicMinimaxAdapter::M31Flash;
                &M31_ADAPTER
            } else if identity.model.to_ascii_lowercase().contains("m3") {
                static M3_ADAPTER: AnthropicMinimaxAdapter = AnthropicMinimaxAdapter::M3;
                &M3_ADAPTER
            } else {
                static M2_ADAPTER: AnthropicMinimaxAdapter = AnthropicMinimaxAdapter::M2;
                &M2_ADAPTER
            }
        }
        (WireProtocol::AnthropicMessages, _) => &AnthropicCompatibleAdapter,
        (WireProtocol::OpenAiChatCompletions, "openai") => &OpenAiOfficialAdapter,
        (WireProtocol::OpenAiChatCompletions, "deepseek") => &DeepSeekOpenAiAdapter,
        (WireProtocol::OpenAiChatCompletions, "alibaba") => &QwenOpenAiAdapter,
        (WireProtocol::OpenAiChatCompletions, "zhipu") => &ZhipuOpenAiAdapter,
        (WireProtocol::OpenAiChatCompletions, "volcengine") => &VolcengineOpenAiAdapter,
        (WireProtocol::OpenAiChatCompletions, "moonshot") => &MoonshotOpenAiAdapter,
        (WireProtocol::OpenAiChatCompletions, "minimax") => {
            // M3 起支持 reasoning_effort；M2.x 仍走 SwitchOnly。spec.effort_levels
            // 是否为空决定了是否能上送 effort，但 &'static adapter 拿不到 model
            // 信息，所以这里按 model 名分派到 MinimaxAdapter::M2 / M3。
            if identity.model.to_ascii_lowercase().contains("m3") {
                static M3_ADAPTER: MinimaxAdapter = MinimaxAdapter::M3;
                &M3_ADAPTER
            } else {
                static M2_ADAPTER: MinimaxAdapter = MinimaxAdapter::M2;
                &M2_ADAPTER
            }
        }
        (WireProtocol::OpenAiChatCompletions, "ollama") => &OllamaOpenAiAdapter,
        (WireProtocol::OpenAiChatCompletions, "vllm") => &VllmOpenAiAdapter,
        (WireProtocol::OpenAiChatCompletions, _) => &GenericOpenAiAdapter,
        _ => &GenericOpenAiAdapter,
    }
}

// ── Adapter 实现 ──────────────────────────────────────────────────────────────

/// 官方 Anthropic：thinking 字段照发 + interleaved beta 照发。
struct AnthropicNativeAdapter;

impl ThinkingAdapter for AnthropicNativeAdapter {
    fn apply_openai(&self, _body: &mut Map<String, Value>, _opts: &RequestOptions) {
        // Anthropic 协议不走 OpenAI body；anthropic.rs 自行处理。
    }

    fn apply_anthropic(
        &self,
        identity: &ModelIdentity,
        opts: &RequestOptions,
    ) -> AnthropicThinkingPatch {
        // 官方 Anthropic 有**两个**思考旋钮：
        // * `thinking.budget_tokens` —— 思考本身的 token 预算（extended thinking）；
        // * `output_config.effort` —— 整个响应（正文 + 工具调用 + 思考）的
        //   投入档位（low/medium/high/xhigh/max），与 OpenAI 系的
        //   `reasoning_effort` 语义对齐。
        //
        // 两者**互补而非互斥**：官方文档明确说 effort 与 thinking 可以同时用
        // （Opus 4.5 上 effort 配合 budget_tokens 一起用）。所以这里两个都发，
        // 而不是二选一。
        let budget = opts.thinking_budget.filter(|b| *b > 0);
        let mut patch = AnthropicThinkingPatch {
            thinking: budget.map(|b| json!({"type": "enabled", "budget_tokens": b})),
            budget,
            output_config: None,
            ignored: Vec::new(),
        };
        // effort 走 `output_config.effort`。档位判据统一走 `normalize_effort`
        // （即 spec 的 `effort_levels`），不在本 adapter 里维护第二份白名单。
        if let Some(raw) = opts.reasoning_effort.as_deref() {
            match normalize_effort(identity, raw) {
                Some(level) => patch.output_config = Some(json!({ "effort": level })),
                None => patch.ignored.push("reasoning_effort"),
            }
        }
        // 这条线协议的开关**只有 budget 一个旋钮**，profile 的 thinking_switch
        // 无处落地。两种静默覆盖都要记进 ignored：配了 `disabled` 却仍发出
        // `enabled`（thinking 照开）、配了 `enabled` 却没有 budget（thinking
        // 其实没开）——用户看日志才知道自己配的东西被谁覆盖了。
        // 注意 `text_only()` 的 `disabled` + budget=None 组合是**被兑现的**，
        // 不该告警（否则 compact/memory 等每次辅助调用都吵一遍）。
        if explicit_switch(opts).is_some() && switch_says_off(opts) != budget.is_none() {
            patch.ignored.push("thinking_switch");
        }
        patch
    }

    fn emit_interleaved_beta(&self, _is_official: bool) -> bool {
        true
    }
}

/// 第三方 Anthropic 兼容端点：thinking 字段照发，但不附加 interleaved beta。
struct AnthropicCompatibleAdapter;

impl ThinkingAdapter for AnthropicCompatibleAdapter {
    fn apply_openai(&self, _body: &mut Map<String, Value>, _opts: &RequestOptions) {
        // 同上，anthropic.rs 处理。
    }

    fn apply_anthropic(
        &self,
        _identity: &ModelIdentity,
        opts: &RequestOptions,
    ) -> AnthropicThinkingPatch {
        // 第三方 Anthropic 兼容端点但 vendor 不是 MiniMax：沿用 Anthropic 原生
        // 形态。GLM / Moonshot 的 `/anthropic` 路径确实认 `enabled` +
        // `budget_tokens`；未知的兼容端点按最保守的可接受形态发。
        // 开关判据与 `AnthropicNativeAdapter` 完全一致（那边就是本 adapter 的原型）。
        let budget = opts.thinking_budget.filter(|b| *b > 0);
        let mut patch = AnthropicThinkingPatch {
            thinking: budget.map(|b| json!({"type": "enabled", "budget_tokens": b})),
            budget,
            output_config: None,
            ignored: Vec::new(),
        };
        if explicit_switch(opts).is_some() && switch_says_off(opts) != budget.is_none() {
            patch.ignored.push("thinking_switch");
        }
        patch
    }

    fn emit_interleaved_beta(&self, _is_official: bool) -> bool {
        // 第三方兼容端点不认 interleaved-thinking beta 时会 400。
        // 调用方传入 is_official_anthropic_endpoint() 结果，但即便官方 Anthropic
        // 路径的兼容模式也按"无 beta"对待。
        false
    }
}

/// 官方 Anthropic（Claude）系列的 effort 档位集合，按型号分档。
///
/// Anthropic 官方协议在 2026 年新增 `output_config.effort`（与 MiniMax 的
/// `/anthropic` 端点同构），但**各型号支持的档位不同**——档位越界会被端点
/// 判为非法参数（400），所以这张表必须逐型号精确，不能一律给五档。
///
/// 依据官方文档「Effort levels」节：
/// * 五档全量（low/medium/high/xhigh/max）：Opus 4.8 / 4.7 / 5 / 5.5、
///   Sonnet 5 / 5.5、Haiku 5.5、Fable 5 / 5.1、Mythos 5 / 5.1、Mythos Preview；
/// * 到 max 为止（无 xhigh）：Opus 4.6、Sonnet 4.6；
/// * 到 high 为止（无 xhigh/max）：Opus 4.5、Sonnet 4.5；
/// * 更早的 3.x / 4.0 系列不支持 effort。
///
/// **判据只认版本段**：`contains("opus")` / `contains("sonnet")` 这类**系列名**
/// 是稳定的；但 `flash` / `fast` 之类营销词不是版本证据，不能参与分档。
///
/// 未识别的型号一律返回**空切片**（不支持 effort）——宁可让 `request_plan`
/// 把用户的 effort 记成可见的 dropped，也不能发一个大概率 400 的参数。
fn anthropic_effort_levels(model: &str) -> &'static [ReasoningEffort] {
    /// 五档全量。
    const ALL: &[ReasoningEffort] = &[
        ReasoningEffort::Low,
        ReasoningEffort::Medium,
        ReasoningEffort::High,
        ReasoningEffort::XHigh,
        ReasoningEffort::Max,
    ];
    /// 到 max 为止（无 xhigh）。
    const TO_MAX: &[ReasoningEffort] = &[
        ReasoningEffort::Low,
        ReasoningEffort::Medium,
        ReasoningEffort::High,
        ReasoningEffort::Max,
    ];
    /// 到 high 为止（无 xhigh/max）。
    const TO_HIGH: &[ReasoningEffort] = &[
        ReasoningEffort::Low,
        ReasoningEffort::Medium,
        ReasoningEffort::High,
    ];

    let m = model.to_ascii_lowercase();
    // 3.x / 4.0 系列在任何 effort 支持之前 —— 早退，避免后面的「4.5」误命中。
    let is_claude = m.contains("claude");
    if !is_claude {
        return &[];
    }
    // Fable / Mythos 是 Anthropic 的虚构系列名（官方文档里的 effort 支持表
    // 同样按它们分档），与 Opus/Sonnet/Haiku 并列。
    let series = ["opus", "sonnet", "haiku", "fable", "mythos"]
        .iter()
        .find(|name| m.contains(*name))
        .copied();
    let Some(series) = series else {
        return &[];
    };

    // 抽出版本号：`claude-opus-4-8-20260514` → `4.8`；`claude-opus-5` → `5`。
    let version = model_version_segment(&m, series);
    match (series, version) {
        // 5 系（Opus 5 / 5.5、Sonnet 5 / 5.5、Haiku 5.5、Fable/Mythos 5/5.1）
        ("opus" | "sonnet" | "haiku" | "fable" | "mythos", Some((major, _))) if major >= 5 => ALL,
        // 4.8 / 4.7：五档全量。
        ("opus", Some((4, Some(8)))) | ("opus", Some((4, Some(7)))) => ALL,
        // 4.6：到 max 为止。
        ("opus" | "sonnet", Some((4, Some(6)))) => TO_MAX,
        // 4.5：到 high 为止。
        ("opus" | "sonnet", Some((4, Some(5)))) => TO_HIGH,
        _ => &[],
    }
}

/// 从模型名里抽出 `<series>` 之后的版本号，返回 `(主版本, 次版本)`。
///
/// 次版本为 `None` 表示只识别到主版本（如 `claude-opus-5`）。
/// 找不到时返回 `None`（调用方据此判「不支持 effort」）。
///
/// **只认 1 位次版本**：真实型号的次版本都是一位（4.5 / 4.6 / 4.7 / 4.8 / 5.5），
/// 而日期段是 8 位（`claude-opus-4-20250514`）。不设这道闸，`claude-3-5-sonnet-
/// 20241022` 这种把日期排在系列名**之后**的老命名会把 20241022 当成主版本，
/// 于是 3.5 型号被误判成「5 系 → 五档全量」。
fn model_version_segment(model: &str, series: &str) -> Option<(u32, Option<u32>)> {
    let after = model.split_once(series)?.1;
    let mut parts = after
        .split(['-', '_', '.'])
        .skip_while(|part| part.is_empty())
        .filter(|part| !part.is_empty());
    let major: u32 = parts.next()?.parse().ok()?;
    // 主版本只可能是 1 位（3 / 4 / 5）；8 位日期段直接排除。
    if major > 9 {
        return None;
    }
    let minor = parts
        .next()
        .filter(|part| part.len() == 1)
        .and_then(|part| part.parse::<u32>().ok());
    Some((major, minor))
}

/// MiniMax `/anthropic` 端点（**仅 M3.1**）支持的 effort 档位全量。
///
/// 同时供 [`ThinkingSpec::minimax_anthropic`] 的 `effort_levels` 与
/// `AnthropicMinimaxAdapter` 使用——两处共用同一份常量，避免「catalog 说支持、
/// adapter 悄悄丢掉」这种自相矛盾（`normalize_effort` 与 adapter 都只认它）。
/// 注意与 OpenAI 线的 `Self::minimax()` 档位集合**不同**：那边刻意避开了
/// `medium` / `xhigh`，`/anthropic` 端点文档则两者都收。
pub(crate) const MINIMAX_M31_ANTHROPIC_EFFORT_LEVELS: &[ReasoningEffort] = &[
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::XHigh,
    ReasoningEffort::Max,
];

/// `MiniMax-M3.1-Flash-Preview` 的模型名识别。
///
/// 该模型的 thinking 由 `output_config.effort` 档位控制、且 `thinking.type=disabled`
/// 会被端点拒绝（400），与 M3 / M2.x 的默认行为都不同，必须单独分派。
///
/// **判据刻意只认版本段**：`contains("flash")` 这类营销词不能当版本证据——
/// 它会把将来的 `M2.x-Flash` / `M3.5-Flash` 全部误路由到 M3.1，进而给这些
/// 端点发它们不认的 `output_config.effort`。分派错误在这里是 400 级事故
/// （把 M3.5 当 M2 会白发 `disabled`），不是退化的性能问题。
fn is_minimax_m31_flash(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    // `MiniMax-M3.1-Flash-Preview` / `minimax-m3-1` 都见过。**不认 `flash`**：
    // 它是营销词而非版本证据，将来 `M2.x-Flash` 会被误路由。
    m.contains("m3.1") || m.contains("m3-1") || m.contains("m31")
}

/// MiniMax `/anthropic` 端点的 thinking 参数形态，按模型代际分派。
///
/// 端点只承认三种 `thinking.type` 取值：**省略** / `adaptive` / `disabled`——
/// `enabled` 与 `budget_tokens` 都不在文档支持范围内。各代际的语义：
///
/// | 模型 | 省略 | `adaptive` | `disabled` |
/// |---|---|---|---|
/// | M3.1-Flash-Preview | 开 | 开 | **400** |
/// | M3 | 关 | 开 | 保持关 |
/// | M2.x | 开 | 开 | 接收但不生效 |
///
/// 思考深度由 `output_config.effort`（`low/medium/high/xhigh/max`）控制，且**只有
/// M3.1-Flash-Preview 支持**；省略时端点默认 `max`。
///
/// 因此本 adapter 的三条硬规则：
/// 1. 绝不发送 `enabled` 或 `budget_tokens`——`thinking_budget` 在这条线协议上
///    没有对应语义，只能落进 `ignored` 提示用户。**而且它连「用户想开思考」
///    这个信号都不算**：若拿它去翻开关，就会出现「一边记为被忽略、一边按它
///    改变行为」的自相矛盾，且对 M3 是把思考从默认关翻成开这种用户没要求的
///    行为翻转。开/关只认 `opts.thinking_switch`（显式意图）。
/// 2. `disabled` 只在 M3 上真的生效；M3.1 发它会 400，M2.x 发了也无效，
///    两者的 `disabled` 请求都降级成「不发 thinking 字段」并记入 `ignored`。
/// 3. `effort` 只在 M3.1 上上送；档位判据统一走 [`normalize_effort`]（即
///    `ThinkingSpec::minimax_anthropic` 的 `effort_levels`），不在本 adapter
///    里维护第二份白名单。
enum AnthropicMinimaxAdapter {
    M31Flash,
    M3,
    M2,
}

impl ThinkingAdapter for AnthropicMinimaxAdapter {
    fn apply_openai(&self, _body: &mut Map<String, Value>, _opts: &RequestOptions) {
        // Anthropic 线协议不走 OpenAI body。
    }

    fn apply_anthropic(
        &self,
        identity: &ModelIdentity,
        opts: &RequestOptions,
    ) -> AnthropicThinkingPatch {
        let mut patch = AnthropicThinkingPatch::default();

        // thinking.type：**只认显式 switch**。不表达意图时字段整个不发——省略
        // 本身就有明确语义（M3.1/M2.x=开、M3=关），比猜一个 `adaptive` 更贴近
        // 端点默认，也省得撞上任何代际对 disabled 的拒绝。
        let switch = explicit_switch(opts);
        patch.thinking = match switch.as_deref() {
            None => None,
            Some(s) if switch_is_off(s) => match self {
                // M3 是唯一「关得掉」的代际。
                Self::M3 => Some(json!({"type": "disabled"})),
                // M3.1 发 disabled 会 400、M2.x 发了也不生效 → 降级成「不发」。
                _ => None,
            },
            Some(_) => Some(json!({"type": "adaptive"})),
        };
        if switch_says_off(opts) && !matches!(self, Self::M3) {
            patch.ignored.push("thinking_switch=disabled");
        }

        // thinking_budget：MiniMax 不用 token 预算控制思考深度，且不得作为
        // 开关信号（见上方规则 1）。
        if opts.thinking_budget.is_some() {
            patch.ignored.push("thinking_budget");
        }

        // output_config.effort：档位判据与 catalog 层共用 `effort_levels`。
        if let Some(raw) = opts.reasoning_effort.as_deref() {
            match normalize_effort(identity, raw) {
                Some(level) => patch.output_config = Some(json!({ "effort": level })),
                None => patch.ignored.push("reasoning_effort"),
            }
        }

        patch
    }

    fn emit_interleaved_beta(&self, _is_official: bool) -> bool {
        false
    }
}

/// 官方 OpenAI（o 系列 / gpt-5 系，Chat Completions 形态）。
///
/// 官方这条线只有一个旋钮：顶层 `reasoning_effort`。因此本 adapter
/// **绝不**写 `thinking` / `enable_thinking` / `thinking_budget`——它们都会
/// 被判成未知字段并 400（官方文档「Unsupported parameters」明确列出
/// `max_tokens` 与 reasoning 模型不兼容，`thinking` 更不在此列）。
///
/// 与 `MoonshotOpenAiAdapter` 的区别值得注意：那边在没有 effort 时会退化写
/// `thinking.type`，直接复用会让官方端点收到它不认识的字段。这也是本 adapter
/// 独立存在而不是复用别人的原因。
///
/// `text_only()`（辅助调用：compact / 记忆提取 / auto 分类器 / 标题摘要）在这条
/// 线上发的是 `thinking_switch = "disabled"`。官方没有开关字段，但**不能就这样
/// 当作没配**——不发字段时端点按默认档位跑满 reasoning，那正是「预算全被思考
/// 吃掉、可见文本为空」的历史故障形态。这里降级成 `reasoning_effort: "low"`：
/// 这是能表达「少想一点」的最接近语义，且所有 o 系列 / gpt-5 系都认。
/// 不写 `"none"`——`o1`/`o3` 不认它，会是 400。
struct OpenAiOfficialAdapter;

impl ThinkingAdapter for OpenAiOfficialAdapter {
    fn apply_openai(&self, body: &mut Map<String, Value>, opts: &RequestOptions) {
        // 档位已在 `apply_thinking_to_openai_body` 里由 `normalize_effort`
        // 收敛过：到这里能拿到的一定是 `spec.effort_levels` 里的值，收到即写入。
        if let Some(effort) = opts.reasoning_effort.as_deref() {
            body.insert(
                "reasoning_effort".to_string(),
                Value::String(effort.to_string()),
            );
            return;
        }
        // 没有可用档位但用户明确要求关思考：o 系列的「关」只能表达成低档位。
        if switch_says_off(opts) {
            body.insert(
                "reasoning_effort".to_string(),
                Value::String(ReasoningEffort::Low.as_str().to_string()),
            );
        }
        // 显式 `enabled` 且无档位：不写任何字段（= 端点默认档位）。
        // 官方没有「开启」字段，写了也没处落，强行塞 `thinking` 只会 400。
    }

    fn emit_interleaved_beta(&self, _is_official: bool) -> bool {
        // interleaved-thinking beta 是 Anthropic 专属；OpenAI 线永远不发。
        false
    }
}

/// DeepSeek：`thinking.type` + 顶层 `reasoning_effort`。
struct DeepSeekOpenAiAdapter;

impl ThinkingAdapter for DeepSeekOpenAiAdapter {
    fn apply_openai(&self, body: &mut Map<String, Value>, opts: &RequestOptions) {
        if let Some(switch_type) = effective_switch_type(opts, "enabled") {
            body.insert("thinking".to_string(), json!({ "type": switch_type }));
        }
        if let Some(effort) = opts
            .reasoning_effort
            .as_deref()
            .and_then(ReasoningEffort::parse)
        {
            body.insert(
                "reasoning_effort".to_string(),
                Value::String(effort.as_str().to_string()),
            );
        }
        // DeepSeek 启用 thinking 后 temperature 等被忽略，主动移除避免歧义。
        // 判据用 `effective_switch_enabled` 而非"有没有 budget/effort"：档位被
        // 丢弃后 thinking 仍然是开着的（降级成 `thinking.type=enabled`）。
        if effective_switch_enabled(opts).unwrap_or(false) {
            body.remove("temperature");
            body.remove("top_p");
        }
    }

    fn emit_interleaved_beta(&self, _is_official: bool) -> bool {
        false
    }
}

/// Qwen：`enable_thinking` + `thinking_budget` 或 `reasoning_effort`（互斥）。
struct QwenOpenAiAdapter;

impl ThinkingAdapter for QwenOpenAiAdapter {
    fn apply_openai(&self, body: &mut Map<String, Value>, opts: &RequestOptions) {
        // Qwen 用顶层 bool `enable_thinking`，但 profile 也可能传 "false"/"true" 字符串
        let switch_type = effective_switch_type(opts, "enabled");
        if let Some(switch_type) = switch_type {
            let enabled = !matches!(switch_type.as_str(), "disabled" | "false");
            body.insert("enable_thinking".to_string(), Value::Bool(enabled));
        }

        // Qwen 互斥规则：thinking_budget 与 reasoning_effort 不能同传
        if let Some(budget) = opts.thinking_budget {
            body.insert("thinking_budget".to_string(), Value::Number(budget.into()));
            // effort 不发
        } else if let Some(effort_str) = opts.reasoning_effort.as_deref() {
            body.insert(
                "reasoning_effort".to_string(),
                Value::String(effort_str.to_string()),
            );
        }
    }

    fn emit_interleaved_beta(&self, _is_official: bool) -> bool {
        false
    }
}

/// GLM/Z.ai：`thinking.type`；启用时锁 temperature。
struct ZhipuOpenAiAdapter;

impl ThinkingAdapter for ZhipuOpenAiAdapter {
    fn apply_openai(&self, body: &mut Map<String, Value>, opts: &RequestOptions) {
        if let Some(switch_type) = effective_switch_type(opts, "enabled") {
            body.insert("thinking".to_string(), json!({ "type": switch_type }));
        }
        // GLM 启用 thinking 后 temperature/top_p 被忽略，主动移除
        if effective_switch_enabled(opts).unwrap_or(false) {
            body.remove("temperature");
            body.remove("top_p");
        }
    }

    fn emit_interleaved_beta(&self, _is_official: bool) -> bool {
        false
    }
}

/// Doubao：`thinking.type` + `thinking.budget_tokens`。
struct VolcengineOpenAiAdapter;

impl ThinkingAdapter for VolcengineOpenAiAdapter {
    fn apply_openai(&self, body: &mut Map<String, Value>, opts: &RequestOptions) {
        if let Some(switch_type) = effective_switch_type(opts, "enabled") {
            let mut thinking = json!({ "type": switch_type });
            if let Some(budget) = opts.thinking_budget {
                // Doubao budget 经验上限 8192；超过时夹到 8192 而非 400
                let clamped = budget.min(8192);
                thinking["budget_tokens"] = json!(clamped);
            }
            body.insert("thinking".to_string(), thinking);
        }
    }

    fn emit_interleaved_beta(&self, _is_official: bool) -> bool {
        false
    }
}

/// Moonshot：k3 走 effort，其余走 thinking.type。
struct MoonshotOpenAiAdapter;

impl ThinkingAdapter for MoonshotOpenAiAdapter {
    fn apply_openai(&self, body: &mut Map<String, Value>, opts: &RequestOptions) {
        // 通过 opts 中携带的 vendor 信息推断 k3；这里简化为：若 reasoning_effort 已
        // 显式设置且 spec 为 EffortOnly（adapter 选型决定），则走 effort 路径。
        // 实际判别在 catalog 阶段，adapter 自身只关心 wire 字段名。
        if let Some(effort_str) = opts.reasoning_effort.as_deref() {
            body.insert(
                "reasoning_effort".to_string(),
                Value::String(effort_str.to_string()),
            );
        } else if let Some(switch_type) = effective_switch_type(opts, "enabled") {
            body.insert("thinking".to_string(), json!({ "type": switch_type }));
        }
    }

    fn emit_interleaved_beta(&self, _is_official: bool) -> bool {
        false
    }
}

/// MiniMax：`thinking.type`（M2.x 强制 adaptive；M3 接受 enabled/disabled）。
/// M3 同时支持顶层 `reasoning_effort`（low / high / max），与 switch 分支独立
/// 写入：user 既可以单独配 effort（adapter 默认 `thinking.type=enabled`），
/// 也可以单独配 switch（effort 不上送）。
///
/// M2.x 与 M3 形态差异显著：M2 SwitchOnly 强制开且**不**支持 effort（M2 服务端
/// 不认顶层 `reasoning_effort`，发出去会 400）；M3 SwitchPlusEffort 支持 effort
/// 但仅限 spec.effort_levels = [Low, High, Max] 列出的档位（其它如 xhigh、
/// adaptive 被 parse 后仍需过滤，避免 server 端不认）。
///
/// 单个 `&'static` adapter 实例无法携带 model 信息，因此拆成 enum 两个分支，
/// `adapter_for` 按 model 名（含 m3）分派到对应 `&'static MinimaxAdapter`。
enum MinimaxAdapter {
    M2,
    M3,
}

impl ThinkingAdapter for MinimaxAdapter {
    fn apply_openai(&self, body: &mut Map<String, Value>, opts: &RequestOptions) {
        if let Some(switch_type) = effective_switch_type(opts, "enabled") {
            body.insert("thinking".to_string(), json!({ "type": switch_type }));
            // MiniMax 启用 reasoning_split 让 thinking 与 content 分字段返回
            body.insert("reasoning_split".to_string(), Value::Bool(true));
        }
        // 只在 M3 路径上送 reasoning_effort，且必须命中 spec.effort_levels：
        //   MINIMAX_M3_EFFORT_LEVELS = [Low, High, Max]（对齐 Moonshot k3）
        // 其它已识别档位（xhigh / adaptive 等）即使 parse 成功也丢弃，
        // 避免 server 端不认返回 400。
        if matches!(self, Self::M3) {
            if let Some(effort) = opts
                .reasoning_effort
                .as_deref()
                .and_then(ReasoningEffort::parse)
            {
                const MINIMAX_M3_EFFORT_LEVELS: &[ReasoningEffort] = &[
                    ReasoningEffort::Low,
                    ReasoningEffort::High,
                    ReasoningEffort::Max,
                ];
                if MINIMAX_M3_EFFORT_LEVELS.contains(&effort) {
                    body.insert(
                        "reasoning_effort".to_string(),
                        Value::String(effort.as_str().to_string()),
                    );
                }
            }
        }
    }

    fn emit_interleaved_beta(&self, _is_official: bool) -> bool {
        false
    }
}

/// Ollama：仅当 model 名含 "think" 时发 `think: true`。
struct OllamaOpenAiAdapter;

impl ThinkingAdapter for OllamaOpenAiAdapter {
    fn apply_openai(&self, body: &mut Map<String, Value>, opts: &RequestOptions) {
        if effective_switch_enabled(opts).unwrap_or(false) {
            body.insert("think".to_string(), Value::Bool(true));
        }
    }

    fn emit_interleaved_beta(&self, _is_official: bool) -> bool {
        false
    }
}

/// vLLM：`chat_template_kwargs.enable_thinking`。
///
/// vLLM 没有 vendor 私有的顶层 thinking 字段，开思考靠把开关传给 chat
/// template（`chat_template_kwargs`）——这正是 vLLM 文档给 Qwen3 系模型的做法。
/// 注意 `reasoning_effort` 在 vLLM 里只是透传给 template 的一个变量名，各家
/// 模板并不都消费它，所以 `vllm()` spec 的 `effort_levels` 保持为空：用户填的
/// effort 会被 `normalize_effort` 记成 dropped_parameter，而不是发一个大概率被
/// 忽略的字段。
struct VllmOpenAiAdapter;

impl ThinkingAdapter for VllmOpenAiAdapter {
    fn apply_openai(&self, body: &mut Map<String, Value>, opts: &RequestOptions) {
        // 与 `effective_switch_type` 同语义，但 vLLM 的取值是 bool 而非字符串。
        if effective_switch_enabled(opts).unwrap_or(false) {
            body.insert(
                "chat_template_kwargs".to_string(),
                json!({ "enable_thinking": true }),
            );
        }
    }

    fn emit_interleaved_beta(&self, _is_official: bool) -> bool {
        false
    }
}

/// Generic OpenAI：不主动发 thinking；即使用户 profile 配置了 budget/effort，
/// 也不主动猜测未知 vendor 支持什么字段——保持请求体不变。
/// 阶段 7 字段名版本探测启用后，会先发探测请求再决定是否回填 thinking。
struct GenericOpenAiAdapter;

impl ThinkingAdapter for GenericOpenAiAdapter {
    fn apply_openai(&self, _body: &mut Map<String, Value>, _opts: &RequestOptions) {
        // 故意空：generic 兜底不应假设未知 vendor 支持 thinking。
        // 已有 RequestPlan.from_capabilities 在 capabilities.thinking == Unsupported
        // 时把 reasoning 设成 Disabled 并写 dropped_parameter（用户可见）。
    }

    fn emit_interleaved_beta(&self, _is_official: bool) -> bool {
        false
    }
}

// ── 自由函数：provider 调用入口 ──────────────────────────────────────────────

/// **只读 profile 显式指定的 switch**，不做「配了 budget/effort ⇒ 想开」的推断。
///
/// 与 [`effective_switch_type`] 的区别就是后者多出的那层推断。分派时必须先问
/// 「用户说了什么」再问「默认怎么开」——混在一起会导致「记为被忽略的参数」反过来
/// 又被当成开启信号（`thinking_budget` 在 MiniMax `/anthropic` 上就踩过这个坑）。
fn explicit_switch(opts: &RequestOptions) -> Option<String> {
    opts.thinking_switch
        .as_deref()
        .map(|s| s.trim().to_ascii_lowercase())
}

/// `switch` 字符串里的「关闭」取值。各 vendor 对关的表达不统一
/// （GLM/Qwen 用 `disabled`，部分 profile 直接写 `false`），判据必须同源。
fn switch_is_off(switch: &str) -> bool {
    matches!(switch, "disabled" | "false")
}

/// `explicit_switch` 的谓词版本：「显式要求关闭」。
fn switch_says_off(opts: &RequestOptions) -> bool {
    explicit_switch(opts).is_some_and(|s| switch_is_off(&s))
}

/// 推导 effective `thinking.type` 字符串。
///
/// 返回 `None` 表示"用户没显式指定，vendor 自己决定"（GLM adaptive 等场景）。
/// 返回 `Some(s)` 表示"应该写到 body 里"。
///
/// 优先级：
/// 1. `opts.thinking_switch`（profile 显式覆盖）→ Some(trim+lowercase)
/// 2. 用户开了 budget/effort → Some(default)
/// 3. 否则 → None（不写 thinking 字段）
fn effective_switch_type(opts: &RequestOptions, default: &str) -> Option<String> {
    if let Some(explicit) = explicit_switch(opts) {
        return Some(explicit);
    }
    if opts.thinking_budget.is_some() || opts.reasoning_effort.is_some() {
        Some(default.to_string())
    } else {
        None
    }
}

/// `effective_switch_type` 的布尔版本，供只认开/关（不认档位）的 vendor 使用：
/// Ollama `think` / vLLM `chat_template_kwargs.enable_thinking`。
/// 返回 `None` 表示"用户没说"，此时不写任何字段。
fn effective_switch_enabled(opts: &RequestOptions) -> Option<bool> {
    if let Some(explicit) = explicit_switch(opts) {
        return Some(!switch_is_off(&explicit));
    }
    if opts.thinking_budget.is_some() || opts.reasoning_effort.is_some() {
        return Some(true);
    }
    None
}

/// OpenAI provider 调用的统一入口。在 `OpenAIProvider::stream` 序列化 body 后调用。
/// 当 `ThinkingSpec::for_vendor(...).control == Disabled` 时直接返回，不改 body
/// （generic 与不支持的 vendor 都不会自作主张塞 thinking 字段）。
pub fn apply_thinking_to_openai_body(
    identity: &ModelIdentity,
    body: &mut Map<String, Value>,
    opts: &RequestOptions,
) {
    let spec = ThinkingSpec::for_vendor(&identity.vendor, &identity.wire_protocol, &identity.model);
    if matches!(spec.control, ThinkingControl::Disabled) {
        return;
    }
    let adapter = adapter_for(identity);
    // effort 在这里收敛一次：不在 `spec.effort_levels` 内的档位（含 vendor 整体
    // 不支持 effort 的情况）不进 body。adapter 因此可以放心地"收到即写入"，
    // 不会再出现 catalog 说支持、adapter 悄悄丢掉的半吊子状态。
    let mut sanitized = opts.clone();
    if let Some(raw) = opts.reasoning_effort.as_deref() {
        sanitized.reasoning_effort = normalize_effort(identity, raw).map(str::to_string);
        // 档位发不出去 ≠ 不思考。GLM / Ollama / Moonshot k2 / vLLM 这些只认开关的
        // vendor，用户填 effort 的意图就是"开思考"——退化成 `thinking.type=enabled`
        // 才不会让思考凭空消失（丢弃档位这件事本身记在 `dropped_parameter` 里）。
        if sanitized.reasoning_effort.is_none() && sanitized.thinking_switch.is_none() {
            sanitized.thinking_switch = Some("enabled".to_string());
        }
    }
    // **EffortOnly 且档位集合为空**：该型号既不认 `reasoning_effort`，也没有任何
    // 开关字段可写（`o1-mini` / `o3-mini` 正是这种）。此时必须**什么都不发**。
    // 不清掉 `thinking_switch`，官方 adapter 会把 `text_only()` 的 `disabled`
    // 降级成 `reasoning_effort: "low"` —— 而这两代不认该字段，直接 400。
    if matches!(spec.control, ThinkingControl::EffortOnly) && spec.effort_levels.is_empty() {
        sanitized.thinking_switch = None;
    }
    adapter.apply_openai(body, &sanitized);
}

/// Anthropic provider 调用的统一入口。返回是否发 `interleaved-thinking-2025-05-14` beta。
pub fn should_emit_interleaved_beta(identity: &ModelIdentity, is_official_anthropic: bool) -> bool {
    let adapter = adapter_for(identity);
    adapter.emit_interleaved_beta(is_official_anthropic)
}

// ── 测试 ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::ModelIdentity;

    fn identity(vendor: &str, wire: WireProtocol, model: &str) -> ModelIdentity {
        ModelIdentity {
            vendor: vendor.to_string(),
            model: model.to_string(),
            base_url: "https://example.com".to_string(),
            wire_protocol: wire,
        }
    }

    #[test]
    fn normalize_effort_respects_per_vendor_levels() {
        let deepseek = identity(
            "deepseek",
            WireProtocol::OpenAiChatCompletions,
            "deepseek-v4-pro",
        );
        assert_eq!(normalize_effort(&deepseek, " High "), Some("high"));
        // DeepSeek 只认 low/medium/high/max：xhigh / adaptive 发出去就是 400。
        assert_eq!(normalize_effort(&deepseek, "xhigh"), None);
        assert_eq!(normalize_effort(&deepseek, "adaptive"), None);

        let qwen = identity(
            "alibaba",
            WireProtocol::OpenAiChatCompletions,
            "qwen3-coder-plus",
        );
        assert_eq!(normalize_effort(&qwen, "xhigh"), Some("xhigh"));

        let k3 = identity("moonshot", WireProtocol::OpenAiChatCompletions, "kimi-k3");
        assert_eq!(normalize_effort(&k3, "high"), Some("high"));
        assert_eq!(normalize_effort(&k3, "medium"), None);

        // 只认开关的 vendor 一律不收 effort。
        for (vendor, model) in [
            ("zhipu", "glm-4.6"),
            ("volcengine", "doubao-seed-code"),
            ("ollama", "qwen3-think"),
            ("vllm", "deepseek-r1-distill-reasoning"),
            ("moonshot", "kimi-k2.5"),
            ("minimax", "MiniMax-M2"),
        ] {
            let id = identity(vendor, WireProtocol::OpenAiChatCompletions, model);
            assert!(!supports_effort(&id), "{vendor}/{model} 不该支持 effort");
            assert_eq!(normalize_effort(&id, "high"), None);
        }

        assert!(supports_effort(&deepseek));
        assert!(supports_effort(&qwen));
        assert!(supports_effort(&k3));
        assert!(supports_effort(&identity(
            "minimax",
            WireProtocol::OpenAiChatCompletions,
            "MiniMax-M3"
        )));
    }

    /// 不在 `effort_levels` 内的档位不能进 body——用户填错时是"不发"，
    /// 而不是让服务端回 400。
    #[test]
    fn unsupported_effort_level_never_reaches_body() {
        let id = identity(
            "deepseek",
            WireProtocol::OpenAiChatCompletions,
            "deepseek-v4-pro",
        );
        let mut body = Map::new();
        let opts = RequestOptions {
            max_tokens: 1024,
            reasoning_effort: Some("xhigh".to_string()),
            ..Default::default()
        };
        apply_thinking_to_openai_body(&id, &mut body, &opts);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert!(body.get("reasoning_effort").is_none(), "{body:?}");
    }

    /// vLLM 此前没有 adapter 分支，用户配的 thinking 开关被 Generic 兜底静默丢掉。
    /// 它认的是 `chat_template_kwargs.enable_thinking`。
    #[test]
    fn vllm_thinking_goes_through_chat_template_kwargs() {
        let id = identity(
            "vllm",
            WireProtocol::OpenAiChatCompletions,
            "deepseek-r1-distill-reasoning",
        );
        let mut body = Map::new();
        let opts = RequestOptions {
            max_tokens: 1024,
            reasoning_effort: Some("high".to_string()),
            ..Default::default()
        };
        apply_thinking_to_openai_body(&id, &mut body, &opts);
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], true);

        let mut body = Map::new();
        let opts = RequestOptions {
            max_tokens: 1024,
            thinking_switch: Some("disabled".to_string()),
            ..Default::default()
        };
        apply_thinking_to_openai_body(&id, &mut body, &opts);
        assert!(body.is_empty(), "显式关闭时不应留下任何字段: {body:?}");

        // 非 reasoning 模型：control = Disabled，body 完全不动。
        let plain = identity("vllm", WireProtocol::OpenAiChatCompletions, "qwen3");
        let mut body = Map::new();
        apply_thinking_to_openai_body(&plain, &mut body, &opts);
        assert!(body.is_empty());
    }

    /// Moonshot k2.5 不认顶层 `reasoning_effort`（只认 `thinking.type`）：
    /// 档位必须在这里就被挡掉，不能等 adapter 分支二选一。
    #[test]
    fn moonshot_k2_effort_falls_back_to_switch_only() {
        let id = identity("moonshot", WireProtocol::OpenAiChatCompletions, "kimi-k2.5");
        let mut body = Map::new();
        let opts = RequestOptions {
            max_tokens: 1024,
            reasoning_effort: Some("high".to_string()),
            ..Default::default()
        };
        apply_thinking_to_openai_body(&id, &mut body, &opts);
        assert!(body.get("reasoning_effort").is_none(), "{body:?}");
        assert_eq!(body["thinking"]["type"], "enabled");
    }

    #[test]
    fn reasoning_effort_parse_round_trip() {
        for s in [
            "minimal", "low", "medium", "high", "max", "xhigh", "adaptive",
        ] {
            let parsed = ReasoningEffort::parse(s).unwrap();
            assert_eq!(parsed.as_str(), s);
        }
        assert!(ReasoningEffort::parse("unknown").is_none());
    }

    #[test]
    fn vendor_normalization_aliases() {
        assert_eq!(normalize_vendor("GLM"), "zhipu");
        assert_eq!(normalize_vendor("z.ai"), "zhipu");
        assert_eq!(normalize_vendor("Kimi"), "moonshot");
        assert_eq!(normalize_vendor("Moonshot"), "moonshot");
        assert_eq!(normalize_vendor("Qwen"), "alibaba");
        assert_eq!(normalize_vendor("Bailian"), "alibaba");
        assert_eq!(normalize_vendor("doubao"), "volcengine");
        assert_eq!(normalize_vendor("ark"), "volcengine");
        assert_eq!(normalize_vendor("MiniMax"), "minimax");
        assert_eq!(normalize_vendor("DeepSeek"), "deepseek");
    }

    #[test]
    fn anthropic_native_spec_is_budget_only() {
        let spec = ThinkingSpec::for_vendor(
            "anthropic",
            &WireProtocol::AnthropicMessages,
            "claude-opus-4-8",
        );
        assert_eq!(spec.control, ThinkingControl::BudgetOnly);
        assert!(spec.budget_tokens_supported);
        assert!(spec.user_can_disable);
        // Opus 4.8 官方支持 effort，且 budget 与 effort 是两个独立旋钮（可共存）。
        assert!(spec.budget_and_effort_coexist);
        assert_eq!(
            spec.effort_levels, MINIMAX_M31_ANTHROPIC_EFFORT_LEVELS,
            "Opus 4.8 与 M3.1 一样是五档全量"
        );
    }

    #[test]
    fn anthropic_compatible_spec_omits_interleaved_beta() {
        let adapter = adapter_for(&identity(
            "zhipu",
            WireProtocol::AnthropicMessages,
            "glm-4.6",
        ));
        assert!(!adapter.emit_interleaved_beta(false));
        assert!(!adapter.emit_interleaved_beta(true));
    }

    #[test]
    fn anthropic_official_emits_interleaved_beta() {
        let adapter = adapter_for(&identity(
            "anthropic",
            WireProtocol::AnthropicMessages,
            "claude-opus-4-8",
        ));
        assert!(adapter.emit_interleaved_beta(true));
    }

    #[test]
    fn deepseek_hybrid_uses_switch_plus_effort() {
        let spec = ThinkingSpec::for_vendor(
            "deepseek",
            &WireProtocol::OpenAiChatCompletions,
            "deepseek-v4-pro",
        );
        assert_eq!(spec.control, ThinkingControl::SwitchPlusEffort);
        assert!(spec.user_can_disable);
        assert!(!spec.ignore_disabled_when_forced);
    }

    #[test]
    fn deepseek_reasoner_is_forced_on() {
        let spec = ThinkingSpec::for_vendor(
            "deepseek",
            &WireProtocol::OpenAiChatCompletions,
            "deepseek-reasoner",
        );
        assert!(!spec.user_can_disable);
        assert!(spec.ignore_disabled_when_forced);
    }

    #[test]
    fn qwen_thinking_suffix_is_forced_on() {
        let spec =
            ThinkingSpec::for_vendor("alibaba", &WireProtocol::OpenAiChatCompletions, "qwq-plus");
        assert!(!spec.user_can_disable);
        assert_eq!(spec.control, ThinkingControl::SwitchPlusBudget);
    }

    #[test]
    fn glm_openai_is_switch_only_and_locks_sampling() {
        let spec =
            ThinkingSpec::for_vendor("zhipu", &WireProtocol::OpenAiChatCompletions, "glm-4.6");
        assert_eq!(spec.control, ThinkingControl::SwitchOnly);
        assert!(!spec.budget_tokens_supported);
    }

    #[test]
    fn doubao_thinking_suffix_is_forced_on() {
        let spec = ThinkingSpec::for_vendor(
            "volcengine",
            &WireProtocol::OpenAiChatCompletions,
            "doubao-1.5-thinking-pro",
        );
        assert!(!spec.user_can_disable);
    }

    #[test]
    fn moonshot_k3_uses_effort_only() {
        let spec =
            ThinkingSpec::for_vendor("moonshot", &WireProtocol::OpenAiChatCompletions, "kimi-k3");
        assert_eq!(spec.control, ThinkingControl::EffortOnly);
    }

    #[test]
    fn moonshot_k2_5_uses_switch_only() {
        let spec = ThinkingSpec::for_vendor(
            "moonshot",
            &WireProtocol::OpenAiChatCompletions,
            "kimi-k2.5",
        );
        assert_eq!(spec.control, ThinkingControl::SwitchOnly);
    }

    #[test]
    fn moonshot_thinking_suffix_is_forced_on() {
        let spec = ThinkingSpec::for_vendor(
            "moonshot",
            &WireProtocol::OpenAiChatCompletions,
            "kimi-k2-thinking",
        );
        assert!(!spec.user_can_disable);
    }

    #[test]
    fn minimax_m2_is_forced_on_with_details() {
        let spec = ThinkingSpec::for_vendor(
            "minimax",
            &WireProtocol::OpenAiChatCompletions,
            "MiniMax-M2",
        );
        assert!(!spec.user_can_disable);
        assert!(spec.response_fields.reasoning_details);
    }

    #[test]
    fn minimax_m3_can_be_disabled() {
        let spec = ThinkingSpec::for_vendor(
            "minimax",
            &WireProtocol::OpenAiChatCompletions,
            "MiniMax-M3",
        );
        assert!(spec.user_can_disable);
        // M3 从 v0.2.18 起走 SwitchPlusEffort：保留 thinking.type 开关同时
        // 接受 reasoning_effort 档位（low/high/max，对齐 Moonshot k3）。
        assert_eq!(spec.control, ThinkingControl::SwitchPlusEffort);
        assert_eq!(
            spec.effort_levels,
            &[
                ReasoningEffort::Low,
                ReasoningEffort::High,
                ReasoningEffort::Max
            ][..]
        );
        assert!(!spec.budget_tokens_supported);
        assert!(!spec.ignore_disabled_when_forced);
        assert!(spec.response_fields.reasoning_details);
    }

    /// M3 路径：switch + effort 两条字段独立写入 body。user 既可以单独配 switch、
    /// 也可以单独配 effort（adapter 因 effort 非空回落到默认 enabled），也可以两者
    /// 都配。M3 从 v0.2.18 起走 SwitchPlusEffort（与 DeepSeek / Moonshot k3 同形态）。
    #[test]
    fn minimax_m3_emits_switch_and_effort_independently() {
        let id = identity("minimax", WireProtocol::OpenAiChatCompletions, "MiniMax-M3");
        let adapter = adapter_for(&id);

        // switch + effort 同时配：两条字段独立写入。
        let mut body = Map::new();
        let opts = RequestOptions {
            max_tokens: 1024,
            reasoning_effort: Some("high".to_string()),
            thinking_switch: Some("enabled".to_string()),
            ..Default::default()
        };
        adapter.apply_openai(&mut body, &opts);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_split"], Value::Bool(true));
        assert_eq!(body["reasoning_effort"], "high");

        // 只配 effort：effective_switch_type 因 effort 非空回落到 "enabled"，
        // body 同时含 thinking + reasoning_split + reasoning_effort。
        let mut body = Map::new();
        let opts = RequestOptions {
            max_tokens: 1024,
            reasoning_effort: Some("max".to_string()),
            ..Default::default()
        };
        adapter.apply_openai(&mut body, &opts);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_split"], Value::Bool(true));
        assert_eq!(body["reasoning_effort"], "max");

        // 只配 switch：effort 不上送（保持 switch 与 effort 独立）。
        let mut body = Map::new();
        let opts = RequestOptions {
            max_tokens: 1024,
            thinking_switch: Some("enabled".to_string()),
            ..Default::default()
        };
        adapter.apply_openai(&mut body, &opts);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_split"], Value::Bool(true));
        assert!(body.get("reasoning_effort").is_none());
    }

    /// M3 effort 档位必须能被 ReasoningEffort::parse 识别；非法字符串（不在
    /// spec.effort_levels 内的）被 and_then(parse) 静默丢弃，避免 400。
    #[test]
    fn minimax_m3_effort_levels_round_trip() {
        let adapter = adapter_for(&identity(
            "minimax",
            WireProtocol::OpenAiChatCompletions,
            "MiniMax-M3",
        ));
        for effort in ["low", "high", "max"] {
            let mut body = Map::new();
            let opts = RequestOptions {
                max_tokens: 1024,
                reasoning_effort: Some(effort.to_string()),
                ..Default::default()
            };
            adapter.apply_openai(&mut body, &opts);
            assert_eq!(body["reasoning_effort"], effort, "档位 {effort} 应原样上送");
        }
        // 非法档位（xhigh 不在 M3 档位列表里）：parse 失败，adapter 不上送。
        let mut body = Map::new();
        let opts = RequestOptions {
            max_tokens: 1024,
            reasoning_effort: Some("xhigh".to_string()),
            ..Default::default()
        };
        adapter.apply_openai(&mut body, &opts);
        assert!(
            body.get("reasoning_effort").is_none(),
            "xhigh 不在 M3 档位列表，应被 parse 过滤: {body:?}"
        );
    }

    /// M2.x 仍是 SwitchOnly 强制开；adapter 不上送 reasoning_effort——
    /// 冻结当前 M2 行为，避免 M2 误支持 effort（M2 服务端若不认 effort 字段会 400）。
    #[test]
    fn minimax_m2_apply_does_not_emit_effort() {
        let id = identity("minimax", WireProtocol::OpenAiChatCompletions, "MiniMax-M2");
        let adapter = adapter_for(&id);
        let mut body = Map::new();
        let opts = RequestOptions {
            max_tokens: 1024,
            reasoning_effort: Some("high".to_string()),
            thinking_switch: Some("enabled".to_string()),
            ..Default::default()
        };
        adapter.apply_openai(&mut body, &opts);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_split"], Value::Bool(true));
        assert!(
            body.get("reasoning_effort").is_none(),
            "M2 SwitchOnly 不应上送 reasoning_effort: {body:?}"
        );
    }

    /// M3 + `thinking_switch = "disabled"`：body 写 thinking.type="disabled"，
    /// 按当前 adapter 实现同时写 reasoning_split=true（关掉也写，
    /// 让服务端回包时仍走分字段路径；这是当前代码事实，不是建议）。
    /// 该测试冻结这个行为，避免后续静默改动。
    #[test]
    fn minimax_m3_disabled_switch_writes_disabled_type_and_split() {
        let adapter = adapter_for(&identity(
            "minimax",
            WireProtocol::OpenAiChatCompletions,
            "MiniMax-M3",
        ));
        let mut body = Map::new();
        let opts = RequestOptions {
            max_tokens: 1024,
            thinking_switch: Some("disabled".to_string()),
            ..Default::default()
        };
        adapter.apply_openai(&mut body, &opts);
        assert_eq!(body["thinking"]["type"], "disabled");
        assert_eq!(body["reasoning_split"], Value::Bool(true));
    }

    /// M3 + 完全没设任何开关：effective_switch_type 返回 None，body 不动。
    /// 这是 SwitchOnly vendor 的"用户没说就什么都不发"基线。
    #[test]
    fn minimax_m3_no_switch_leaves_body_untouched() {
        let adapter = adapter_for(&identity(
            "minimax",
            WireProtocol::OpenAiChatCompletions,
            "MiniMax-M3",
        ));
        let mut body = Map::new();
        let opts = RequestOptions {
            max_tokens: 1024,
            ..Default::default()
        };
        adapter.apply_openai(&mut body, &opts);
        assert!(body.is_empty(), "不应写入任何 thinking 字段: {body:?}");
    }

    /// MiniMax 走 OpenAI 协议——interleaved-thinking beta 永远不发，
    /// 即便 base_url 指向官方 Anthropic 兼容端点也不发。
    #[test]
    fn minimax_never_emits_interleaved_beta() {
        let adapter = adapter_for(&identity(
            "minimax",
            WireProtocol::OpenAiChatCompletions,
            "MiniMax-M3",
        ));
        assert!(!adapter.emit_interleaved_beta(false));
        assert!(!adapter.emit_interleaved_beta(true));
    }

    #[test]
    fn ollama_think_model_uses_switch_only() {
        let spec = ThinkingSpec::for_vendor(
            "ollama",
            &WireProtocol::OpenAiChatCompletions,
            "qwen3-think",
        );
        assert_eq!(spec.control, ThinkingControl::SwitchOnly);
    }

    #[test]
    fn ollama_non_think_model_is_disabled() {
        let spec =
            ThinkingSpec::for_vendor("ollama", &WireProtocol::OpenAiChatCompletions, "qwen3");
        assert_eq!(spec.control, ThinkingControl::Disabled);
    }

    #[test]
    fn vllm_thinking_model_is_switch_only() {
        let spec = ThinkingSpec::for_vendor(
            "vllm",
            &WireProtocol::OpenAiChatCompletions,
            "deepseek-r1-distill-reasoning",
        );
        assert_eq!(spec.control, ThinkingControl::SwitchOnly);
    }

    #[test]
    fn unknown_vendor_openai_falls_back_to_generic_disabled() {
        let spec =
            ThinkingSpec::for_vendor("unknown", &WireProtocol::OpenAiChatCompletions, "mystery");
        assert_eq!(spec.control, ThinkingControl::Disabled);
    }

    #[test]
    fn openai_responses_falls_back_to_generic() {
        let spec = ThinkingSpec::for_vendor("openai", &WireProtocol::OpenAiResponses, "gpt-5");
        assert_eq!(spec.control, ThinkingControl::Disabled);
    }

    // ── apply_openai 行为 ──

    #[test]
    fn deepseek_apply_with_effort_emits_switch_and_effort() {
        let id = identity(
            "deepseek",
            WireProtocol::OpenAiChatCompletions,
            "deepseek-v4-pro",
        );
        let mut body = Map::new();
        let opts = RequestOptions {
            max_tokens: 1024,
            reasoning_effort: Some("high".to_string()),
            ..Default::default()
        };
        apply_thinking_to_openai_body(&id, &mut body, &opts);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_effort"], "high");
        // DeepSeek 启用后 temperature 应被移除（profile 默认值）
    }

    #[test]
    fn qwen_budget_and_effort_are_mutually_exclusive() {
        let id = identity(
            "alibaba",
            WireProtocol::OpenAiChatCompletions,
            "qwen3-coder-plus",
        );
        let mut body = Map::new();

        // 给 budget
        let opts = RequestOptions {
            max_tokens: 1024,
            thinking_budget: Some(4096),
            ..Default::default()
        };
        apply_thinking_to_openai_body(&id, &mut body, &opts);
        assert_eq!(body["enable_thinking"], true);
        assert_eq!(body["thinking_budget"], 4096);
        assert!(body.get("reasoning_effort").is_none());

        // 给 effort
        let mut body = Map::new();
        let opts = RequestOptions {
            max_tokens: 1024,
            reasoning_effort: Some("medium".to_string()),
            ..Default::default()
        };
        apply_thinking_to_openai_body(&id, &mut body, &opts);
        assert_eq!(body["enable_thinking"], true);
        assert_eq!(body["reasoning_effort"], "medium");
        assert!(body.get("thinking_budget").is_none());
    }

    #[test]
    fn glm_with_budget_locks_sampling_params() {
        let id = identity("zhipu", WireProtocol::OpenAiChatCompletions, "glm-4.6");
        let mut body = Map::new();
        body.insert("temperature".to_string(), json!(0.6));
        body.insert("top_p".to_string(), json!(0.9));
        let opts = RequestOptions {
            max_tokens: 1024,
            reasoning_effort: Some("medium".to_string()),
            ..Default::default()
        };
        apply_thinking_to_openai_body(&id, &mut body, &opts);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert!(body.get("temperature").is_none());
        assert!(body.get("top_p").is_none());
    }

    #[test]
    fn doubao_clamps_budget_above_8192() {
        let id = identity(
            "volcengine",
            WireProtocol::OpenAiChatCompletions,
            "doubao-seed-code",
        );
        let mut body = Map::new();
        let opts = RequestOptions {
            max_tokens: 1024,
            thinking_budget: Some(20_000),
            ..Default::default()
        };
        apply_thinking_to_openai_body(&id, &mut body, &opts);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 8192);
    }

    #[test]
    fn minimax_m2_emit_reasoning_split() {
        let id = identity("minimax", WireProtocol::OpenAiChatCompletions, "MiniMax-M2");
        let mut body = Map::new();
        let opts = RequestOptions {
            max_tokens: 1024,
            reasoning_effort: Some("medium".to_string()),
            ..Default::default()
        };
        apply_thinking_to_openai_body(&id, &mut body, &opts);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_split"], true);
    }

    #[test]
    fn explicit_thinking_switch_overrides_default() {
        let id = identity(
            "deepseek",
            WireProtocol::OpenAiChatCompletions,
            "deepseek-v4-pro",
        );
        let mut body = Map::new();
        let opts = RequestOptions {
            max_tokens: 1024,
            thinking_switch: Some("auto".to_string()),
            ..Default::default()
        };
        apply_thinking_to_openai_body(&id, &mut body, &opts);
        assert_eq!(body["thinking"]["type"], "auto");
    }

    #[test]
    fn ollama_think_model_emits_think_true() {
        let id = identity("ollama", WireProtocol::OpenAiChatCompletions, "qwen3-think");
        let mut body = Map::new();
        let opts = RequestOptions {
            max_tokens: 1024,
            reasoning_effort: Some("low".to_string()),
            ..Default::default()
        };
        apply_thinking_to_openai_body(&id, &mut body, &opts);
        assert_eq!(body["think"], true);
    }

    #[test]
    fn ollama_non_think_model_emits_nothing() {
        let id = identity("ollama", WireProtocol::OpenAiChatCompletions, "qwen3");
        let mut body = Map::new();
        let opts = RequestOptions {
            max_tokens: 1024,
            reasoning_effort: Some("low".to_string()),
            ..Default::default()
        };
        apply_thinking_to_openai_body(&id, &mut body, &opts);
        assert!(body.is_empty());
    }

    #[test]
    fn generic_openai_does_not_invent_thinking_field() {
        // Generic 兜底不应自作主张塞 thinking.type=enabled；catalog 阶段
        // 已经把 unknown vendor 的 capabilities.thinking 标成 Unsupported，
        // RequestPlan.from_capabilities 会把 reasoning 设成 Disabled 并写
        // dropped_parameter（用户可见）。这里验证 adapter 真的不动 body。
        let id = identity("custom", WireProtocol::OpenAiChatCompletions, "mystery");
        let mut body = Map::new();
        let opts = RequestOptions {
            max_tokens: 1024,
            reasoning_effort: Some("low".to_string()),
            ..Default::default()
        };
        apply_thinking_to_openai_body(&id, &mut body, &opts);
        assert!(body.is_empty());
    }

    // ── MiniMax `/anthropic` 端点的 thinking 形态 ──
    //
    // 端点只认省略 / `adaptive` / `disabled`，思考深度走 `output_config.effort`。
    // 这些测试锁住「绝不发 enabled / budget_tokens」这条硬约束。

    fn minimax_anthropic(model: &str) -> ModelIdentity {
        identity("minimax", WireProtocol::AnthropicMessages, model)
    }

    #[test]
    fn minimax_m31_maps_effort_to_output_config_not_budget_tokens() {
        let id = minimax_anthropic("MiniMax-M3.1-Flash-Preview");
        let opts = RequestOptions {
            max_tokens: 32000,
            thinking_budget: Some(8192),
            reasoning_effort: Some("high".to_string()),
            ..Default::default()
        };
        let patch = apply_anthropic_thinking(&id, &opts);

        assert_eq!(
            patch.output_config,
            Some(json!({"effort": "high"})),
            "思考深度必须走 output_config.effort，否则 reasoning_effort 是死配置"
        );
        // 端点不认 `budget_tokens`，也不能出现 `enabled`。
        assert_eq!(
            patch.thinking, None,
            "budget 在这条线上没有语义，不能拿它当『想开思考』的信号去翻开关"
        );
        assert!(patch.budget.is_none(), "MiniMax 侧不存在 budget 预算");
        assert!(patch.ignored.contains(&"thinking_budget"));
    }

    #[test]
    fn minimax_budget_alone_never_flips_the_switch() {
        // 「记为被忽略的参数」不得反过来改变行为：M3 默认关思考，
        // 只配 thinking_budget 时若被翻成 adaptive 就是用户没要求的行为翻转。
        for model in ["MiniMax-M3", "MiniMax-M3.1-Flash-Preview", "MiniMax-M2.7"] {
            let patch = apply_anthropic_thinking(
                &minimax_anthropic(model),
                &RequestOptions {
                    thinking_budget: Some(8192),
                    ..Default::default()
                },
            );
            assert_eq!(patch.thinking, None, "{model}: budget 不该翻开关");
            assert!(patch.ignored.contains(&"thinking_budget"), "{model}");
        }
    }

    #[test]
    fn minimax_explicit_switch_on_maps_to_adaptive() {
        // 显式要开 → adaptive（`enabled` 是 Anthropic 原生取值，端点不认）。
        for model in ["MiniMax-M3", "MiniMax-M3.1-Flash-Preview", "MiniMax-M2.7"] {
            let patch = apply_anthropic_thinking(
                &minimax_anthropic(model),
                &RequestOptions {
                    thinking_switch: Some("Enabled".to_string()),
                    ..Default::default()
                },
            );
            assert_eq!(
                patch.thinking,
                Some(json!({"type": "adaptive"})),
                "{model}: 显式开关必须生效，且大小写/空白归一"
            );
            assert!(patch.ignored.is_empty(), "{model}: {:?}", patch.ignored);
        }
    }

    #[test]
    fn minimax_m31_refuses_disabled_instead_of_sending_a_400() {
        let id = minimax_anthropic("MiniMax-M3.1-Flash-Preview");
        let opts = RequestOptions {
            max_tokens: 32000,
            thinking_switch: Some("disabled".to_string()),
            ..Default::default()
        };
        let patch = apply_anthropic_thinking(&id, &opts);

        assert_eq!(
            patch.thinking, None,
            "M3.1 收 disabled 会 400；省略字段（默认开）才是等价且安全的表达"
        );
        assert!(
            patch.ignored.contains(&"thinking_switch=disabled"),
            "静默忽略用户配置会让人以为关掉了思考：{:?}",
            patch.ignored
        );
    }

    #[test]
    fn minimax_m3_honours_disabled_and_defaults_to_off() {
        let id = minimax_anthropic("MiniMax-M3");

        // M3 默认关闭 thinking：什么都没配就不该发任何字段。
        assert_eq!(
            apply_anthropic_thinking(&id, &RequestOptions::default()).thinking,
            None
        );

        // 显式关掉是真能关的（与 M3.1 相反）。
        let off = apply_anthropic_thinking(
            &id,
            &RequestOptions {
                thinking_switch: Some("disabled".to_string()),
                ..Default::default()
            },
        );
        assert_eq!(off.thinking, Some(json!({"type": "disabled"})));
        assert!(off.ignored.is_empty());
    }

    #[test]
    fn minimax_m2_forces_thinking_on_and_drops_effort() {
        let id = minimax_anthropic("MiniMax-M2.7");
        let patch = apply_anthropic_thinking(
            &id,
            &RequestOptions {
                reasoning_effort: Some("high".to_string()),
                thinking_switch: Some("disabled".to_string()),
                ..Default::default()
            },
        );
        assert_eq!(
            patch.thinking, None,
            "M2.x 省略即开启，disabled 发了也不生效"
        );
        assert!(patch.output_config.is_none());
        assert!(patch.ignored.contains(&"reasoning_effort"));
    }

    #[test]
    fn minimax_m31_drops_unsupported_effort_levels() {
        let id = minimax_anthropic("MiniMax-M3.1-Flash-Preview");
        let patch = apply_anthropic_thinking(
            &id,
            &RequestOptions {
                reasoning_effort: Some("minimal".to_string()),
                ..Default::default()
            },
        );
        assert_eq!(
            patch.output_config, None,
            "端点只认 low/medium/high/xhigh/max，minimal 发出去会 400"
        );
        assert!(patch.ignored.contains(&"reasoning_effort"));
    }

    #[test]
    fn official_anthropic_keeps_the_native_budget_shape() {
        let id = identity(
            "anthropic",
            WireProtocol::AnthropicMessages,
            "claude-opus-4-8",
        );
        let patch = apply_anthropic_thinking(
            &id,
            &RequestOptions {
                thinking_budget: Some(4096),
                ..Default::default()
            },
        );
        assert_eq!(
            patch.thinking,
            Some(json!({"type": "enabled", "budget_tokens": 4096})),
            "官方端点的原生形态不能被 MiniMax 适配带偏"
        );
        assert_eq!(
            patch.budget,
            Some(4096),
            "max_tokens 兜底靠这个结构化字段，不许再从 Value 里反解键名"
        );
        assert!(patch.output_config.is_none());
    }

    // ── 官方 Anthropic 的 effort 支持 ──
    //
    // 回归背景（2026-10）：`reasoning_effort` 此前在 Anthropic 线上是**死配置**
    // ——`anthropic_native()` 的 `effort_levels` 是空切片，adapter 也从不读
    // `opts.reasoning_effort`，于是用户配的 effort 被 catalog 记成 dropped 后
    // 彻底消失。同一份 profile 换到 OpenAI 线则 effort 生效，两条线的思考量
    // 因此完全对不上。官方协议现已原生支持 `output_config.effort`，这里把它接上。

    /// 官方 Anthropic：`reasoning_effort` 必须真的变成 `output_config.effort`。
    #[test]
    fn official_anthropic_emits_output_config_effort() {
        let id = identity(
            "anthropic",
            WireProtocol::AnthropicMessages,
            "claude-opus-4-8",
        );
        let patch = apply_anthropic_thinking(
            &id,
            &RequestOptions {
                reasoning_effort: Some("high".to_string()),
                ..Default::default()
            },
        );
        assert_eq!(
            patch.output_config,
            Some(json!({"effort": "high"})),
            "Anthropic 线的 reasoning_effort 必须是活的配置，不能是死配置"
        );
        assert!(patch.ignored.is_empty(), "{:?}", patch.ignored);
    }

    /// budget 与 effort 是**两个独立旋钮**（官方文档明确说可以一起用）：
    /// budget 限思考的 token 数，effort 限整个响应的投入档位。
    #[test]
    fn official_anthropic_sends_budget_and_effort_together() {
        let id = identity(
            "anthropic",
            WireProtocol::AnthropicMessages,
            "claude-opus-4-8",
        );
        let patch = apply_anthropic_thinking(
            &id,
            &RequestOptions {
                thinking_budget: Some(4096),
                reasoning_effort: Some("medium".to_string()),
                ..Default::default()
            },
        );
        assert_eq!(
            patch.thinking,
            Some(json!({"type": "enabled", "budget_tokens": 4096})),
            "budget 仍是原生形态"
        );
        assert_eq!(
            patch.output_config,
            Some(json!({"effort": "medium"})),
            "effort 不能因为 budget 存在就被吃掉"
        );
        assert!(patch.ignored.is_empty(), "{:?}", patch.ignored);
    }

    /// 档位越界必须变成**可见的 dropped**，而不是发出去换 400。
    #[test]
    fn official_anthropic_drops_unsupported_effort_level() {
        // Opus 4.5 只到 high：xhigh / max 会被端点判非法。
        let id = identity(
            "anthropic",
            WireProtocol::AnthropicMessages,
            "claude-opus-4-5",
        );
        for level in ["xhigh", "max"] {
            let patch = apply_anthropic_thinking(
                &id,
                &RequestOptions {
                    reasoning_effort: Some(level.to_string()),
                    ..Default::default()
                },
            );
            assert_eq!(patch.output_config, None, "{level} 不该上送");
            assert!(patch.ignored.contains(&"reasoning_effort"), "{level}");
        }
    }

    /// 各型号的档位集合必须精确——发一个端点不认的档位就是 400。
    #[test]
    fn anthropic_effort_levels_are_per_model() {
        let levels = |model: &str| {
            ThinkingSpec::for_vendor("anthropic", &WireProtocol::AnthropicMessages, model)
                .effort_levels
                .to_vec()
        };
        // 五档全量。
        assert_eq!(levels("claude-opus-4-8").len(), 5);
        assert_eq!(levels("claude-opus-4-7").len(), 5);
        assert_eq!(levels("claude-opus-5").len(), 5);
        assert_eq!(levels("claude-sonnet-5-5").len(), 5);
        assert_eq!(levels("claude-haiku-5-5").len(), 5);
        // 到 max（无 xhigh）。
        assert_eq!(levels("claude-opus-4-6").len(), 4);
        assert_eq!(levels("claude-sonnet-4-6").len(), 4);
        // 到 high（无 xhigh/max）。
        assert_eq!(levels("claude-opus-4-5").len(), 3);
        assert_eq!(levels("claude-sonnet-4-5").len(), 3);
        // 老型号不支持 effort。
        assert!(levels("claude-3-5-sonnet-20241022").is_empty());
        assert!(levels("claude-3-7-sonnet-20250219").is_empty());
        // 非 Claude 型号（第三方 Anthropic 兼容端点走别的分支）。
        assert!(levels("glm-4.6").is_empty());
    }

    /// 官方 Anthropic 的 effort 必须真的落到 `RequestOptions` 里——
    /// `request_plan` 层（catalog 依据 `effort_levels` 裁决）不能把它当死配置丢掉。
    #[test]
    fn official_anthropic_effort_survives_the_catalog_layer() {
        let profile = wyj_config::Profile {
            name: "anthropic".to_string(),
            provider: wyj_config::Provider::Anthropic,
            base_url: "https://api.anthropic.com".to_string(),
            model: "claude-opus-4-8".to_string(),
            vendor: Some("anthropic".to_string()),
            reasoning_effort: Some("high".to_string()),
            ..Default::default()
        };
        let plan = crate::request_plan::RequestPlan::from_profile(&profile, None);
        assert!(
            !plan
                .dropped_parameters
                .iter()
                .any(|d| d.name == "reasoning_effort"),
            "effort 被 catalog 层丢掉了：{:?}",
            plan.dropped_parameters
        );

        let opts = RequestOptions::from_request_plan(&plan);
        let id = identity("anthropic", WireProtocol::AnthropicMessages, &profile.model);
        let patch = apply_anthropic_thinking(&id, &opts);
        assert_eq!(patch.output_config, Some(json!({"effort": "high"})));
    }

    #[test]
    fn anthropic_native_reports_switch_that_budget_overrode() {
        // budget 是这条线唯一的开关旋钮：用户要关、却因为有 budget 仍在发
        // `enabled`（thinking 照开）——必须可见，否则用户以为关掉了。
        let id = identity(
            "anthropic",
            WireProtocol::AnthropicMessages,
            "claude-opus-4-8",
        );
        let off = apply_anthropic_thinking(
            &id,
            &RequestOptions {
                thinking_switch: Some("disabled".to_string()),
                thinking_budget: Some(4096),
                ..Default::default()
            },
        );
        assert_eq!(off.thinking.unwrap()["type"], "enabled");
        assert!(
            off.ignored.contains(&"thinking_switch"),
            "{:?}",
            off.ignored
        );

        // 反向：要开却没给 budget → 压根没发 thinking 字段，thinking 其实没开。
        let on = apply_anthropic_thinking(
            &id,
            &RequestOptions {
                thinking_switch: Some("enabled".to_string()),
                ..Default::default()
            },
        );
        assert!(on.thinking.is_none());
        assert!(on.ignored.contains(&"thinking_switch"), "{:?}", on.ignored);
    }

    #[test]
    fn text_only_switch_is_honoured_on_official_anthropic_without_warning() {
        // compact / memory / summary / 分类器全走 text_only()：它的
        // `disabled` + 无 budget 正好是「被兑现」的组合，绝不能记成 ignored，
        // 否则每次辅助调用都刷一条 warn。
        let id = identity(
            "anthropic",
            WireProtocol::AnthropicMessages,
            "claude-opus-4-8",
        );
        let patch = apply_anthropic_thinking(&id, &RequestOptions::text_only(256));
        assert!(patch.thinking.is_none());
        assert!(
            patch.ignored.is_empty(),
            "text_only 不该产生告警：{:?}",
            patch.ignored
        );
    }

    /// **端到端**：profile → catalog → RequestOptions → 请求体补丁。
    ///
    /// 前面所有 adapter 单测都手工构造 `RequestOptions`，恰好绕开了真正会杀人的
    /// 那一层：`request_plan` 依据 `ThinkingSpec::effort_levels` 决定 reasoning_effort
    /// 是否放行。spec 一旦漏了 MiniMax + AnthropicMessages 分臂，adapter 写得再对
    /// 也是死代码（曾经就这么坏过：profile 里配的 effort 被 catalog 静默丢弃，
    /// 而单测全绿）。因此这条断言必须从 profile 出发。
    #[test]
    fn minimax_effort_survives_the_catalog_layer() {
        let profile = minimax_profile_with_effort("high");
        let plan = crate::request_plan::RequestPlan::from_profile(&profile, None);
        assert!(
            !plan
                .dropped_parameters
                .iter()
                .any(|d| d.name == "reasoning_effort"),
            "effort 被 catalog 层丢掉了：{:?}",
            plan.dropped_parameters
        );

        let opts = RequestOptions::from_request_plan(&plan);
        let identity = identity("minimax", WireProtocol::AnthropicMessages, &profile.model);
        let patch = apply_anthropic_thinking(&identity, &opts);
        assert_eq!(
            patch.output_config,
            Some(json!({"effort": "high"})),
            "catalog 放行了 effort，adapter 却没上送——两层判据又漂移了"
        );
    }

    #[test]
    fn minimax_anthropic_spec_refuses_budget_but_keeps_effort() {
        // 旧 config 里常见的「两个都配」：budget 不支持不该把 effort 连坐丢掉。
        let mut profile = minimax_profile_with_effort("high");
        profile.thinking_budget = Some(8192);
        let plan = crate::request_plan::RequestPlan::from_profile(&profile, None);
        let names: Vec<_> = plan
            .dropped_parameters
            .iter()
            .map(|d| d.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec!["thinking_budget"],
            "只应丢 budget，effort 必须留：{names:?}"
        );
        assert_eq!(
            plan.reasoning,
            crate::request_plan::ReasoningRequest::Effort("high".to_string()),
            "budget 不可用时 effort 仍应生效，不能整体退化成 Disabled"
        );
    }

    fn minimax_profile_with_effort(effort: &str) -> wyj_config::Profile {
        wyj_config::Profile {
            name: "minimax".to_string(),
            provider: wyj_config::Provider::Anthropic,
            base_url: "https://api.minimaxi.com/anthropic".to_string(),
            model: "MiniMax-M3.1-Flash-Preview".to_string(),
            vendor: Some("minimax".to_string()),
            reasoning_effort: Some(effort.to_string()),
            ..Default::default()
        }
    }
}
