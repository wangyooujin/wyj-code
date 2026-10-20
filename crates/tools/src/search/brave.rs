//! Brave Search 后端（`GET https://api.search.brave.com/res/v1/web/search`）。
//!
//! 与 Tavily 的两处结构性差异，都在这里被吸收掉：
//!
//! * **没有 `topic` 参数**，只有 `freshness` 时效窗口。所以 [`BraveBackend::supports_topic`]
//!   返回 `false`，工具层对 `topic` 请求直接报错——静默降级成通用搜索会让模型
//!   在「找新闻」时拿到一堆常青博客还以为检索生效了。
//! * **域过滤靠查询运算符**（`site:` / `-site:`）而不是独立字段，需要在拼 URL 时
//!   把域名注入 query 字符串。数学上等价，但表达式要带括号，否则 `site:a OR site:b`
//!   会被解析成 `(原查询 AND site:a) OR site:b`。
//!
//! 认证走 `X-Subscription-Token` 头（Brave 的约定），同样不进 URL——URL 会进
//! 访问日志、Referer 与 shell 历史。

use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;

use super::{SearchBackend, SearchError, SearchHit, SearchRequest, SearchResponse, TimeRange};

const ENDPOINT: &str = "https://api.search.brave.com/res/v1/web/search";

pub struct BraveBackend {
    client: Client,
    api_key: String,
    /// `true` → `safesearch=strict`。
    safe_search: bool,
    /// ISO 639-1 语言码，空串表示不指定（走 Brave 默认）。
    language: String,
}

impl BraveBackend {
    pub fn new(
        client: Client,
        api_key: impl Into<String>,
        safe_search: bool,
        language: impl Into<String>,
    ) -> Self {
        Self {
            client,
            api_key: api_key.into(),
            safe_search,
            language: language.into(),
        }
    }
}

#[async_trait]
impl SearchBackend for BraveBackend {
    fn name(&self) -> &'static str {
        "brave"
    }

    fn supports_topic(&self) -> bool {
        false
    }

    async fn search(&self, req: &SearchRequest) -> Result<SearchResponse, SearchError> {
        let url = build_url(req, self.safe_search, &self.language);
        let resp = self
            .client
            .get(url)
            .header("X-Subscription-Token", &self.api_key)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| SearchError::Transient(e.to_string()))?;

        let status = resp.status().as_u16();
        let retry_after = retry_after_secs(&resp);
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

/// `freshness` 取值是 Brave 的单字母缩写（`pd`=past day 等）。
fn freshness_code(range: TimeRange) -> &'static str {
    match range {
        TimeRange::Day => "pd",
        TimeRange::Week => "pw",
        TimeRange::Month => "pm",
        TimeRange::Year => "py",
    }
}

/// 把域过滤注入 query。`include` 用括号包裹整体，`exclude` 用 `-site:`。
fn apply_domain_filters(query: &str, req: &SearchRequest) -> String {
    let mut out = query.to_string();
    if !req.include_domains.is_empty() {
        let terms: Vec<String> = req
            .include_domains
            .iter()
            .map(|d| format!("site:{d}"))
            .collect();
        out.push_str(&format!(" ({})", terms.join(" OR ")));
    }
    for domain in &req.exclude_domains {
        out.push_str(&format!(" -site:{domain}"));
    }
    out
}

/// 构造完整 URL（含 query string）。纯函数，单测直接断言字符串。
fn build_url(req: &SearchRequest, safe_search: bool, language: &str) -> String {
    let query = apply_domain_filters(&req.query, req);
    let mut url = format!(
        "{ENDPOINT}?q={}&count={}",
        urlencode(&query),
        req.max_results.min(20)
    );
    if let Some(range) = req.time_range {
        url.push_str(&format!("&freshness={}", freshness_code(range)));
    }
    // Brave 的 safesearch 三档，这里只暴露「严/中」两档：off 交给配置里显式关闭。
    url.push_str(if safe_search {
        "&safesearch=strict"
    } else {
        "&safesearch=off"
    });
    if !language.trim().is_empty() {
        url.push_str(&format!("&search_lang={}", urlencode(language.trim())));
    }
    url
}

/// 最小 percent-encoding。只转义会破坏 query string 的字符，保留 `:`/`-`/空格
/// 可读（`site:docs.rs` 与 `-site:x` 里的运算符必须原样传过去，不能被编成
/// `%3A`——Brave 与 Google 都接受编码形式，但保留原样便于排障时肉眼核对）。
fn urlencode(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 16);
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b':' => {
                out.push(byte as char)
            }
            b' ' => out.push_str("%20"),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// 429 的等待秒数：优先 `Retry-After`，其次 Brave 的 `X-RateLimit-Reset`
