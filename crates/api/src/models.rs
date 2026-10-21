//! 「vendor 推断」与「拉取模型列表」两件事的归属地。

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use wyj_config::{Profile, Provider};

/// **全仓唯一的 vendor 推导入口**。
///
/// 背景：这份实现此前存在**三份互不一致的副本**——`model_catalog.rs`、
/// `openai.rs`、`anthropic.rs` 各写一份。三者 needle 表不同、兜底不同：
/// `openai.rs` 缺 `api.openai.com` 判定（官方 OpenAI 端点会被推成
/// `"custom"`），`anthropic.rs` 缺 deepseek/qwen/volcengine/ollama/vllm
/// 且兜底返回 `"anthropic"`。
///
/// 那时无害，因为 `Profile.vendor` 显式写死时三者拿到的是同一个值。
/// **一旦 `Profile.vendor` 字段被移除，三者就会开始分歧**——最直接的后果是
/// 官方 OpenAI 端点在 `openai.rs` 里被推成 `"custom"`，于是
/// `uses_max_completion_tokens` 误判为「非官方 OpenAI」，把 `max_tokens`
/// 而不是 `max_completion_tokens` 发出去，官方 reasoning 模型直接 400。
///
/// 所以这里收敛成一份权威实现，needle 表取三方**并集**（宁可多认一个
/// vendor，也不要让同一个端点在两处推成不同结果），兜底统一为 `"custom"`
/// ——「认不出来」与「认成 anthropic 官方」是两回事，前者不会误开官方端点
/// 专属的行为（interleaved beta / max_completion_tokens / 五档 effort）。
///
/// 特判必须排在 needle 表之前：ollama / vllm 的判定靠端口号与路径，
/// 而它们的模型名可能含 `qwen`（`qwen3-coder`）——若先跑 needle 表，
/// 本地 ollama 会被认成阿里云。
pub fn infer_vendor(profile: &Profile, model: &str) -> String {
    let base_url = profile.base_url.to_ascii_lowercase();
    let model = model.to_ascii_lowercase();

    // ── ① 本地端点：靠端口 / 路径判定，优先于任何模型名 needle ──
    //
    // 两个判定都必须在 needle 表之前：本地推理服务跑的模型名往往含
    // `qwen`（`qwen3-coder`）——若先跑 needle 表，本地 ollama 会被认成阿里云。
    //
    // vLLM 额外认端口 8000：它的默认监听端口（`--port 8000`），而
    // `http://127.0.0.1:8000/v1` 这个地址**不含 "vllm" 字样**——只靠路径
    // 匹配的话，它会掉进 needle 表被模型名带偏（`qwen3` → 阿里云）。
    // 这正是旧模板靠显式 `vendor = "vllm"` 兜住、移除该字段后暴露出来的洞。
    if base_url.contains("127.0.0.1:11434") || base_url.contains("localhost:11434") {
        return "ollama".to_string();
    }
    if base_url.contains("vllm")
        || base_url.contains("127.0.0.1:8000")
        || base_url.contains("localhost:8000")
    {
        return "vllm".to_string();
    }

    // ── ② base_url 留空 = 走 provider 的官方默认端点 ──
    //
    // 无歧义，且必须在下面的模型名规则之前：OpenAI 的微调模型 ID 带冒号
    //（`ft:gpt-4o:acme:123`），若先看模型名会把它误判成「带命名空间的
    // 聚合路由」。
    if base_url.trim().is_empty() {
        return match profile.provider {
            Provider::Anthropic => "anthropic".to_string(),
            Provider::OpenAI => "openai".to_string(),
        };
    }

    // ── ③ 端点**域名**命中厂商官方域名 → 该厂商 ──
    //
    // 这一层是**无歧义**的：域名属于谁，就按谁的 thinking 规格发字段。
    // 它排在模型名规则之前，正是为了让「厂商官方端点 + 带前缀的模型名」
    // 仍然走厂商臂（后者是聚合路由的形态，不是厂商自己的）。
    let host = host_of(&base_url);
    for (domain, vendor) in VENDOR_DOMAINS {
        if host_matches(&host, domain) {
            return vendor.to_string();
        }
    }

    // ── ④ 聚合路由 / 模型中转 → `custom` ──
    //
    // OpenRouter / larprouter / openscale 这类中转端点把多家厂商的模型聚在
    // 一起转发，模型 ID 带厂商前缀（`minimax/minimax-m3.1-flash:free`、
    // `deepseek/deepseek-v4.1-flash-1017`），但**不认**厂商专属的 thinking
    // 字段——发 `thinking.type` / `reasoning_split` / `output_config.effort`
    // 过去多半 400。
    //
    // 两条判据，缺一不可：
    // a) 域名特征词（`router` / `oneapi` / `newapi` / …）；
    // b) **模型名带命名空间分隔符**（`/` 或 `:`）——聚合路由的模型 ID 几乎
    //    都带厂商前缀，而厂商官方模型名不带（`claude-opus-4-8`、
    //    `deepseek-chat`）。这条覆盖了词表穷举不到的域名（用户的
    //    `api.openscale.so` 就是这类）。
    //
    // 代价是**信息损失**：一个中转端点若用的是**无前缀**的模型 ID
    //（`deepseek-v4.1-flash`），它与厂商官方端点就再也区分不出来了 ——
    // 旧配置里靠显式 `vendor = "custom"` 兜住的那个信息，随字段移除而消失。
    // 只能靠域名特征词兜（`larprouter.com` 命中 `router`）。
    if is_aggregator_endpoint(&host) || has_vendor_namespace(&model) {
        return "custom".to_string();
    }

    // ── ⑤ 模型名 needle → 该厂商 ──
    //
    // 兜底到这里说明端点不是任何厂商的官方域名（自建代理 / 公司网关），
    // 那就按模型名猜——与旧实现同源。
    let haystack = format!("{base_url} {model}");
    for (needle, vendor) in [
        ("minimax", "minimax"),
        ("bigmodel", "zhipu"),
        ("z.ai", "zhipu"),
        ("glm", "zhipu"),
        ("moonshot", "moonshot"),
        ("kimi", "moonshot"),
        ("deepseek", "deepseek"),
        ("dashscope", "alibaba"),
        ("aliyun", "alibaba"),
        ("qwen", "alibaba"),
        ("volces", "volcengine"),
        ("doubao", "volcengine"),
        ("ollama", "ollama"),
    ] {
        if haystack.contains(needle) {
            return vendor.to_string();
        }
    }

    "custom".to_string()
}

