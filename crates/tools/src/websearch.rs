//! WebSearch 工具 — 联网搜索（Tavily provider）
//!
//! 仅当配置了搜索 API Key 时才注册（见 cli 侧装配），未配置则模型看不到本工具。

use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;
use wyj_api::types::ToolDefinition;
use wyj_core::tool::{Tool, ToolContext, ToolResult};

const TAVILY_ENDPOINT: &str = "https://api.tavily.com/search";
const TIMEOUT_SECS: u64 = 30;
const DEFAULT_MAX_RESULTS: u32 = 5;
const MAX_SNIPPET: usize = 600;

pub struct WebSearchTool {
    client: Client,
    api_key: String,
}

impl WebSearchTool {
    pub fn new(api_key: impl Into<String>) -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
            .user_agent("wyj-code/1.0")
            .build()
            .unwrap_or_default();
        Self {
            client,
            api_key: api_key.into(),
        }
    }
}

#[derive(Deserialize)]
struct Input {
    query: String,
    #[serde(default)]
    max_results: Option<u32>,
}

#[derive(Deserialize)]
struct TavilyResponse {
    #[serde(default)]
    answer: Option<String>,
    #[serde(default)]
    results: Vec<TavilyResult>,
}

#[derive(Deserialize)]
struct TavilyResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    content: String,
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "WebSearch"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: crate::descriptions::WEBSEARCH.to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": crate::descriptions::FIELD_WEBSEARCH_QUERY
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "Maximum number of results to return (default 5, max 10)"
                    }
                },
                "required": ["query"]
            }),
        }
    }

    /// 只读网络请求，无副作用，可安全并发执行。
    fn parallel_safe(&self) -> bool {
        true
    }

    async fn run(&self, input: Value, _ctx: &dyn ToolContext) -> Result<ToolResult> {
        let inp: Input = serde_json::from_value(input)?;
        if inp.query.trim().is_empty() {
            return Ok(ToolResult::err("query 不能为空"));
        }
        let max_results = inp.max_results.unwrap_or(DEFAULT_MAX_RESULTS).clamp(1, 10);

        let body = serde_json::json!({
            "api_key": self.api_key,
            "query": inp.query,
            "max_results": max_results,
            "search_depth": "basic",
            "include_answer": true,
        });

        let resp = self
            .client
            .post(TAVILY_ENDPOINT)
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("搜索请求失败: {e}"))?;

        let status = resp.status();
        if !status.is_success() {
            let detail = resp.text().await.unwrap_or_default();
            return Ok(ToolResult::err(format!(
                "搜索失败 (HTTP {status}): {}",
                detail.trim()
            )));
        }

        let parsed: TavilyResponse = resp
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("解析搜索结果失败: {e}"))?;

        Ok(ToolResult::ok(format_results(&inp.query, &parsed)))
    }
}

/// 把 Tavily 响应格式化为紧凑文本，供模型消费。
fn format_results(query: &str, resp: &TavilyResponse) -> String {
    let mut out = format!("搜索: {query}\n");
    if let Some(answer) = &resp.answer {
        let a = answer.trim();
        if !a.is_empty() {
            out.push_str(&format!("\n摘要: {a}\n"));
        }
    }
    if resp.results.is_empty() {
        out.push_str("\n（无结果）");
        return out;
    }
    out.push_str("\n结果:\n");
    for (i, r) in resp.results.iter().enumerate() {
        let snippet = crate::textutil::truncate_str(r.content.trim(), MAX_SNIPPET);
        out.push_str(&format!(
            "{}. {}\n   {}\n   {}\n",
            i + 1,
            r.title.trim(),
            r.url.trim(),
            snippet
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_results_with_answer() {
        let resp = TavilyResponse {
            answer: Some("Rust 是一门系统编程语言".to_string()),
            results: vec![TavilyResult {
                title: "Rust".to_string(),
                url: "https://rust-lang.org".to_string(),
                content: "Rust official site".to_string(),
            }],
        };
        let text = format_results("rust", &resp);
        assert!(text.contains("摘要: Rust 是一门系统编程语言"));
        assert!(text.contains("1. Rust"));
        assert!(text.contains("https://rust-lang.org"));
    }

    #[test]
    fn formats_empty_results() {
        let resp = TavilyResponse {
            answer: None,
            results: vec![],
        };
        let text = format_results("nothing", &resp);
        assert!(text.contains("（无结果）"));
    }
}
