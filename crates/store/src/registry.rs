//! 官方 MCP Registry HTTP client（`https://registry.modelcontextprotocol.io`）
//!
//! 已对照真实 API 响应核实：`GET /v0/servers` 支持 `search` 关键字查询参数
//! （服务端过滤，如 `?search=fetch`），且 `servers[]` 每个元素是
//! `{"server": {...}, "_meta": {...}}` 的包装对象（`RegistryServerEntry`），
//! 不是把 name/description/packages 直接摊平在顶层。

use crate::lockfile;
use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;

pub const REGISTRY_BASE_URL: &str = "https://registry.modelcontextprotocol.io";
const TIMEOUT_SECS: u64 = 20;
const DEFAULT_LIST_LIMIT: u32 = 100;
const SEARCH_PAGE_CAP: u32 = 500;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryListResponse {
    pub servers: Vec<RegistryServerEntry>,
    pub metadata: RegistryMetadata,
}

/// 官方 API 每个 servers[] 元素实际是 `{"server": {...}, "_meta": {...}}`，
/// 而不是把 name/description/packages 等字段直接摊平在顶层。
///
/// 重要：同一个 server 名字会在结果里出现多条——registry 返回的是**全部历史
/// 版本**，不是去重后的最新版，用 `_meta` 里的 `isLatest` 标记区分。忽略这个
/// 字段会导致搜索列表里同一个 server 重复出现多次、且安装/升级可能捡到旧版本。
#[derive(Debug, Clone, Deserialize)]
pub struct RegistryServerEntry {
    pub server: RegistryServerSummary,
    #[serde(default, rename = "_meta")]
    pub meta: Option<RegistryEntryMeta>,
}