/// 厂商**官方域名**表。命中即按该厂商处理，无歧义。
///
/// 用**域名精确匹配**（`host == domain` 或 `host` 以 `.domain` 结尾）而不是
/// 子串包含——`z.ai` 若是子串匹配，`siz.ai` 之类无关域名会被误伤。
const VENDOR_DOMAINS: &[(&str, &str)] = &[
    ("api.anthropic.com", "anthropic"),
    ("api.openai.com", "openai"),
    ("api.minimaxi.com", "minimax"),
    ("api.minimax.chat", "minimax"),
    ("api.deepseek.com", "deepseek"),
    ("api.moonshot.cn", "moonshot"),
    ("open.bigmodel.cn", "zhipu"),
    ("bigmodel.cn", "zhipu"),
    ("z.ai", "zhipu"),
    ("dashscope.aliyuncs.com", "alibaba"),
    ("aliyuncs.com", "alibaba"),
    ("volces.com", "volcengine"),
    ("volcengine.com", "volcengine"),
];

/// 从 base_url 里取出主机名（小写，不含端口）。
fn host_of(base_url: &str) -> String {
    let without_scheme = base_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(base_url);
    // 去掉路径与可能的凭据段。
    let host = without_scheme
        .split('/')
        .next()
        .unwrap_or(without_scheme)
        .rsplit_once('@')
        .map(|(_, host)| host)
        .unwrap_or(without_scheme.split('/').next().unwrap_or(without_scheme));
    // 去掉端口。
    host.split(':').next().unwrap_or(host).to_string()
}