/// （Unix 时间戳，需换算成「还有多少秒」）。
fn retry_after_secs(resp: &reqwest::Response) -> Option<u64> {
    let header = |name: &str| {
        resp.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    if let Some(secs) = header("retry-after").and_then(|v| v.trim().parse::<u64>().ok()) {
        return Some(secs);
    }
    let reset = header("x-ratelimit-reset")?.trim().parse::<u64>().ok()?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(reset.saturating_sub(now))
}

fn map_error(status: u16, retry_after: Option<u64>, body: &str) -> SearchError {
    let detail = extract_error_detail(body);
    match status {
        401 | 403 => SearchError::Auth(detail),
        429 => SearchError::RateLimited { retry_after },
        // 402 = 免费档/付费档配额用尽。这不是「稍后重试」能解决的，措辞必须
        // 明确指向配额，否则用户会一直重试到怀疑 key 坏了。
        402 => SearchError::BadRequest(format!(
            "配额已用尽（升级套餐或等待下个计费周期）: {detail}"
        )),
        400 | 422 => SearchError::BadRequest(detail),
        s if (400..500).contains(&s) => SearchError::BadRequest(format!("HTTP {s}: {detail}")),
        s => SearchError::Transient(format!("HTTP {s}: {detail}")),
    }
}

/// Brave 错误体形如 `{"error": {"status": 401, "detail": "..."}}`，
/// 也见过平铺的 `{"message": "..."}`，两种都试。
fn extract_error_detail(body: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return crate::textutil::truncate_str(body.trim(), 300).to_string();
    };
    for path in [
        &["error", "detail"][..],
        &["error", "message"][..],
        &["message"][..],
    ] {
        let mut cursor = &value;
        let mut found = true;
        for key in path {
            match cursor.get(*key) {
                Some(next) => cursor = next,
                None => {
                    found = false;
                    break;
                }
            }
        }
        if found {
            if let Some(s) = cursor.as_str() {
                return s.to_string();
            }
        }
    }
    // 兜底：`{"error": "..."}` 这种直接把 error 写成字符串的形状。
    if let Some(s) = value.get("error").and_then(|v| v.as_str()) {
        return s.to_string();
    }
    crate::textutil::truncate_str(body.trim(), 300).to_string()
}

#[derive(Deserialize)]
struct BraveResponse {
    #[serde(default)]
    query: Option<BraveQuery>,
    #[serde(default)]
    web: Option<BraveWeb>,
}

#[derive(Deserialize)]
struct BraveQuery {
    #[serde(default)]
    more_results_available: bool,
}

#[derive(Deserialize)]
struct BraveWeb {
    #[serde(default)]
    results: Vec<BraveResult>,
}

#[derive(Deserialize)]
struct BraveResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    age: Option<String>,
}

/// 解析 Brave 响应。纯函数。
fn parse_response(raw: &str) -> Result<SearchResponse, SearchError> {
    let parsed: BraveResponse =
        serde_json::from_str(raw).map_err(|e| SearchError::Decode(e.to_string()))?;
    let hits = parsed
        .web
        .map(|w| w.results)
        .unwrap_or_default()
        .into_iter()
        .map(|r| SearchHit {
            title: strip_markup(&r.title),
            url: r.url,
            // Brave 的 description 里带 `<strong>` 高亮标签，直接回填会让模型
            // 以为那是网页原文的一部分。
            snippet: strip_markup(&r.description),
            published: r.age.filter(|s| !s.trim().is_empty()),
            // Brave 不返回相关度分数。
            score: None,
        })
        .collect();
    Ok(SearchResponse {
        // Brave 没有 LLM 摘要；答案由模型自己从 snippet 里组织。
        answer: None,
        hits,
        more_available: parsed
            .query
            .map(|q| q.more_results_available)
            .unwrap_or(false),
    })
}

