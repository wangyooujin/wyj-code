//! wyj-code 配置模块
//! 管理 ~/.wyj-code/ 目录下的配置文件与 API Key 读取。

use anyhow::{Context, Result};
use directories::UserDirs;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 支持的 LLM 供应商格式
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    #[default]
    Anthropic,
    OpenAI,
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Provider::Anthropic => write!(f, "anthropic"),
            Provider::OpenAI => write!(f, "openai"),
        }
    }
}

/// 主配置结构，对应 ~/.wyj-code/config.toml
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// LLM 供应商格式（anthropic 或 openai）
    pub provider: Provider,
    /// 模型名称
    pub model: String,
    /// API 端点（留空使用供应商默认值）
    pub base_url: String,
    /// API Key（优先从环境变量 WYJ_CODE_API_KEY 读取）
    pub api_key: Option<String>,
    /// 最大 token 预算（每轮）
    pub max_tokens: u32,
    /// 日志级别
    pub log_level: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            provider: Provider::Anthropic,
            model: "claude-opus-4-8".to_string(),
            base_url: String::new(),
            api_key: None,
            max_tokens: 8192,
            log_level: "warn".to_string(),
        }
    }
}

impl Config {
    /// 加载配置：先读文件，再用环境变量覆盖 api_key。
    pub fn load() -> Result<Self> {
        let config_path = config_file_path()?;
        let mut cfg: Config = if config_path.exists() {
            let content = std::fs::read_to_string(&config_path)
                .with_context(|| format!("读取配置文件失败: {}", config_path.display()))?;
            toml::from_str(&content)
                .with_context(|| format!("解析配置文件失败: {}", config_path.display()))?
        } else {
            Config::default()
        };

        // 环境变量优先
        if let Ok(key) = std::env::var("WYJ_CODE_API_KEY") {
            if !key.is_empty() {
                cfg.api_key = Some(key);
            }
        }

        Ok(cfg)
    }

    /// 返回有效的 API Key，若无则报错。
    pub fn api_key(&self) -> Result<&str> {
        self.api_key
            .as_deref()
            .filter(|k| !k.is_empty())
            .ok_or_else(|| anyhow::anyhow!(
                "未找到 API Key。请设置环境变量 WYJ_CODE_API_KEY 或在配置文件中设置 api_key。"
            ))
    }

    /// 返回供应商的默认 base_url（若配置为空时使用）。
    pub fn resolved_base_url(&self) -> &str {
        if !self.base_url.is_empty() {
            &self.base_url
        } else {
            match self.provider {
                Provider::Anthropic => "https://api.anthropic.com",
                Provider::OpenAI => "https://api.openai.com",
            }
        }
    }

    /// 将当前配置写入文件。
    pub fn save(&self) -> Result<()> {
        let config_path = config_file_path()?;
        let content = toml::to_string_pretty(self).context("序列化配置失败")?;
        std::fs::write(&config_path, content)
            .with_context(|| format!("写入配置文件失败: {}", config_path.display()))
    }
}

/// 返回配置目录路径（~/.wyj-code），若不存在则创建。
pub fn config_dir() -> Result<PathBuf> {
    let user_dirs = UserDirs::new().ok_or_else(|| anyhow::anyhow!("无法获取用户主目录"))?;
    let dir = user_dirs.home_dir().join(".wyj-code");
    if !dir.exists() {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("创建配置目录失败: {}", dir.display()))?;
        tracing::info!("初始化配置目录: {}", dir.display());
    }
    Ok(dir)
}

/// 返回主配置文件路径（~/.wyj-code/config.toml）。
pub fn config_file_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}