/// 主机名是否等于该域名，或以 `.` + 该域名结尾（子域）。
fn host_matches(host: &str, domain: &str) -> bool {
    host == domain || host.ends_with(&format!(".{domain}"))
}

/// 该端点是否是**聚合路由 / 模型中转**（OpenRouter、oneapi、new-api 等）。
///
/// 判据是域名里的特征词。刻意保守：宁可把某个真厂商端点误判成 `custom`
/// （thinking 全关，最多是少了思考档位），也不能把聚合路由误判成某厂商
/// （发错字段直接 400）。
fn is_aggregator_endpoint(host: &str) -> bool {
    const MARKERS: &[&str] = &[
        "openrouter",
        "router",
        "oneapi",
        "newapi",
        "new-api",
        "openai-hub",
        "ai-gateway",
        "aigateway",
        "api-hub",
        "apihub",
        "anyrouter",
        "siliconflow",
        "closeai",
        "openscale",
    ];
    MARKERS.iter().any(|marker| host.contains(marker))
}

/// 模型名是否带**命名空间分隔符**（`/` 或 `:`）。
///
/// 聚合路由的模型 ID 几乎都带厂商前缀（`minimax/minimax-m3.1-flash:free`、
/// `deepseek/deepseek-v4.1-flash-1017`），而厂商官方模型名不带
/// （`claude-opus-4-8`、`deepseek-chat`）。这条是聚合路由判定里唯一能覆盖
/// 未知域名的信号。
fn has_vendor_namespace(model: &str) -> bool {
    model.contains('/') || model.contains(':')
}

/// 已知模型的容量组合（上下文窗口 / 单次输出上限）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelCapacity {
    pub context_window: u32,
    pub max_output_tokens: u32,
}

impl ModelCapacity {
    const fn new(context_window: u32, max_output_tokens: u32) -> Self {
        Self {
            context_window,
            max_output_tokens,
        }
    }
}

