//! Claude Code 插件清单格式解析：`.claude-plugin/plugin.json` + `.claude-plugin/marketplace.json`
//!
//! 尽量贴近官方 schema（`https://www.schemastore.org/claude-code-plugin-manifest.json`
//! 与 `claude-code-marketplace.json`），使真实存在的社区插件仓库能被直接解析浏览。
//! commands/agents/skills/mcpServers 与 runtime contributions 都会进入统一安装记录；
//! hooks/lspServers/themes/outputStyles/monitors/channels/settings/userConfig 由
//! `plugin_runtime` 在启动时事务式激活。单个插件任一 runtime contribution 无效时，
//! 该插件的全部 runtime contribution 都不会部分生效，并给出可诊断 warning。

use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashMap;

fn default_true() -> bool {
    true
}

fn is_present(v: &Option<serde_json::Value>) -> bool {
    match v {
        None => false,
        Some(serde_json::Value::Null) => false,
        Some(serde_json::Value::Array(a)) => !a.is_empty(),
        Some(serde_json::Value::Object(o)) => !o.is_empty(),
        Some(serde_json::Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

// ─── author ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginAuthor {
    pub name: String,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

// ─── 多形态字段：commands/agents/skills 支持字符串 / 数组 / (仅 commands) 对象 ──

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PluginPathListField {
    Single(String),
    Multiple(Vec<String>),
}

impl PluginPathListField {
    pub fn into_paths(self) -> Vec<String> {
        match self {
            PluginPathListField::Single(s) => vec![s],
            PluginPathListField::Multiple(v) => v,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PluginCommandsField {
    Single(String),
    Multiple(Vec<String>),
    Map(HashMap<String, PluginCommandEntry>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginCommandEntry {
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub argument_hint: Option<String>,
    /// 只解析不使用：wyj-code 的 skill 命令没有 per-command 模型覆盖概念。
    #[serde(default)]
    pub model: Option<String>,
    /// 只解析不使用：wyj-code 的 skill 命令没有工具白名单机制。
    #[serde(default)]
    pub allowed_tools: Option<Vec<String>>,
}

// ─── mcpServers ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PluginMcpServersField {
    /// 指向 `.mcp.json` 文件的路径
    Path(String),
    Map(HashMap<String, PluginMcpServerDef>),
}

/// Stdio 和 Streamable HTTP 会进入运行时；SSE/WS 仍保留为可诊断的未支持能力。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum PluginMcpServerDef {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: HashMap<String, String>,
    },
    Sse {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
        #[serde(default)]
        oauth: Option<serde_json::Value>,
    },
    Http {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
        #[serde(default)]
        oauth: Option<serde_json::Value>,
    },
    Ws {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
    },
}

impl PluginMcpServerDef {
    pub fn transport_label(&self) -> &'static str {
        match self {
            PluginMcpServerDef::Stdio { .. } => "stdio",
            PluginMcpServerDef::Sse { .. } => "sse",
            PluginMcpServerDef::Http { .. } => "http",
            PluginMcpServerDef::Ws { .. } => "ws",
        }
    }
}

// ─── dependencies ────────────────────────────────────────────────────────────

/// `"name"` | `"name@marketplace"` | `"name@marketplace@^version"`，或对象形式。
/// v1 仅用于安装前"检查依赖是否已安装，缺失则告警"，不做自动递归安装。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PluginDependency {
    Simple(String),
    Detailed {
        name: String,
        #[serde(default)]
        marketplace: Option<String>,
    },
}

impl PluginDependency {
    /// 解析出 (name, marketplace, version_req)。
    pub fn parse(&self) -> (String, Option<String>, Option<String>) {
        match self {
            PluginDependency::Detailed { name, marketplace } => {
                (name.clone(), marketplace.clone(), None)
            }
            PluginDependency::Simple(s) => {
                let mut parts = s.splitn(3, '@');
                let name = parts.next().unwrap_or_default().to_string();
                let marketplace = parts.next().map(|s| s.to_string());
                let version_req = parts.next().map(|s| s.to_string());
                (name, marketplace, version_req)
            }
        }
    }
}

// ─── source ──────────────────────────────────────────────────────────────────

/// `source` 字段：手写 `Deserialize` 是为了对未支持的类型（如 `npm`）给出清晰的
/// "该 source 类型暂不支持"报错，而不是泛用 untagged 枚举的模糊报错。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginSource {
    /// 字符串形态：相对 marketplace 仓库根（或 `metadata.pluginRoot`）的路径。
    LocalPath(String),
    Github {
        repo: String,
        git_ref: Option<String>,
        sha: Option<String>,
    },
    GitUrl {
        url: String,
        git_ref: Option<String>,
        sha: Option<String>,
    },
    GitSubdir {
        url: String,
        path: String,
        git_ref: Option<String>,
        sha: Option<String>,
    },
    /// 能解析出内容，但 wyj-code v1 不支持从 npm 包安装插件。
    NpmUnsupported {
        package: String,
        version: Option<String>,
        registry: Option<String>,
    },
}

impl Serialize for PluginSource {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;
        match self {
            PluginSource::LocalPath(s) => serializer.serialize_str(s),
            PluginSource::Github { repo, git_ref, sha } => {
                let mut map = serializer.serialize_map(None)?;
                map.serialize_entry("source", "github")?;
                map.serialize_entry("repo", repo)?;
                if let Some(r) = git_ref {
                    map.serialize_entry("ref", r)?;
                }
                if let Some(s) = sha {
                    map.serialize_entry("sha", s)?;
                }
                map.end()
            }
            PluginSource::GitUrl { url, git_ref, sha } => {
                let mut map = serializer.serialize_map(None)?;
                map.serialize_entry("source", "url")?;
                map.serialize_entry("url", url)?;
                if let Some(r) = git_ref {
                    map.serialize_entry("ref", r)?;
                }
                if let Some(s) = sha {
                    map.serialize_entry("sha", s)?;
                }
                map.end()
            }
            PluginSource::GitSubdir {
                url,
                path,
                git_ref,
                sha,
            } => {
                let mut map = serializer.serialize_map(None)?;
                map.serialize_entry("source", "git-subdir")?;
                map.serialize_entry("url", url)?;
                map.serialize_entry("path", path)?;
                if let Some(r) = git_ref {
                    map.serialize_entry("ref", r)?;
                }
                if let Some(s) = sha {
                    map.serialize_entry("sha", s)?;
                }
                map.end()
            }
            PluginSource::NpmUnsupported {
                package,
                version,
                registry,
            } => {
                let mut map = serializer.serialize_map(None)?;
                map.serialize_entry("source", "npm")?;
                map.serialize_entry("package", package)?;
                if let Some(v) = version {
                    map.serialize_entry("version", v)?;
                }
                if let Some(r) = registry {
                    map.serialize_entry("registry", r)?;
                }
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for PluginSource {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        match &value {
            serde_json::Value::String(s) => Ok(PluginSource::LocalPath(s.clone())),
            serde_json::Value::Object(map) => {
                let get_str = |key: &str| -> Option<String> {
                    map.get(key).and_then(|v| v.as_str()).map(|s| s.to_string())
                };
                match map.get("source").and_then(|v| v.as_str()) {
                    Some("github") => {
                        let repo = get_str("repo").ok_or_else(|| DeError::missing_field("repo"))?;
                        Ok(PluginSource::Github {
                            repo,
                            git_ref: get_str("ref"),
                            sha: get_str("sha"),
                        })
                    }
                    Some("url") => {
                        let url = get_str("url").ok_or_else(|| DeError::missing_field("url"))?;
                        Ok(PluginSource::GitUrl {
                            url,
                            git_ref: get_str("ref"),
                            sha: get_str("sha"),
                        })
                    }
                    Some("git-subdir") => {
                        let url = get_str("url").ok_or_else(|| DeError::missing_field("url"))?;
                        let path =
                            get_str("path").ok_or_else(|| DeError::missing_field("path"))?;
                        Ok(PluginSource::GitSubdir {
                            url,
                            path,
                            git_ref: get_str("ref"),
                            sha: get_str("sha"),
                        })
                    }
                    Some("npm") => {
                        let package =
                            get_str("package").ok_or_else(|| DeError::missing_field("package"))?;
                        Ok(PluginSource::NpmUnsupported {
                            package,
                            version: get_str("version"),
                            registry: get_str("registry"),
                        })
                    }
                    other => Err(DeError::custom(format!(
                        "未知或缺失的 source 类型: {other:?}（支持 github/url/git-subdir/npm 或本地相对路径字符串）"
                    ))),
                }
            }
            _ => Err(DeError::custom("source 字段必须是字符串或对象")),
        }
    }
}

// ─── plugin.json ─────────────────────────────────────────────────────────────

/// 独立插件仓库根目录的 `.claude-plugin/plugin.json`（`name` 必需）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginManifest {
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub author: Option<PluginAuthor>,
    #[serde(default)]
    pub homepage: Option<String>,
    /// 只解析不使用（形态不定：字符串或对象）。
    #[serde(default)]
    pub repository: Option<serde_json::Value>,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub dependencies: Vec<PluginDependency>,

    // ↓↓↓ 实际驱动安装逻辑的四类字段 ↓↓↓
    #[serde(default)]
    pub commands: Option<PluginCommandsField>,
    #[serde(default)]
    pub agents: Option<PluginPathListField>,
    #[serde(default)]
    pub skills: Option<PluginPathListField>,
    #[serde(default)]
    pub mcp_servers: Option<PluginMcpServersField>,

    // ↓↓↓ 能解析通过，但 v1 不实现功能 ↓↓↓
    #[serde(default)]
    pub hooks: Option<serde_json::Value>,
    #[serde(default)]
    pub output_styles: Option<serde_json::Value>,
    #[serde(default)]
    pub themes: Option<serde_json::Value>,
    #[serde(default)]
    pub channels: Option<serde_json::Value>,
    #[serde(default)]
    pub lsp_servers: Option<serde_json::Value>,
    #[serde(default)]
    pub monitors: Option<serde_json::Value>,
    #[serde(default)]
    pub settings: Option<serde_json::Value>,
    #[serde(default)]
    pub user_config: Option<serde_json::Value>,
}

impl PluginManifest {
    /// Runtime capability families declared by this plugin.
    pub fn runtime_capability_names(&self) -> Vec<&'static str> {
        let mut caps = Vec::new();
        if is_present(&self.hooks) {
            caps.push("hooks");
        }
        if is_present(&self.output_styles) {
            caps.push("outputStyles");
        }
        if is_present(&self.themes) {
            caps.push("themes");
        }
        if is_present(&self.channels) {
            caps.push("channels");
        }
        if is_present(&self.lsp_servers) {
            caps.push("lspServers");
        }
        if is_present(&self.monitors) {
            caps.push("monitors");
        }
        if is_present(&self.settings) {
            caps.push("settings");
        }
        if is_present(&self.user_config) {
            caps.push("userConfig");
        }
        caps
    }

    /// Kept for source compatibility with older callers. All currently parsed runtime
    /// capability families are implemented; transport/path failures are reported while resolving
    /// contributions instead of being classified as globally unsupported.
    pub fn unsupported_capability_names(&self) -> Vec<&'static str> {
        Vec::new()
    }
}

/// 与 [`PluginManifest`] 字段一致，但 `name` 可省略——用于 `marketplace.json` 里
/// `plugins[]` 条目对插件自身 `plugin.json` 的内联覆盖场景（真正的 name 通常来自
/// 插件自己仓库根的 plugin.json，marketplace 条目本身可能只给 `source`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginManifestPartial {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub author: Option<PluginAuthor>,
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default)]
    pub repository: Option<serde_json::Value>,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub dependencies: Vec<PluginDependency>,
    #[serde(default)]
    pub commands: Option<PluginCommandsField>,
    #[serde(default)]
    pub agents: Option<PluginPathListField>,
    #[serde(default)]
    pub skills: Option<PluginPathListField>,
    #[serde(default)]
    pub mcp_servers: Option<PluginMcpServersField>,
    #[serde(default)]
    pub hooks: Option<serde_json::Value>,
    #[serde(default)]
    pub output_styles: Option<serde_json::Value>,
    #[serde(default)]
    pub themes: Option<serde_json::Value>,
    #[serde(default)]
    pub channels: Option<serde_json::Value>,
    #[serde(default)]
    pub lsp_servers: Option<serde_json::Value>,
    #[serde(default)]
    pub monitors: Option<serde_json::Value>,
    #[serde(default)]
    pub settings: Option<serde_json::Value>,
    #[serde(default)]
    pub user_config: Option<serde_json::Value>,
}

