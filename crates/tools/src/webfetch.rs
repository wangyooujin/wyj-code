//! WebFetch 工具 — 抓取网页并转换为纯文本

use anyhow::Result;
use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use scraper::{Html, Selector};
use serde::Deserialize;
use serde_json::Value;
use std::net::IpAddr;
use wyj_api::types::ToolDefinition;
use wyj_core::tool::{Tool, ToolContext, ToolResult};

const MAX_CONTENT: usize = 50_000;
const TIMEOUT_SECS: u64 = 30;
/// 响应体硬上限。旧实现 `resp.bytes()` 把整个响应读进内存，只在上限超过 50KB
/// 时截断**输出**——一个 2GB 的响应照样会先被完整读入并落盘到 spill。
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
/// 手动重定向的最大跳数（reqwest 默认 10，这里更保守）。
const MAX_REDIRECTS: usize = 5;

pub struct WebFetchTool {
    client: Client,
}

impl WebFetchTool {
    pub fn new() -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
            .user_agent("Mozilla/5.0 (compatible; wyj-code/0.1)")
            // 重定向必须自己跟：reqwest 的内置跟随不会重新校验目标 host，
            // 一个公网 URL 跳一次 302 就能把请求带进内网。
            .redirect(reqwest::redirect::Policy::none())
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
            description: crate::descriptions::WEBFETCH.to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": crate::descriptions::FIELD_WEBFETCH_URL
                    },
                    "raw": {
                        "type": "boolean",
                        "description": "Return raw HTML instead of extracted plain text (default false)"
                    }
                },
                "required": ["url"]
            }),
            native: None,
        }
    }

    /// WebFetch 是只读网络请求，无副作用，可安全并发执行。
    fn parallel_safe(&self) -> bool {
        true
    }

    async fn run(&self, input: Value, _ctx: &dyn ToolContext) -> Result<ToolResult> {
        let inp: Input = serde_json::from_value(input)?;

        // 手动跟重定向：每一跳都重新做 URL 校验，避免「公网地址 302 到
        // 169.254.169.254」这类绕过。
        let mut url = match validate_url(&inp.url) {
            Ok(url) => url,
            Err(reason) => return Ok(ToolResult::err(reason)),
        };
        let mut response = None;
        for hop in 0..=MAX_REDIRECTS {
            let resp = match self.client.get(url.clone()).send().await {
                Ok(resp) => resp,
                Err(e) => return Ok(ToolResult::err(format!("请求失败: {e}"))),
            };
            if resp.status().is_redirection() {
                if hop == MAX_REDIRECTS {
                    return Ok(ToolResult::err(format!(
                        "重定向次数超过上限（{MAX_REDIRECTS}）: {}",
                        inp.url
                    )));
                }
                let Some(location) = resp.headers().get(reqwest::header::LOCATION) else {
                    return Ok(ToolResult::err("重定向响应缺少 Location 头"));
                };
                let Ok(location) = location.to_str() else {
                    return Ok(ToolResult::err("重定向 Location 头不是合法 UTF-8"));
                };
                url = match url
                    .join(location)
                    .map_err(|e| e.to_string())
                    .and_then(|next| validate_url(next.as_str()))
                {
                    Ok(next) => next,
                    Err(reason) => {
                        return Ok(ToolResult::err(format!("拒绝重定向目标: {reason}")));
                    }
                };
                continue;
            }
            response = Some(resp);
            break;
        }
        let Some(resp) = response else {
            return Ok(ToolResult::err("请求未得到最终响应"));
        };

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

        let bytes = match read_capped(resp).await {
            Ok(bytes) => bytes,
            Err(reason) => return Ok(ToolResult::err(reason)),
        };
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

/// URL 校验：只允许 http/https，且目标不得是环回 / 私有 / 链路本地地址。
///
/// 威胁模型很具体：WebFetch 是模型可自由调用、且**免权限确认**的工具。没有
/// 这层校验时，一句「帮我看看 http://169.254.169.254/latest/meta-data/」就能
/// 让模型读到云主机凭据，`http://127.0.0.1:8080/admin` 也能打到本机服务。
///
/// 注意 DNS 解析结果是**解析时**的地址，域名可以在两次解析之间改指向（DNS
/// rebinding）。彻底解决需要把连接钉死在已校验的 IP 上；这里先把最直接的
/// 形态挡住，并在注释里写明残留风险。
fn validate_url(raw: &str) -> Result<url::Url, String> {
    let url = url::Url::parse(raw.trim()).map_err(|e| format!("URL 不合法: {e}"))?;
    match url.scheme() {
        "http" | "https" => {}
        other => return Err(format!("只允许 http/https，拒绝 {other}")),
    }
    let Some(host) = url.host_str().map(str::to_string) else {
        return Err("URL 缺少主机名".to_string());
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".localhost") || host.ends_with(".local") {
        return Err(format!("拒绝访问本机/局域网主机: {host}"));
    }
    // 字面量 IP 直接判定；域名做一次解析，任一结果落在受限网段就拒绝。
    if let Ok(ip) = host.parse::<IpAddr>() {
        return check_ip(ip, &host).map(|_| url);
    }
    if host.ends_with(".internal") {
        return Err(format!("拒绝访问内部域名: {host}"));
    }
    let addrs = std::net::ToSocketAddrs::to_socket_addrs(&(host.as_str(), 0))
        .map_err(|e| format!("无法解析主机 {host}: {e}"))?;
    for addr in addrs {
        check_ip(addr.ip(), &host)?;
    }
    Ok(url)
}

fn check_ip(ip: IpAddr, host: &str) -> Result<(), String> {
    let blocked = match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                // 云元数据服务：169.254.169.254 已被 link_local 覆盖，这里补
                // 阿里云/腾讯云等使用的 100.64/10 与 192.0.0.0/24。
                || (v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1]))
                || v4.octets()[..3] == [192, 0, 0]
        }
        IpAddr::V6(v6) => {
            // `is_unique_local` / `is_unicast_link_local` 在 1.84 才稳定，而本
            // crate 声明的 MSRV 是 1.80，因此按字节判定 fc00::/7 与 fe80::/10。
            let octets = v6.octets();
            v6.is_loopback()
                || v6.is_unspecified()
                || (octets[0] & 0xfe) == 0xfc
                || (octets[0] == 0xfe && (octets[1] & 0xc0) == 0x80)
                // IPv4-mapped（::ffff:127.0.0.1）要按内嵌的 v4 再判一次。
                || v6
                    .to_ipv4_mapped()
                    .map(|v4| v4.is_loopback() || v4.is_private() || v4.is_link_local())
                    .unwrap_or(false)
        }
    };
    if blocked {
        Err(format!("拒绝访问受限地址 {ip}（主机 {host}）"))
    } else {
        Ok(())
    }
}