/// 按模型族名给出容量。**这是静态猜测表，不是探测结果。**
///
/// 为什么必须保守：`context_window` 直接决定压缩触发线（window -
/// max(max_tokens+8192, window/10)），填大了保护等于没跑、填小了每几轮就压缩。
/// 所以这里只收录**能确定下限**的常见族，且给出的值取保守侧的公开值；命中
/// 不到的模型返回 `None`，让面板保持用户已填的数字（默认 200k/32k）而不是
/// 猜一个可能错的数。
///
/// 注意：供应商的 `/models` 列表接口不返回任何容量元数据（OpenAI 只给 id/owned_by，
/// Anthropic 只给 id/display_name），所以「拉列表 → 自动填容量」只能靠这张表。
pub fn known_model_capacity(model: &str) -> Option<ModelCapacity> {
    let m = model.trim().to_ascii_lowercase();
    if m.is_empty() {
        return None;
    }
    // 顺序有意义：先匹配更具体的前缀，避免 "gpt-4" 吃掉 "gpt-4o"。
    const TABLE: &[(&str, ModelCapacity)] = &[
        // ── Anthropic ──
        ("claude-opus-4", ModelCapacity::new(200_000, 32_000)),
        ("claude-sonnet-4", ModelCapacity::new(200_000, 32_000)),
        ("claude-3-7", ModelCapacity::new(200_000, 32_000)),
        ("claude-3-5", ModelCapacity::new(200_000, 8_192)),
        ("claude-3", ModelCapacity::new(200_000, 4_096)),
        // ── OpenAI ──
        // gpt-5 系是 reasoning 模型：输出预算**同时覆盖 reasoning tokens**，
        // 因此这里给的是端点允许的最大完成量（Chat Completions 的
        // `max_completion_tokens` 语义），不是"可见文本上限"。
        ("gpt-5.1", ModelCapacity::new(400_000, 128_000)),
        ("gpt-5", ModelCapacity::new(400_000, 128_000)),
        ("gpt-4.1", ModelCapacity::new(1_047_576, 32_768)),
        ("gpt-4o", ModelCapacity::new(128_000, 16_384)),
        ("gpt-4-turbo", ModelCapacity::new(128_000, 4_096)),
        ("gpt-3.5", ModelCapacity::new(16_385, 4_096)),
        ("o4-mini", ModelCapacity::new(200_000, 100_000)),
        ("o3", ModelCapacity::new(200_000, 100_000)),
        ("o1", ModelCapacity::new(200_000, 100_000)),
        // ── 国内厂商 ──
        ("deepseek-chat", ModelCapacity::new(64_000, 8_192)),
        ("deepseek-reasoner", ModelCapacity::new(64_000, 8_192)),
        ("deepseek", ModelCapacity::new(64_000, 8_192)),
        ("qwen3", ModelCapacity::new(131_072, 8_192)),
        ("qwen2.5", ModelCapacity::new(131_072, 8_192)),
        ("qwen", ModelCapacity::new(131_072, 8_192)),
        ("glm-4.6", ModelCapacity::new(200_000, 16_384)),
        ("glm-4.5", ModelCapacity::new(131_072, 16_384)),
        ("glm-4", ModelCapacity::new(131_072, 4_096)),
        ("kimi-k2", ModelCapacity::new(131_072, 8_192)),
        ("kimi", ModelCapacity::new(131_072, 8_192)),
        ("minimax-m2", ModelCapacity::new(204_800, 32_000)),
        ("minimax-m3", ModelCapacity::new(204_800, 32_000)),
        ("minimax", ModelCapacity::new(204_800, 32_000)),
        ("doubao", ModelCapacity::new(131_072, 8_192)),
        // ── Google ──
        ("gemini-2.5", ModelCapacity::new(1_048_576, 65_536)),
        ("gemini-2", ModelCapacity::new(1_048_576, 8_192)),
        ("gemini", ModelCapacity::new(1_048_576, 8_192)),
    ];
    TABLE
        .iter()
        .find(|(prefix, _)| m.starts_with(prefix) || m.contains(prefix))
        .map(|(_, cap)| *cap)
}

#[derive(Deserialize)]
struct OpenAiModelsResponse {
    data: Vec<OpenAiModelEntry>,
}

#[derive(Deserialize)]
struct OpenAiModelEntry {
    id: String,
}

#[derive(Deserialize)]
struct AnthropicModelsResponse {
    data: Vec<AnthropicModelEntry>,
}

#[derive(Deserialize)]
struct AnthropicModelEntry {
    id: String,
}

/// 尽力而为地拉取供应商可用模型 ID 列表。
///
/// 第三方代理不一定实现模型列表接口，失败时返回 Err，调用方应展示内联错误
/// 并允许用户继续手填模型名，而不是阻塞其他操作。
pub async fn fetch_model_ids(
    provider: &Provider,
    base_url: &str,
    api_key: &str,
) -> Result<Vec<String>> {
    let base_url = base_url.trim_end_matches('/');
    if base_url.is_empty() {
        return Err(anyhow!("base_url 为空，无法拉取模型列表"));
    }
    let client = crate::stream_guard::build_client()?;
    let ids: Vec<String> = match provider {
        Provider::OpenAI => {
            let url = format!("{base_url}/models");
            let resp = client
                .get(&url)
                .header("Authorization", format!("Bearer {api_key}"))
                .send()
                .await
                .with_context(|| format!("请求 {url} 失败"))?
                .error_for_status()
                .with_context(|| format!("{url} 返回错误状态"))?;
            let parsed: OpenAiModelsResponse = resp.json().await.context("解析模型列表响应失败")?;
            parsed.data.into_iter().map(|e| e.id).collect()
        }
        Provider::Anthropic => {
            // base_url 常被用户按 OpenAI 习惯填成 `.../v1`（本文件的 vllm 模板就是），
            // 再拼 `/v1/models` 会得到 `/v1/v1/models`。只补一次版本段。
            let url = if base_url.ends_with("/v1") {
                format!("{base_url}/models")
            } else {
                format!("{base_url}/v1/models")
            };
            let resp = client
                .get(&url)
                .header("x-api-key", api_key)
                .header("anthropic-version", "2023-06-01")
                .send()
                .await
                .with_context(|| format!("请求 {url} 失败"))?
                .error_for_status()
                .with_context(|| format!("{url} 返回错误状态"))?;
            let parsed: AnthropicModelsResponse =
                resp.json().await.context("解析模型列表响应失败")?;
            parsed.data.into_iter().map(|e| e.id).collect()
        }
    };
    if ids.is_empty() {
        return Err(anyhow!("模型列表为空"));
    }
    Ok(ids)
}