// ─── marketplace.json ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginMarketplaceOwner {
    pub name: String,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginMarketplaceMetadata {
    #[serde(default)]
    pub plugin_root: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginMarketplaceEntry {
    #[serde(flatten)]
    pub manifest: PluginManifestPartial,
    pub source: PluginSource,
    /// 只解析不使用（v1 Browse tab 不做分类筛选）。
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default = "default_true")]
    pub strict: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginMarketplaceManifest {
    pub name: String,
    pub owner: PluginMarketplaceOwner,
    pub plugins: Vec<PluginMarketplaceEntry>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// 只解析不使用（v1 不做"市场移除插件自动清理"）。
    #[serde(default)]
    pub force_remove_deleted_plugins: bool,
    #[serde(default)]
    pub metadata: Option<PluginMarketplaceMetadata>,
    /// 只解析不使用。
    #[serde(default)]
    pub allow_cross_marketplace_dependencies_on: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_list_field_single_and_multiple() {
        let single: PluginPathListField =
            serde_json::from_str(r#""./agents/reviewer.md""#).unwrap();
        assert_eq!(
            single.into_paths(),
            vec!["./agents/reviewer.md".to_string()]
        );

        let multiple: PluginPathListField =
            serde_json::from_str(r#"["./a.md", "./b.md"]"#).unwrap();
        assert_eq!(
            multiple.into_paths(),
            vec!["./a.md".to_string(), "./b.md".to_string()]
        );
    }

    #[test]
    fn commands_field_three_shapes() {
        let single: PluginCommandsField =
            serde_json::from_str(r#""./commands/review.md""#).unwrap();
        assert!(matches!(single, PluginCommandsField::Single(_)));

        let multiple: PluginCommandsField =
            serde_json::from_str(r#"["./a.md", "./b.md"]"#).unwrap();
        assert!(matches!(multiple, PluginCommandsField::Multiple(_)));

        let map: PluginCommandsField = serde_json::from_str(
            r#"{"review": {"source": "./commands/review.md", "description": "Review code"}}"#,
        )
        .unwrap();
        match map {
            PluginCommandsField::Map(m) => {
                assert_eq!(m.len(), 1);
                assert_eq!(
                    m.get("review").unwrap().source.as_deref(),
                    Some("./commands/review.md")
                );
            }
            other => panic!("expected Map, got {other:?}"),
        }
    }

    #[test]
    fn mcp_servers_field_stdio_and_sse() {
        let map: PluginMcpServersField = serde_json::from_str(
            r#"{"my-server": {"type": "stdio", "command": "node", "args": ["server.js"]}}"#,
        )
        .unwrap();
        match map {
            PluginMcpServersField::Map(m) => {
                let def = m.get("my-server").unwrap();
                assert_eq!(def.transport_label(), "stdio");
                assert!(
                    matches!(def, PluginMcpServerDef::Stdio { command, .. } if command == "node")
                );
            }
            other => panic!("expected Map, got {other:?}"),
        }

        let sse: PluginMcpServersField = serde_json::from_str(
            r#"{"remote-server": {"type": "sse", "url": "https://example.com/mcp"}}"#,
        )
        .unwrap();
        match sse {
            PluginMcpServersField::Map(m) => {
                assert_eq!(m.get("remote-server").unwrap().transport_label(), "sse");
            }
            other => panic!("expected Map, got {other:?}"),
        }

        let path: PluginMcpServersField = serde_json::from_str(r#""./.mcp.json""#).unwrap();
        assert!(matches!(path, PluginMcpServersField::Path(_)));
    }

    #[test]
    fn source_all_variants_parse() {
        let local: PluginSource = serde_json::from_str(r#""./plugins/my-plugin""#).unwrap();
        assert_eq!(
            local,
            PluginSource::LocalPath("./plugins/my-plugin".to_string())
        );

        let github: PluginSource =
            serde_json::from_str(r#"{"source":"github","repo":"owner/repo"}"#).unwrap();
        assert_eq!(
            github,
            PluginSource::Github {
                repo: "owner/repo".to_string(),
                git_ref: None,
                sha: None
            }
        );

        let git_url: PluginSource = serde_json::from_str(
            r#"{"source":"url","url":"https://example.com/repo.git","ref":"main"}"#,
        )
        .unwrap();
        assert_eq!(
            git_url,
            PluginSource::GitUrl {
                url: "https://example.com/repo.git".to_string(),
                git_ref: Some("main".to_string()),
                sha: None
            }
        );

        let git_subdir: PluginSource = serde_json::from_str(
            r#"{"source":"git-subdir","url":"https://example.com/repo.git","path":"plugins/foo"}"#,
        )
        .unwrap();
        assert_eq!(
            git_subdir,
            PluginSource::GitSubdir {
                url: "https://example.com/repo.git".to_string(),
                path: "plugins/foo".to_string(),
                git_ref: None,
                sha: None
            }
        );

        let npm: PluginSource =
            serde_json::from_str(r#"{"source":"npm","package":"@scope/plugin"}"#).unwrap();
        assert_eq!(
            npm,
            PluginSource::NpmUnsupported {
                package: "@scope/plugin".to_string(),
                version: None,
                registry: None
            }
        );
    }

    #[test]
    fn source_unknown_type_errors() {
        let err =
            serde_json::from_str::<PluginSource>(r#"{"source":"gitlab","repo":"x"}"#).unwrap_err();
        assert!(err.to_string().contains("未知或缺失的 source 类型"));
    }

    #[test]
    fn source_round_trip_through_serialize() {
        let src = PluginSource::Github {
            repo: "owner/repo".to_string(),
            git_ref: Some("main".to_string()),
            sha: None,
        };
        let json = serde_json::to_string(&src).unwrap();
        let back: PluginSource = serde_json::from_str(&json).unwrap();
        assert_eq!(src, back);
    }

    #[test]
    fn dependency_simple_string_parses_name_marketplace_version() {
        let dep: PluginDependency =
            serde_json::from_str(r#""foo@bar-marketplace@^1.0.0""#).unwrap();
        let (name, marketplace, version_req) = dep.parse();
        assert_eq!(name, "foo");
        assert_eq!(marketplace.as_deref(), Some("bar-marketplace"));
        assert_eq!(version_req.as_deref(), Some("^1.0.0"));

        let dep_bare: PluginDependency = serde_json::from_str(r#""foo""#).unwrap();
        let (name, marketplace, version_req) = dep_bare.parse();
        assert_eq!(name, "foo");
        assert!(marketplace.is_none());
        assert!(version_req.is_none());
    }

    #[test]
    fn plugin_manifest_unknown_top_level_fields_do_not_error() {
        let json = r#"{
            "name": "my-plugin",
            "version": "1.0.0",
            "someFutureField": {"anything": true},
            "hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": []}]},
            "themes": ["dark"]
        }"#;
        let manifest: PluginManifest = serde_json::from_str(json).unwrap();
        assert_eq!(manifest.name, "my-plugin");
        let caps = manifest.runtime_capability_names();
        assert!(caps.contains(&"hooks"));
        assert!(caps.contains(&"themes"));
        assert!(!caps.contains(&"lspServers"));
        assert!(manifest.unsupported_capability_names().is_empty());
    }

    #[test]
    fn plugin_manifest_full_shape_parses() {
        let json = r#"{
            "name": "code-reviewer",
            "version": "1.2.0",
            "description": "A code review plugin",
            "author": {"name": "Jane Doe", "email": "jane@example.com"},
            "commands": "./commands/review.md",
            "agents": ["./agents/reviewer.md"],
            "skills": "./skills",
            "mcpServers": {"linter": {"type": "stdio", "command": "npx", "args": ["-y", "linter-mcp"]}},
            "dependencies": ["other-plugin@some-marketplace"]
        }"#;
        let manifest: PluginManifest = serde_json::from_str(json).unwrap();
        assert_eq!(manifest.name, "code-reviewer");
        assert_eq!(manifest.author.clone().unwrap().name, "Jane Doe");
        assert!(matches!(
            manifest.commands,
            Some(PluginCommandsField::Single(_))
        ));
        assert!(matches!(
            manifest.agents,
            Some(PluginPathListField::Multiple(_))
        ));
        assert!(matches!(
            manifest.skills,
            Some(PluginPathListField::Single(_))
        ));
        assert!(manifest.mcp_servers.is_some());
        assert_eq!(manifest.dependencies.len(), 1);
        assert!(manifest.unsupported_capability_names().is_empty());
    }

    #[test]
    fn marketplace_entry_flatten_overrides_and_source_required() {
        let json = r#"{
            "name": "code-reviewer",
            "description": "Marketplace-level override description",
            "source": {"source": "github", "repo": "owner/code-reviewer"},
            "category": "productivity",
            "tags": ["review", "quality"]
        }"#;
        let entry: PluginMarketplaceEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.manifest.name.as_deref(), Some("code-reviewer"));
        assert_eq!(
            entry.manifest.description.as_deref(),
            Some("Marketplace-level override description")
        );
        assert!(matches!(entry.source, PluginSource::Github { .. }));
        assert!(entry.strict); // 默认 true
        assert_eq!(
            entry.tags,
            vec!["review".to_string(), "quality".to_string()]
        );
    }

    #[test]
    fn marketplace_entry_name_optional_when_plugin_provides_own_manifest() {
        let json = r#"{"source": "./plugins/my-plugin", "strict": false}"#;
        let entry: PluginMarketplaceEntry = serde_json::from_str(json).unwrap();
        assert!(entry.manifest.name.is_none());
        assert!(!entry.strict);
        assert!(matches!(entry.source, PluginSource::LocalPath(_)));
    }

    #[test]
    fn marketplace_manifest_full_shape_parses() {
        let json = r#"{
            "name": "acme-plugins",
            "owner": {"name": "Acme Corp", "url": "https://acme.example.com"},
            "version": "1.0.0",
            "plugins": [
                {"name": "code-reviewer", "source": "./plugins/code-reviewer"},
                {"source": {"source": "url", "url": "https://example.com/other.git"}}
            ]
        }"#;
        let manifest: PluginMarketplaceManifest = serde_json::from_str(json).unwrap();
        assert_eq!(manifest.name, "acme-plugins");
        assert_eq!(manifest.owner.name, "Acme Corp");
        assert_eq!(manifest.plugins.len(), 2);
        assert_eq!(
            manifest.plugins[0].manifest.name.as_deref(),
            Some("code-reviewer")
        );
        assert!(manifest.plugins[1].manifest.name.is_none());
    }

    #[test]
    fn marketplace_manifest_missing_required_owner_errors() {
        let json = r#"{"name": "acme-plugins", "plugins": []}"#;
        assert!(serde_json::from_str::<PluginMarketplaceManifest>(json).is_err());
    }
}
