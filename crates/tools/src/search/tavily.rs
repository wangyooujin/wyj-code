//! Tavily 后端（`POST https://api.tavily.com/search`）。
//!
//! 参数选择的两点理由：
//!
//! * `search_depth: "basic"` 而非 `"advanced"` —— 后者花 2 个 credit 换更高相关度，
//!   但本工具给模型的是「摘要 + 链接」，深读交给 WebFetch，多付一倍配额不值。
//! * `chunks_per_source: 1` —— 默认 3 段会让每条结果的 `content` 膨胀到 1.5KB 以上，
//!   10 条就是 15KB 上下文，而我们只需要一句能判断「值不值得点进去」的摘要。
//!
//! **API Key 走 `Authorization` 头**（Tavily 官方支持 Bearer）。旧实现把 key 放进
//! JSON body，请求一旦被代理日志/错误上报记录就泄漏——body 是会被完整打印的，头不会。

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;

use super::{SearchBackend, SearchError, SearchHit, SearchRequest, SearchResponse};

const ENDPOINT: &str = "https://api.tavily.com/search";

pub struct TavilyBackend {
    client: Client,
    api_key: String,
}

impl TavilyBackend {
    pub fn new(client: Client, api_key: impl Into<String>) -> Self {
        Self {
            client,
            api_key: api_key.into(),
        }
    }
}

#[async_trait]
impl SearchBackend for TavilyBackend {
    fn name(&self) -> &'static str {
        "tavily"
    }

    fn supports_topic(&self) -> bool {
        // Tavily 的 topic 枚举为 general/news/finance。
        true
    }

    async fn search(&self, req: &SearchRequest) -> Result<SearchResponse, SearchError> {
        let body = build_request_body(req);
        let resp = self
            .client
            .post(ENDPOINT)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&body)
            .send()
            .await
            .map_err(|e| SearchError::Transient(e.to_string()))?;

        let status = resp.status().as_u16();
        let retry_after = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok());
        let text = resp
            .text()
            .await
            .map_err(|e| SearchError::Transient(e.to_string()))?;

        if !(200..300).contains(&status) {
            return Err(map_error(status, retry_after, &text));
        }
        parse_response(&text)
    }
}

/// 构造 Tavily 请求体。纯函数，便于断言「key 不在 body 里」。
fn build_request_body(req: &SearchRequest) -> Value {
    let mut body = serde_json::json!({
        "query": req.query,
        "max_results": req.max_results,
        "search_depth": "basic",
        "chunks_per_source": 1,
        "topic": req.topic.as_str(),
        "include_answer": true,
        // 不带发布日期，模型无从判断「这条是不是去年的旧文档」。
        "include_published_date": true,
    });
    let obj = body.as_object_mut().expect("json! 产出必为 object");
    if let Some(time_range) = req.time_range {
        obj.insert(
            "time_range".to_string(),
            Value::String(time_range.as_str().to_string()),
        );
    }
    // 空数组不发送：部分网关对「显式空过滤器」的处理并不一致，省略更稳。
    if !req.include_domains.is_empty() {
        obj.insert(
            "include_domains".to_string(),
            serde_json::json!(req.include_domains),
        );
    }
    if !req.exclude_domains.is_empty() {
        obj.insert(
            "exclude_domains".to_string(),
            serde_json::json!(req.exclude_domains),
        );
    }
    body
}

/// HTTP 状态码 → 语义化错误。分类决定调用方是否重试、以及模型该怎么做。
fn map_error(status: u16, retry_after: Option<u64>, body: &str) -> SearchError {
    let detail = extract_error_detail(body);
    match status {
        401 | 403 => SearchError::Auth(detail),
        429 | 432 | 433 => SearchError::RateLimited { retry_after },
        400 | 422 => SearchError::BadRequest(detail),
        // 其余 4xx 是请求构造问题（如域名格式），5xx 是对方故障。
        s if (400..500).contains(&s) => SearchError::BadRequest(format!("HTTP {s}: {detail}")),
        s => SearchError::Transient(format!("HTTP {s}: {detail}")),
    }
}

/// 从 Tavily 的错误体里抠出人类可读的一句。
/// 形状有两种：`{"detail": {"error": "..."}}` 与 `{"detail": [{"msg": "..."}]}`。
fn extract_error_detail(body: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return crate::textutil::truncate_str(body.trim(), 300).to_string();
    };
    if let Some(detail) = value.get("detail") {
        if let Some(msg) = detail.as_str() {
            return msg.to_string();
        }
        if let Some(err) = detail.get("error").and_then(|v| v.as_str()) {
            return err.to_string();
        }
        if let Some(arr) = detail.as_array() {
            let msgs: Vec<String> = arr
                .iter()
                .filter_map(|item| item.get("msg").and_then(|v| v.as_str()))
                .map(str::to_string)
                .collect();
            if !msgs.is_empty() {
                return msgs.join("; ");
            }
        }
    }
    crate::textutil::truncate_str(body.trim(), 300).to_string()
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
    #[serde(default)]
    score: Option<f32>,
    #[serde(default)]
    published_date: Option<String>,
}