#[cfg(test)]
mod capacity_tests {
    use super::*;

    #[test]
    fn known_families_resolve_to_sane_capacities() {
        let claude = known_model_capacity("claude-sonnet-4-5").expect("claude 应命中");
        assert_eq!(claude.context_window, 200_000);
        assert_eq!(claude.max_output_tokens, 32_000);

        let gpt4o = known_model_capacity("gpt-4o-mini").expect("gpt-4o 应命中");
        assert_eq!(gpt4o.context_window, 128_000);

        let ds = known_model_capacity("deepseek-reasoner").expect("deepseek 应命中");
        assert_eq!(ds.context_window, 64_000);
    }

    /// 更具体的前缀必须先命中：`gpt-4o` 不能被 `gpt-4-turbo` 那条吃掉，
    /// `deepseek-reasoner` 也不该和 `deepseek-chat` 互相串。
    #[test]
    fn specific_prefixes_win_over_generic_ones() {
        let a = known_model_capacity("gpt-4o").unwrap();
        let b = known_model_capacity("gpt-4-turbo").unwrap();
        assert_ne!(a, b, "gpt-4o 与 gpt-4-turbo 必须区分开: {a:?} vs {b:?}");
        assert_eq!(a.max_output_tokens, 16_384);
        assert_eq!(b.max_output_tokens, 4_096);
    }

    /// 未知模型必须返回 None 而不是猜一个数——`context_window` 决定压缩触发线，
    /// 猜大了保护等于没跑，猜小了每几轮就压缩。
    #[test]
    fn unknown_models_are_left_alone() {
        assert!(known_model_capacity("totally-unknown-model").is_none());
        assert!(known_model_capacity("ep-20250101-abcdef").is_none());
        assert!(known_model_capacity("   ").is_none());
    }

    /// 大小写与空白不该影响命中。
    #[test]
    fn matching_is_case_and_whitespace_insensitive() {
        assert_eq!(
            known_model_capacity("  Claude-Opus-4  "),
            known_model_capacity("claude-opus-4")
        );
    }
}

/// `infer_vendor` 的回归钉子。
///
/// 这份实现此前有**三份互不一致的副本**（`model_catalog.rs` /
/// `openai.rs` / `anthropic.rs`），needle 表与兜底各不相同。`Profile.vendor`
/// 显式写死时三者结果相同，字段一移除就立刻分歧——最直接的后果是官方
/// OpenAI 端点在 `openai.rs` 里被推成 `"custom"`，于是
/// `uses_max_completion_tokens` 误判，把 `max_tokens` 而不是
/// `max_completion_tokens` 发出去，官方 reasoning 模型直接 400。
///
/// 下面两组断言各钉一个方向：
/// ① 常见端点必须推出正确的 vendor；
/// ② **兜底必须是 `"custom"`**——认不出来与「认成 anthropic 官方」是两回事，
///    后者会误开官方端点专属的行为（interleaved beta / 五档 effort）。
#[cfg(test)]
mod infer_vendor_tests {
    use super::*;

    fn profile(provider: Provider, base_url: &str) -> Profile {
        Profile {
            provider,
            base_url: base_url.to_string(),
            ..Profile::default()
        }
    }

