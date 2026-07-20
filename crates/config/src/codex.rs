//! Codex（OpenAI Codex CLI）配置兼容层：只读解析 `~/.codex/config.toml` 的
//! `[mcp_servers.<name>]` 表，供 `/import` 一键导入。
//!
//! 与 `project_mcp::load_native_mcp`（Claude 的 JSON `mcpServers`）同一哲学：
//! 绝不改写来源文件，wyj-code 只把解析结果物化进自己的 TOML 配置。解析走
//! `toml::Value` 手工取字段，Codex 侧的未知字段（`startup_timeout_sec`、
//! `tool_timeout_sec`、`enabled` 等，随其版本演进）天然被忽略。

use crate::{McpServerConfig, McpTransport};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use toml::Value;

/// 返回 Codex 配置目录路径（`~/.codex`），仅解析路径、不创建。
pub fn codex_home_dir() -> Result<PathBuf> {
    Ok(crate::home_dir()?.join(".codex"))
}

/// 解析 Codex `config.toml` 的 `[mcp_servers.<name>]` 表为 MCP server 列表；
/// 文件不存在返回空列表，非法 TOML 报错。`url` 字段存在则判定为
/// Streamable HTTP，否则为 stdio。
pub fn load_codex_mcp(path: &Path) -> Result<Vec<McpServerConfig>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("读取 Codex 配置失败: {}", path.display()))?;
    let value: Value = toml::from_str(&content)
        .with_context(|| format!("解析 Codex 配置失败: {}", path.display()))?;
    let Some(servers) = value.get("mcp_servers").and_then(Value::as_table) else {
        return Ok(Vec::new());
    };
    let mut result = Vec::new();
    for (name, raw) in servers {
        if !raw.is_table() {
            continue;
        }
        let url = raw.get("url").and_then(Value::as_str).map(str::to_string);
        let transport = if url.is_some() {
            McpTransport::StreamableHttp
        } else {
            McpTransport::Stdio
        };
        let args = raw
            .get("args")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        result.push(McpServerConfig {
            name: name.clone(),
            transport,
            command: raw
                .get("command")
                .and_then(Value::as_str)
                .map(str::to_string),
            args,
            env: toml_string_map(raw.get("env")),
            url,
            headers: toml_string_map(raw.get("headers")),
        });
    }
    Ok(result)
}

fn toml_string_map(value: Option<&Value>) -> HashMap<String, String> {
    value
        .and_then(Value::as_table)
        .map(|table| {
            table
                .iter()
                .filter_map(|(key, value)| {
                    value.as_str().map(|value| (key.clone(), value.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let servers = load_codex_mcp(&dir.path().join("config.toml")).unwrap();
        assert!(servers.is_empty());
    }

    #[test]
    fn no_mcp_servers_section_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "model = \"o4-mini\"\n").unwrap();
        assert!(load_codex_mcp(&path).unwrap().is_empty());
    }

    #[test]
    fn parses_stdio_server_ignoring_unknown_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[mcp_servers.context7]
command = "npx"
args = ["-y", "@upstash/context7-mcp"]
startup_timeout_sec = 20
tool_timeout_sec = 60

[mcp_servers.context7.env]
API_KEY = "secret"
"#,
        )
        .unwrap();
        let servers = load_codex_mcp(&path).unwrap();
        assert_eq!(servers.len(), 1);
        let s = &servers[0];
        assert_eq!(s.name, "context7");
        assert_eq!(s.transport, McpTransport::Stdio);
        assert_eq!(s.command.as_deref(), Some("npx"));
        assert_eq!(s.args, vec!["-y", "@upstash/context7-mcp"]);
        assert_eq!(s.env.get("API_KEY").map(String::as_str), Some("secret"));
    }

    #[test]
    fn url_field_means_streamable_http() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[mcp_servers.remote]
url = "https://example.com/mcp"

[mcp_servers.remote.headers]
Authorization = "Bearer ${TOKEN}"
"#,
        )
        .unwrap();
        let servers = load_codex_mcp(&path).unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].transport, McpTransport::StreamableHttp);
        assert_eq!(servers[0].url.as_deref(), Some("https://example.com/mcp"));
        assert_eq!(
            servers[0].headers.get("Authorization").map(String::as_str),
            Some("Bearer ${TOKEN}")
        );
    }

    #[test]
    fn invalid_toml_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "not [ valid toml").unwrap();
        assert!(load_codex_mcp(&path).is_err());
    }
}
