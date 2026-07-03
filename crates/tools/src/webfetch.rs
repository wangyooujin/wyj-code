//! WebFetch 工具 — 抓取网页并转换为纯文本

use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use scraper::{Html, Selector};
use serde::Deserialize;
use serde_json::Value;
use wyj_api::types::ToolDefinition;
use wyj_core::tool::{Tool, ToolContext, ToolResult};

const MAX_CONTENT: usize = 50_000;
const TIMEOUT_SECS: u64 = 30;

pub struct WebFetchTool {
    client: Client,
}

impl WebFetchTool {
    pub fn new() -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
            .user_agent("Mozilla/5.0 (compatible; wyj-code/0.1)")
            .build()
            .unwrap_or_default();
        Self { client }
    }
}

#[derive(Deserialize)]
struct Input {
    url: String,
    #[serde(default)]
    raw: Option<bool>,
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "WebFetch"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "抓取网页内容并转换为纯文本（移除 HTML 标签）。\
                适合读取文档、README、API 参考页面。\
                不适合需要 JavaScript 渲染的页面。"
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "要抓取的完整 URL"
                    },
                    "raw": {
                        "type": "boolean",
                        "description": "true 则返回原始 HTML，false（默认）返回纯文本"
                    }
                },
                "required": ["url"]
            }),
        }
    }

    /// WebFetch 是只读网络请求，无副作用，可安全并发执行。
    fn parallel_safe(&self) -> bool {
        true
    }

    async fn run(&self, input: Value, _ctx: &dyn ToolContext) -> Result<ToolResult> {
        let inp: Input = serde_json::from_value(input)?;

        let resp = self
            .client
            .get(&inp.url)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("请求失败: {e}"))?;

        let status = resp.status();
        if !status.is_success() {
            return Ok(ToolResult::err(format!("HTTP {status}: {}", inp.url)));
        }

        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        let bytes = resp.bytes().await?;
        let text = String::from_utf8_lossy(&bytes).to_string();

        if inp.raw.unwrap_or(false) {
            return Ok(ToolResult::ok(truncate(&text, MAX_CONTENT)));
        }

        let is_html = content_type.contains("html")
            || text.trim_start().starts_with("<!DOCTYPE")
            || text.trim_start().starts_with("<html");

        let result = if is_html { html_to_text(&text) } else { text };

        Ok(ToolResult::ok(truncate(&result, MAX_CONTENT)))
    }
}

fn html_to_text(html: &str) -> String {
    let doc = Html::parse_document(html);

    // 移除 script/style
    let _script_sel = Selector::parse("script, style, nav, footer, header").unwrap();
    // 提取正文文本
    let body_sel = Selector::parse("body").unwrap();

    let mut out = String::new();

    let root = if let Some(body) = doc.select(&body_sel).next() {
        body.text().collect::<Vec<_>>()
    } else {
        doc.root_element().text().collect::<Vec<_>>()
    };

    for chunk in root {
        let trimmed = chunk.trim();
        if !trimmed.is_empty() {
            out.push_str(trimmed);
            out.push('\n');
        }
    }

    // 压缩连续空行
    let mut result = String::new();
    let mut blank = 0usize;
    for line in out.lines() {
        if line.trim().is_empty() {
            blank += 1;
            if blank <= 1 {
                result.push('\n');
            }
        } else {
            blank = 0;
            result.push_str(line);
            result.push('\n');
        }
    }
    result
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!(
            "{}…（已截断，原长 {} 字节）",
            crate::textutil::truncate_str(s, max),
            s.len()
        )
    }
}