    #[test]
    fn known_endpoints_resolve_to_their_vendor() {
        for (provider, base_url, model, expected) in [
            (
                Provider::Anthropic,
                "https://api.anthropic.com",
                "claude-opus-4-8",
                "anthropic",
            ),
            (
                Provider::OpenAI,
                "https://api.openai.com/v1",
                "gpt-5",
                "openai",
            ),
            (
                Provider::Anthropic,
                "https://open.bigmodel.cn/api/anthropic",
                "glm-5.2",
                "zhipu",
            ),
            (
                Provider::OpenAI,
                "https://ark.cn-beijing.volces.com/api/v3",
                "doubao-seed-1-6",
                "volcengine",
            ),
            (
                Provider::OpenAI,
                "https://api.minimaxi.com/v1",
                "MiniMax-M2",
                "minimax",
            ),
            (
                Provider::Anthropic,
                "https://api.moonshot.cn/anthropic",
                "kimi-k2-turbo-preview",
                "moonshot",
            ),
            (
                Provider::OpenAI,
                "https://api.deepseek.com",
                "deepseek-chat",
                "deepseek",
            ),
            (
                Provider::OpenAI,
                "https://dashscope.aliyuncs.com/compatible-mode/v1",
                "qwen3-coder-plus",
                "alibaba",
            ),
            (
                Provider::OpenAI,
                "http://127.0.0.1:11434/v1",
                "qwen3-coder",
                "ollama",
            ),
            (
                Provider::OpenAI,
                "http://127.0.0.1:8000/v1",
                "qwen3",
                "vllm",
            ),
        ] {
            let p = profile(provider, base_url);
            assert_eq!(
                infer_vendor(&p, model),
                expected,
                "{base_url} + {model} 应推出 {expected}"
            );
        }
    }

    /// 认不出来的端点一律 `custom`——**绝不能**兜底成 `"anthropic"`。
    ///
    /// `"anthropic"` 兜底会让第三方兼容端点拿到官方端点的能力表
    /// （五档 effort / interleaved beta），而它们只认开关。
    #[test]
    fn unknown_endpoints_fall_back_to_custom_not_anthropic() {
        let p = profile(Provider::Anthropic, "https://proxy.invalid/anthropic");
        assert_eq!(infer_vendor(&p, "some-model"), "custom");
    }

    /// 本地端点必须**先于**模型名 needle 判定：ollama 跑的模型可能叫
    /// `qwen3-coder`（含 `qwen` → 阿里云），但它不是阿里云。
    #[test]
    fn local_endpoints_win_over_model_name_needles() {
        let p = profile(Provider::OpenAI, "http://127.0.0.1:11434/v1");
        assert_eq!(infer_vendor(&p, "qwen3-coder"), "ollama");
    }

    /// **聚合路由必须判 `"custom"`，不能按模型名的厂商前缀认厂商。**
    ///
    /// OpenRouter 这类中转端点的模型 ID 带厂商前缀
    /// （`minimax/minimax-m3.1-flash:free`），但它们**不认**厂商专属的
    /// thinking 字段（`thinking.type` / `reasoning_split` /
    /// `output_config.effort`）——按 minimax 处理就会发错字段换 400。
    ///
    /// 这条在移除 `Profile.vendor` 配置项之前是被用户的显式
    /// `vendor = "custom"` 兜住的；字段一移除就必须由推导自己认出来。
    ///
    /// 下面这组样本取自**用户真实的 config.toml**——包括两个词表穷举不到的
    /// 域名（`api.openscale.so` / `192.168.5.5:7863`），它们靠「模型名带
    /// 命名空间分隔符」这条兜住。
    #[test]
    fn aggregator_routers_are_custom_even_when_the_model_id_names_a_vendor() {
        for (base_url, model) in [
            (
                "https://openrouter.ai/api/v1",
                "minimax/minimax-m3.1-flash:free",
            ),
            ("https://larprouter.com/v1", "deepseek-v4.1-flash"),
            (
                "https://api.openscale.so/v1",
                "deepseek/deepseek-v4.1-flash-1017",
            ),
            ("http://192.168.5.5:7863/v1", "global:deepseek-v4.1-flash"),
        ] {
            let p = profile(Provider::OpenAI, base_url);
            assert_eq!(
                infer_vendor(&p, model),
                "custom",
                "{base_url} 是聚合路由，不该按模型名前缀认厂商"
            );
        }
    }