/// 解析 Tavily 响应。纯函数，单测直接喂 fixture 字符串。
fn parse_response(raw: &str) -> Result<SearchResponse, SearchError> {
    let parsed: TavilyResponse =
        serde_json::from_str(raw).map_err(|e| SearchError::Decode(e.to_string()))?;
    let hits = parsed
        .results
        .into_iter()
        .map(|r| SearchHit {
            title: r.title,
            url: r.url,
            snippet: r.content,
            published: r.published_date.filter(|s| !s.trim().is_empty()),
            score: r.score,
        })
        .collect();
    Ok(SearchResponse {
        answer: parsed.answer.filter(|s| !s.trim().is_empty()),
        hits,
        // Tavily 的 /search 不返回「还有更多」标记。
        more_available: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::{TimeRange, Topic};

    fn req() -> SearchRequest {
        SearchRequest {
            query: "rust async".to_string(),
            max_results: 5,
            topic: Topic::General,
            time_range: None,
            include_domains: vec![],
            exclude_domains: vec![],
        }
    }

    #[test]
    fn api_key_never_appears_in_the_request_body() {
        // key 只走 Authorization 头；body 里出现任何形式的 key 都是回归。
        let body = build_request_body(&req()).to_string();
        assert!(!body.contains("api_key"), "{body}");
        assert!(!body.contains("Bearer"), "{body}");
    }

    #[test]
    fn request_body_carries_query_and_filters() {
        let mut r = req();
        r.time_range = Some(TimeRange::Week);
        r.include_domains = vec!["docs.rs".to_string()];
        r.exclude_domains = vec!["spam.example".to_string()];
        let body = build_request_body(&r);
        assert_eq!(body["query"], "rust async");
        assert_eq!(body["max_results"], 5);
        assert_eq!(body["search_depth"], "basic");
        assert_eq!(body["chunks_per_source"], 1);
        assert_eq!(body["time_range"], "week");
        assert_eq!(body["include_domains"][0], "docs.rs");
        assert_eq!(body["exclude_domains"][0], "spam.example");
    }

    #[test]
    fn empty_domain_filters_are_omitted_entirely() {
        let body = build_request_body(&req());
        assert!(body.get("include_domains").is_none(), "{body}");
        assert!(body.get("exclude_domains").is_none(), "{body}");
        assert!(body.get("time_range").is_none(), "{body}");
    }

    #[test]
    fn topic_is_forwarded_lowercase() {
        let mut r = req();
        r.topic = Topic::News;
        assert_eq!(build_request_body(&r)["topic"], "news");
        r.topic = Topic::Finance;
        assert_eq!(build_request_body(&r)["topic"], "finance");
    }

    #[test]
    fn parses_answer_results_and_metadata() {
        let raw = r#"{
            "query": "rust async",
            "answer": "async/await 是 Rust 的异步语法",
            "results": [
                {
                    "title": "Async Book",
                    "url": "https://rust-lang.github.io/async-book/",
                    "content": "The async book",
                    "score": 0.81,
                    "published_date": "Tue, 11 Mar 2025 17:00:00 GMT"
                }
            ]
        }"#;
        let resp = parse_response(raw).unwrap();
        assert_eq!(
            resp.answer.as_deref(),
            Some("async/await 是 Rust 的异步语法")
        );
        assert_eq!(resp.hits.len(), 1);
        assert_eq!(resp.hits[0].title, "Async Book");
        assert_eq!(resp.hits[0].url, "https://rust-lang.github.io/async-book/");
        assert_eq!(resp.hits[0].snippet, "The async book");
        assert_eq!(resp.hits[0].score, Some(0.81));
        assert!(resp.hits[0].published.as_deref().unwrap().contains("2025"));
        assert!(!resp.more_available);
    }

    #[test]
    fn missing_and_blank_fields_degrade_to_none() {
        let raw = r#"{"results":[{"title":"t","url":"u","content":"c","published_date":"  "}]}"#;
        let resp = parse_response(raw).unwrap();
        assert!(resp.answer.is_none());
        assert!(resp.hits[0].published.is_none());
        assert!(resp.hits[0].score.is_none());
    }

    #[test]
    fn empty_result_set_is_not_an_error() {
        let resp = parse_response(r#"{"results":[]}"#).unwrap();
        assert!(resp.hits.is_empty());
    }

    #[test]
    fn malformed_json_becomes_a_decode_error() {
        assert!(matches!(
            parse_response("not json"),
            Err(SearchError::Decode(_))
        ));
    }

    #[test]
    fn status_codes_map_to_actionable_errors() {
        assert!(matches!(map_error(401, None, "{}"), SearchError::Auth(_)));
        assert!(matches!(map_error(403, None, "{}"), SearchError::Auth(_)));
        for code in [429, 432, 433] {
            match map_error(code, Some(60), "{}") {
                SearchError::RateLimited { retry_after } => assert_eq!(retry_after, Some(60)),
                other => panic!("{code} 应为限流，实际 {other:?}"),
            }
        }
        assert!(matches!(
            map_error(422, None, "{}"),
            SearchError::BadRequest(_)
        ));
        assert!(matches!(
            map_error(404, None, "{}"),
            SearchError::BadRequest(_)
        ));
        assert!(matches!(
            map_error(500, None, "{}"),
            SearchError::Transient(_)
        ));
        assert!(matches!(
            map_error(503, None, "{}"),
            SearchError::Transient(_)
        ));
    }

    #[test]
    fn error_detail_is_extracted_from_both_documented_shapes() {
        assert_eq!(
            extract_error_detail(r#"{"detail":{"error":"Unauthorized"}}"#),
            "Unauthorized"
        );
        assert_eq!(
            extract_error_detail(
                r#"{"detail":[{"loc":["body","query"],"msg":"Input should be a valid string"}]}"#
            ),
            "Input should be a valid string"
        );
        // 非 JSON 原样回退（截断），不能因为解析不了就给出空错误信息。
        assert_eq!(
            extract_error_detail("  gateway timeout  "),
            "gateway timeout"
        );
    }
}