/// 去掉 Brave 高亮用的少量 HTML 标签并还原常见实体。
/// 不做通用 HTML 解析——这里的输入只可能是行内强调标签。
fn strip_markup(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_tag = false;
    for ch in input.chars() {
        match ch {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if in_tag => {}
            _ => out.push(ch),
        }
    }
    out.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::Topic;

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
    fn brave_does_not_claim_topic_support() {
        // 这条断言是「静默降级」的防线：一旦有人为了「方便」把它改成 true，
        // topic: news 的请求就会拿到通用结果而没有任何提示。
        let backend = BraveBackend::new(Client::new(), "k", true, "");
        assert!(!backend.supports_topic());
    }

    #[test]
    fn api_key_never_appears_in_the_url() {
        let url = build_url(&req(), true, "en");
        assert!(!url.contains('k'), "{url}");
        assert!(!url.contains("token"), "{url}");
    }

    #[test]
    fn url_carries_query_count_and_freshness() {
        let mut r = req();
        r.time_range = Some(TimeRange::Week);
        let url = build_url(&r, true, "zh");
        assert!(url.contains("q=rust%20async"), "{url}");
        assert!(url.contains("count=5"), "{url}");
        assert!(url.contains("freshness=pw"), "{url}");
        assert!(url.contains("search_lang=zh"), "{url}");
    }

    #[test]
    fn freshness_codes_cover_every_window() {
        assert_eq!(freshness_code(TimeRange::Day), "pd");
        assert_eq!(freshness_code(TimeRange::Week), "pw");
        assert_eq!(freshness_code(TimeRange::Month), "pm");
        assert_eq!(freshness_code(TimeRange::Year), "py");
    }

    #[test]
    fn domain_filters_become_site_operators_and_parenthesize_the_or() {
        let mut r = req();
        r.include_domains = vec!["docs.rs".to_string(), "rust-lang.org".to_string()];
        r.exclude_domains = vec!["spam.example".to_string()];
        let q = apply_domain_filters(&r.query, &r);
        // 括号不能少：否则 OR 只作用于最后一个 site:，前面的会被 AND 掉。
        assert_eq!(
            q,
            "rust async (site:docs.rs OR site:rust-lang.org) -site:spam.example"
        );
    }

    #[test]
    fn safesearch_and_language_reflect_configuration() {
        assert!(build_url(&req(), true, "").contains("safesearch=strict"));
        assert!(build_url(&req(), false, "").contains("safesearch=off"));
        // 未配置语言时不发该参数，让 Brave 用自己的默认值而不是被空串干扰。
        assert!(!build_url(&req(), true, "  ").contains("search_lang"));
    }

    #[test]
    fn count_is_capped_at_braves_limit() {
        let mut r = req();
        r.max_results = 50;
        assert!(build_url(&r, true, "").contains("count=20"));
    }

    #[test]
    fn parses_web_results_age_and_more_available() {
        let raw = r#"{
            "query": {"original": "rust async", "more_results_available": true},
            "web": {
                "results": [
                    {
                        "title": "Async <strong>Book</strong>",
                        "url": "https://rust-lang.github.io/async-book/",
                        "description": "The <strong>async</strong> book &amp; guide",
                        "age": "3 days ago"
                    }
                ]
            }
        }"#;
        let resp = parse_response(raw).unwrap();
        assert!(resp.answer.is_none(), "Brave 不提供 LLM 摘要");
        assert!(resp.more_available);
        assert_eq!(resp.hits.len(), 1);
        // 高亮标签必须被剥掉，否则模型会把 <strong> 当成网页原文。
        assert_eq!(resp.hits[0].title, "Async Book");
        assert_eq!(resp.hits[0].snippet, "The async book & guide");
        assert_eq!(resp.hits[0].published.as_deref(), Some("3 days ago"));
        assert!(resp.hits[0].score.is_none());
    }

    #[test]
    fn missing_web_cluster_yields_no_results_not_an_error() {
        let resp = parse_response(r#"{"query":{"original":"x"}}"#).unwrap();
        assert!(resp.hits.is_empty());
        assert!(!resp.more_available);
    }

    #[test]
    fn malformed_json_becomes_a_decode_error() {
        assert!(matches!(
            parse_response("<html>503</html>"),
            Err(SearchError::Decode(_))
        ));
    }

    #[test]
    fn quota_exhaustion_is_not_reported_as_a_transient_retry() {
        // 402 若被归到 Transient，工具会自动重试一次并给用户「稍后重试」的错误——
        // 而实际要等到下个计费周期。必须落到不可重试的分支。
        match map_error(402, None, r#"{"error":{"detail":"quota"}}"#) {
            SearchError::BadRequest(msg) => assert!(msg.contains("配额"), "{msg}"),
            other => panic!("402 应为不可重试的配额错误，实际 {other:?}"),
        }
    }

    #[test]
    fn status_codes_map_to_actionable_errors() {
        assert!(matches!(map_error(401, None, "{}"), SearchError::Auth(_)));
        assert!(matches!(map_error(403, None, "{}"), SearchError::Auth(_)));
        match map_error(429, Some(12), "{}") {
            SearchError::RateLimited { retry_after } => assert_eq!(retry_after, Some(12)),
            other => panic!("429 应为限流，实际 {other:?}"),
        }
        assert!(matches!(
            map_error(422, None, "{}"),
            SearchError::BadRequest(_)
        ));
        assert!(matches!(
            map_error(500, None, "{}"),
            SearchError::Transient(_)
        ));
    }

    #[test]
    fn error_detail_is_extracted_from_braves_nested_shape() {
        assert_eq!(
            extract_error_detail(r#"{"error":{"status":422,"detail":"Invalid q"}}"#),
            "Invalid q"
        );
        assert_eq!(
            extract_error_detail(r#"{"message":"Unauthorized"}"#),
            "Unauthorized"
        );
        assert_eq!(extract_error_detail("plain text"), "plain text");
    }

    #[test]
    fn urlencode_preserves_operators_and_escapes_spaces() {
        assert_eq!(urlencode("site:docs.rs"), "site:docs.rs");
        assert_eq!(urlencode("a b"), "a%20b");
        assert_eq!(urlencode("a&b=c"), "a%26b%3Dc");
    }
}