    /// 真厂商端点不受聚合路由判定影响。
    #[test]
    fn real_vendor_endpoints_are_not_mistaken_for_aggregators() {
        let p = profile(Provider::OpenAI, "https://api.minimaxi.com/v1");
        assert_eq!(infer_vendor(&p, "MiniMax-M2"), "minimax");
    }

    /// **厂商官方端点 + 带前缀的模型名**仍走厂商臂——域名是无歧义的。
    ///
    /// 这条与上一条互为对照：同样是带命名空间的模型名，端点是谁的域名
    /// 决定按谁的 thinking 规格发字段。
    #[test]
    fn an_official_vendor_domain_wins_over_the_model_id_shape() {
        let p = profile(Provider::OpenAI, "https://api.minimaxi.com/v1");
        assert_eq!(infer_vendor(&p, "minimax/minimax-m3.1-flash"), "minimax");
    }

    /// `base_url` 留空 = 走 provider 的官方默认端点，无歧义。
    ///
    /// 这一条必须在「模型名带命名空间」判定**之前**：OpenAI 的微调模型 ID
    /// 带冒号（`ft:gpt-4o:acme:123`），若顺序反了会被误判成聚合路由。
    #[test]
    fn a_blank_base_url_means_the_official_endpoint() {
        let anthropic = profile(Provider::Anthropic, "");
        assert_eq!(infer_vendor(&anthropic, "claude-opus-4-8"), "anthropic");
        let openai = profile(Provider::OpenAI, "");
        assert_eq!(infer_vendor(&openai, "ft:gpt-4o:acme:123"), "openai");
    }

    /// **移除 `Profile.vendor` 后，用户真实配置的推导结果必须与旧显式值一致。**
    ///
    /// 这组样本逐条取自一位用户的 `~/.wyj-code/config.toml`（6 个分组），
    /// `expected` 就是他原先手写的 `vendor`（没写的按推导补）。字段移除后
    /// 推导必须给出**同一个** vendor——否则他升级即行为变化，而变化的方向
    /// 可能是「给中转端点发厂商专属字段」→ 400。
    #[test]
    fn a_real_users_profiles_resolve_to_the_same_vendors_as_before() {
        for (provider, base_url, model, expected) in [
            // 厂商官方端点
            (
                Provider::Anthropic,
                "https://api.minimaxi.com/anthropic",
                "MiniMax-M3.1-Flash",
                "minimax",
            ),
            (
                Provider::OpenAI,
                "https://api.deepseek.com",
                "deepseek-flash",
                "deepseek",
            ),
            // 聚合路由 / 模型中转（原配置里都是显式 vendor = "custom"）
            (
                Provider::OpenAI,
                "https://openrouter.ai/api/v1",
                "minimax/minimax-m3.1-flash:free",
                "custom",
            ),
            (
                Provider::OpenAI,
                "http://192.168.5.5:7863/v1",
                "global:deepseek-v4.1-flash",
                "custom",
            ),
            (
                Provider::OpenAI,
                "https://larprouter.com/v1",
                "deepseek-v4.1-flash",
                "custom",
            ),
            (
                Provider::OpenAI,
                "https://api.openscale.so/v1",
                "deepseek/deepseek-v4.1-flash-1017",
                "custom",
            ),
        ] {
            let p = profile(provider, base_url);
            assert_eq!(
                infer_vendor(&p, model),
                expected,
                "{base_url} + {model} 应推出 {expected}（与移除 vendor 字段前一致）"
            );
        }
    }
}