/// 流式读取响应体并在超限时立即停止——不是读完再截断。
async fn read_capped(resp: reqwest::Response) -> Result<Vec<u8>, String> {
    if let Some(len) = resp.content_length() {
        if len as usize > MAX_BODY_BYTES {
            return Err(format!(
                "响应体过大（{len} 字节，上限 {MAX_BODY_BYTES}），已拒绝下载"
            ));
        }
    }
    let mut stream = resp.bytes_stream();
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("读取响应失败: {e}"))?;
        if out.len() + chunk.len() > MAX_BODY_BYTES {
            return Err(format!("响应体超过上限 {MAX_BODY_BYTES} 字节，已中断下载"));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
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
        // 只保留开头会让长文档的结论段彻底丢失，而网页正文的有效信息常常在
        // 中后段。走 spill：头尾各半 + 完整内容落盘，模型可按需 Read 取回。
        crate::spill::spill_truncated(s, max * 7 / 10, max * 3 / 10, "webfetch")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_http_schemes() {
        assert!(validate_url("file:///etc/passwd").is_err());
        assert!(validate_url("ftp://example.com/x").is_err());
        assert!(validate_url("gopher://example.com").is_err());
    }

    #[test]
    fn rejects_loopback_private_and_metadata_endpoints() {
        for raw in [
            "http://localhost/",
            "http://127.0.0.1:8080/admin",
            "http://127.1.2.3/",
            "http://10.0.0.1/",
            "http://172.16.0.1/",
            "http://192.168.1.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://100.100.100.200/",
            "http://[::1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://[fd00::1]/",
            "http://[fe80::1]/",
            "http://foo.localhost/",
            "http://metadata.internal/",
        ] {
            assert!(validate_url(raw).is_err(), "{raw} 必须被拒绝");
        }
    }

    #[test]
    fn accepts_public_https_urls() {
        // 只做字面量 IP / scheme 判定，不依赖 DNS。
        assert!(validate_url("https://1.1.1.1/").is_ok());
        assert!(validate_url("https://8.8.8.8/dns-query").is_ok());
    }

    #[test]
    fn ip_check_blocks_ipv4_mapped_loopback() {
        assert!(check_ip("::ffff:127.0.0.1".parse().unwrap(), "x").is_err());
        assert!(check_ip("1.1.1.1".parse().unwrap(), "x").is_ok());
    }
}