impl RegistryServerEntry {
    /// 是否是该 server 名下的最新版本。缺少 `_meta` 信息时保守地当作最新
    /// （避免未来 schema 变化导致误过滤掉全部结果）。
    pub fn is_latest(&self) -> bool {
        self.meta
            .as_ref()
            .and_then(|m| m.official.as_ref())
            .map(|o| o.is_latest)
            .unwrap_or(true)
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct RegistryEntryMeta {
    #[serde(default, rename = "io.modelcontextprotocol.registry/official")]
    pub official: Option<RegistryOfficialMeta>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryOfficialMeta {
    #[serde(default)]
    pub is_latest: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryMetadata {
    #[serde(default)]
    pub count: u32,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryServerSummary {
    /// 反向 DNS 名，如 io.modelcontextprotocol/postgres
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub packages: Vec<RegistryPackage>,
    #[serde(default)]
    pub remotes: Vec<RegistryRemote>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryPackage {
    /// npm | pypi | oci | nuget | cargo | mcpb
    pub registry_type: String,
    pub identifier: String,
    /// mcpb 等按 URL+sha256 分发的包类型没有语义化版本号，字段可能缺失。
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub registry_base_url: Option<String>,
    /// npx | uvx | dnx
    #[serde(default)]
    pub runtime_hint: Option<String>,
    /// 子字段结构未最终定型，先存 raw Value，实现联调时对照真实响应收紧
    #[serde(default)]
    pub package_arguments: Vec<serde_json::Value>,
    #[serde(default)]
    pub runtime_arguments: Vec<serde_json::Value>,
    #[serde(default)]
    pub environment_variables: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryRemote {
    #[serde(rename = "type")]
    pub remote_type: String,
    pub url: String,
}

pub struct RegistryClient {
    http: Client,
    base_url: String,
}

impl RegistryClient {
    /// `base_url` 是目标 registry 的地址——可以是官方公共实例，也可以是
    /// 自建/私有的同一开源 registry 软件的另一部署实例。
    pub fn new(base_url: impl Into<String>) -> Self {
        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
            .user_agent(concat!("wyj-code/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_default();
        Self {
            http,
            base_url: base_url.into(),
        }
    }

    /// 指向官方公共 registry 实例的便捷构造函数。
    pub fn official() -> Self {
        Self::new(REGISTRY_BASE_URL)
    }

    pub async fn list_servers(
        &self,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<RegistryListResponse> {
        self.list_servers_impl(cursor, limit, None).await
    }

    async fn list_servers_impl(
        &self,
        cursor: Option<&str>,
        limit: u32,
        search: Option<&str>,
    ) -> Result<RegistryListResponse> {
        let url = format!("{}/v0/servers", self.base_url);
        let mut query: Vec<(&str, String)> = vec![("limit", limit.to_string())];
        if let Some(cursor) = cursor {
            query.push(("cursor", cursor.to_string()));
        }
        if let Some(search) = search {
            query.push(("search", search.to_string()));
        }
        let resp = self
            .http
            .get(&url)
            .query(&query)
            .send()
            .await
            .with_context(|| format!("请求 MCP Registry 失败: {url}"))?
            .error_for_status()
            .with_context(|| format!("MCP Registry 返回错误状态: {url}"))?;
        let parsed: RegistryListResponse = resp
            .json()
            .await
            .with_context(|| format!("解析 MCP Registry 响应失败: {url}"))?;
        Ok(parsed)
    }

    /// 关键字搜索：服务端 `search` 查询参数过滤，分页拉取（封顶 [`SEARCH_PAGE_CAP`] 条）。
    pub async fn search_servers(&self, keyword: &str) -> Result<Vec<RegistryServerSummary>> {
        let mut matched = Vec::new();
        let mut cursor: Option<String> = None;
        let mut fetched = 0u32;

        loop {
            let page = self
                .list_servers_impl(cursor.as_deref(), DEFAULT_LIST_LIMIT, Some(keyword))
                .await?;
            fetched += page.servers.len() as u32;
            matched.extend(
                page.servers
                    .into_iter()
                    .filter(RegistryServerEntry::is_latest)
                    .map(|e| e.server),
            );

            match page.metadata.next_cursor {
                Some(next) if fetched < SEARCH_PAGE_CAP => cursor = Some(next),
                _ => break,
            }
        }
        Ok(matched)
    }
}

// ── MCP registry 源管理（对齐 marketplace.rs 的增删模式）────────────────────────

/// 对 base_url 取一个稳定的短 id，仅做 lockfile 主键用，无安全性要求。
fn registry_id(base_url: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    base_url.trim_end_matches('/').hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// 从 URL 派生一个展示用名字（去掉协议前缀/尾部斜杠），用户可在添加后自行改名
/// （当前版本未提供改名操作，够用即可）。
fn derive_registry_name(base_url: &str) -> String {
    base_url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string()
}

/// 官方公共 registry 的 `McpRegistrySource` 表示，供 `ensure_default_registry`
/// 内部预置使用，也供 UI 层在读盘失败时兜底构造。
pub fn official_registry_source() -> lockfile::McpRegistrySource {
    lockfile::McpRegistrySource {
        id: registry_id(REGISTRY_BASE_URL),
        name: "Official MCP Registry".to_string(),
        base_url: REGISTRY_BASE_URL.to_string(),
        added_at: chrono::Utc::now(),
    }
}

/// 首次使用（`mcp_registries` 为空）时预置官方源并持久化；若列表已有内容
/// （包括用户曾显式删光后留下的空列表——但那种情况下这个函数会重新预置官方源，
/// 见下方说明）则原样返回。
///
/// 注意：这里无法区分"从未初始化过"和"用户手动删光了所有源"，两者在磁盘上
/// 都表现为空列表，因此都会重新预置官方源。这是刻意的取舍——比起"用户不小心
/// 删光后 /mcp 直接失去所有可用源、无从恢复"，重新预置一个可删除的默认源
/// 更安全。
pub fn ensure_default_registry() -> Result<Vec<lockfile::McpRegistrySource>> {
    let mut manifest = lockfile::load_global()?;
    if manifest.mcp_registries.is_empty() {
        manifest.mcp_registries.push(official_registry_source());
        lockfile::save_global(&manifest)?;
    }
    Ok(manifest.mcp_registries)
}

/// 添加一个 registry 源（只写全局 lockfile，已存在同 id 直接返回已有项）。
pub fn add_registry(base_url: &str) -> Result<lockfile::McpRegistrySource> {
    let mut manifest = lockfile::load_global()?;
    let base_url = base_url.trim_end_matches('/');
    let id = registry_id(base_url);
    if let Some(existing) = manifest.mcp_registries.iter().find(|r| r.id == id) {
        return Ok(existing.clone());
    }
    let source = lockfile::McpRegistrySource {
        id,
        name: derive_registry_name(base_url),
        base_url: base_url.to_string(),
        added_at: chrono::Utc::now(),
    };
    manifest.mcp_registries.push(source.clone());
    lockfile::save_global(&manifest)?;
    Ok(source)
}

/// 移除一个 registry 源记录（不影响已安装的 MCP server——它们的 `registry_url`
/// 已经各自记录在 lockfile 条目里，卸载源不会破坏已装条目的升级能力）。
pub fn remove_registry(id: &str) -> Result<()> {
    let mut manifest = lockfile::load_global()?;
    manifest.mcp_registries.retain(|r| r.id != id);
    lockfile::save_global(&manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 真实响应形状（已对照 registry.modelcontextprotocol.io 的实际返回核实）：
    // servers[] 的每个元素是 `{"server": {...}, "_meta": {...}}` 的包装对象，
    // 不是把 name/description/packages 直接摊平在顶层。
    const SAMPLE_RESPONSE: &str = r#"
    {
      "servers": [
        {
          "server": {
            "name": "io.modelcontextprotocol/postgres",
            "description": "PostgreSQL MCP server",
            "version": "1.2.3",
            "packages": [
              {
                "registryType": "npm",
                "identifier": "@modelcontextprotocol/server-postgres",
                "version": "1.2.3",
                "registryBaseUrl": "https://registry.npmjs.org",
                "runtimeHint": "npx",
                "packageArguments": [],
                "runtimeArguments": [],
                "environmentVariables": []
              }
            ],
            "remotes": []
          },
          "_meta": { "io.modelcontextprotocol.registry/official": { "isLatest": true } }
        },
        {
          "server": {
            "name": "io.example/remote-only",
            "description": "Remote-only server",
            "version": "0.1.0",
            "packages": [],
            "remotes": [{"type": "streamable-http", "url": "https://example.com/mcp"}]
          },
          "_meta": { "io.modelcontextprotocol.registry/official": { "isLatest": true } }
        }
      ],
      "metadata": { "count": 2, "nextCursor": null }
    }
    "#;

    #[test]
    fn deserializes_list_response() {
        let parsed: RegistryListResponse = serde_json::from_str(SAMPLE_RESPONSE).unwrap();
        assert_eq!(parsed.servers.len(), 2);
        assert_eq!(parsed.metadata.count, 2);
        assert!(parsed.metadata.next_cursor.is_none());

        let pg = &parsed.servers[0].server;
        assert_eq!(pg.name, "io.modelcontextprotocol/postgres");
        assert_eq!(pg.packages.len(), 1);
        assert_eq!(pg.packages[0].registry_type, "npm");
        assert_eq!(pg.packages[0].runtime_hint.as_deref(), Some("npx"));

        let remote_only = &parsed.servers[1].server;
        assert!(remote_only.packages.is_empty());
        assert_eq!(remote_only.remotes.len(), 1);

        assert!(parsed.servers[0].is_latest());
        assert!(parsed.servers[1].is_latest());
    }

    // 真实场景：registry 对同一个 server 名字返回全部历史版本，只有一条
    // isLatest=true，其余是旧版本快照——过滤逻辑必须只保留最新的一条，
    // 否则会像 io.github.upstash/context7 那样把旧版本（1.0.25）误当成
    // 可安装候选，而不是真正的最新版（1.0.31）。
    const VERSION_HISTORY_RESPONSE: &str = r#"
    {
      "servers": [
        {
          "server": {
            "name": "io.github.upstash/context7",
            "description": "Up-to-date code docs for any prompt",
            "version": "1.0.0",
            "packages": [
              {"registryType": "npm", "identifier": "@upstash/context7-mcp", "version": "1.0.25"}
            ]
          },
          "_meta": { "io.modelcontextprotocol.registry/official": { "isLatest": false } }
        },
        {
          "server": {
            "name": "io.github.upstash/context7",
            "description": "Up-to-date code docs for any prompt",
            "version": "1.0.30",
            "packages": [
              {"registryType": "npm", "identifier": "@upstash/context7-mcp", "version": "1.0.30"}
            ]
          },
          "_meta": { "io.modelcontextprotocol.registry/official": { "isLatest": false } }
        },
        {
          "server": {
            "name": "io.github.upstash/context7",
            "description": "Up-to-date code docs for any prompt",
            "version": "1.0.31",
            "packages": [
              {"registryType": "npm", "identifier": "@upstash/context7-mcp", "version": "1.0.31"}
            ]
          },
          "_meta": { "io.modelcontextprotocol.registry/official": { "isLatest": true } }
        }
      ],
      "metadata": { "count": 3, "nextCursor": null }
    }
    "#;

    #[test]
    fn is_latest_filters_out_historical_versions() {
        let parsed: RegistryListResponse = serde_json::from_str(VERSION_HISTORY_RESPONSE).unwrap();
        let latest_only: Vec<_> = parsed
            .servers
            .iter()
            .filter(|e| e.is_latest())
            .map(|e| e.server.packages[0].version.clone())
            .collect();
        assert_eq!(latest_only, vec!["1.0.31"]);
    }

    #[test]
    fn is_latest_defaults_true_when_meta_missing() {
        let entry: RegistryServerEntry =
            serde_json::from_str(r#"{"server": {"name": "no.meta/server", "packages": []}}"#)
                .unwrap();
        assert!(entry.is_latest());
    }

    #[test]
    fn registry_id_is_deterministic_and_ignores_trailing_slash() {
        let a = registry_id("https://registry.example.com");
        let b = registry_id("https://registry.example.com/");
        let c = registry_id("https://other.example.com");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn derive_registry_name_strips_protocol_and_trailing_slash() {
        assert_eq!(
            derive_registry_name("https://registry.modelcontextprotocol.io/"),
            "registry.modelcontextprotocol.io"
        );
        assert_eq!(
            derive_registry_name("http://internal.example.com"),
            "internal.example.com"
        );
    }
}
