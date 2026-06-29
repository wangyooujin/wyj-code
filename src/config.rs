//! 配置数据模型与 TOML schema。
//!
//! 顶层 `~/.wyj-code/profiles.toml`:
//! - `default_profile`: 默认 profile 名
//! - `claude_path`: 可选,claude 二进制路径(缺省走 $PATH)
//! - `[defaults]` / `[defaults.env]`: 全局默认开关与字段
//! - `[[profiles]]`: profile 数组,每个含具名字段与 `[profiles.env]` 覆盖表

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// 环境变量表。值统一为字符串;加载时把 TOML 的 int/bool 强制 stringify,
/// 这样 `API_TIMEOUT_MS = 3000000` 也能正确读入,避免类型错。
pub type EnvMap = BTreeMap<String, String>;

/// 把 TOML 任意标量值转成字符串。bool 转 "1"/"0"(与现有 alias 惯例一致)。
fn value_to_string(v: toml::Value) -> String {
    match v {
        toml::Value::String(s) => s,
        toml::Value::Integer(i) => i.to_string(),
        toml::Value::Float(f) => f.to_string(),
        toml::Value::Boolean(true) => "1".to_string(),
        toml::Value::Boolean(false) => "0".to_string(),
        other => other.to_string(),
    }
}

/// 自定义反序列化:接受 string/int/bool 等标量,统一转 String。
pub fn deserialize_env_map<'de, D>(deserializer: D) -> Result<EnvMap, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw: BTreeMap<String, toml::Value> = BTreeMap::deserialize(deserializer)?;
    raw.into_iter().map(|(k, v)| Ok((k, value_to_string(v)))).collect()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_path: Option<String>,
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub profiles: Vec<Profile>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Defaults {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub small_fast_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_context_tokens: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_thinking_tokens: Option<String>,
    #[serde(default, deserialize_with = "deserialize_env_map", skip_serializing_if = "EnvMap::is_empty")]
    pub env: EnvMap,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub small_fast_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub haiku_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sonnet_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opus_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_context_tokens: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_thinking_tokens: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<String>,
    /// 为 true 时,launch/env 从 macOS Keychain 读取 AUTH_TOKEN(而非用明文 auth_token)。
    #[serde(default, skip_serializing_if = "is_false")]
    pub keychain: bool,
    #[serde(default, deserialize_with = "deserialize_env_map", skip_serializing_if = "EnvMap::is_empty")]
    pub env: EnvMap,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl Profile {
    /// 根据 env key 名设置具名字段(命中则改字段,返回 true);否则返回 false。
    pub fn set_named_field(&mut self, key: &str, value: String) -> bool {
        match key {
            "ANTHROPIC_BASE_URL" => self.base_url = Some(value),
            "ANTHROPIC_AUTH_TOKEN" => self.auth_token = Some(value),
            "ANTHROPIC_API_KEY" => self.api_key = Some(value),
            "ANTHROPIC_MODEL" => self.model = Some(value),
            "ANTHROPIC_SMALL_FAST_MODEL" => self.small_fast_model = Some(value),
            "ANTHROPIC_DEFAULT_HAIKU_MODEL" => self.haiku_model = Some(value),
            "ANTHROPIC_DEFAULT_SONNET_MODEL" => self.sonnet_model = Some(value),
            "ANTHROPIC_DEFAULT_OPUS_MODEL" => self.opus_model = Some(value),
            "CLAUDE_CODE_MAX_CONTEXT_TOKENS" => self.max_context_tokens = Some(value),
            "CLAUDE_CODE_MAX_OUTPUT_TOKENS" => self.max_output_tokens = Some(value),
            "MAX_THINKING_TOKENS" => self.max_thinking_tokens = Some(value),
            "API_TIMEOUT_MS" => self.timeout_ms = Some(value),
            _ => return false,
        }
        true
    }

    /// 删除具名字段(命中返回 true)。
    pub fn unset_named_field(&mut self, key: &str) -> bool {
        let field = match key {
            "ANTHROPIC_BASE_URL" => &mut self.base_url,
            "ANTHROPIC_AUTH_TOKEN" => &mut self.auth_token,
            "ANTHROPIC_API_KEY" => &mut self.api_key,
            "ANTHROPIC_MODEL" => &mut self.model,
            "ANTHROPIC_SMALL_FAST_MODEL" => &mut self.small_fast_model,
            "ANTHROPIC_DEFAULT_HAIKU_MODEL" => &mut self.haiku_model,
            "ANTHROPIC_DEFAULT_SONNET_MODEL" => &mut self.sonnet_model,
            "ANTHROPIC_DEFAULT_OPUS_MODEL" => &mut self.opus_model,
            "CLAUDE_CODE_MAX_CONTEXT_TOKENS" => &mut self.max_context_tokens,
            "CLAUDE_CODE_MAX_OUTPUT_TOKENS" => &mut self.max_output_tokens,
            "MAX_THINKING_TOKENS" => &mut self.max_thinking_tokens,
            "API_TIMEOUT_MS" => &mut self.timeout_ms,
            _ => return false,
        };
        if field.is_none() {
            return false;
        }
        *field = None;
        true
    }
}

impl Config {
    /// 按名查找 profile(返回首个匹配;若 TOML 内有重名,加载时另行告警)。
    pub fn get_profile(&self, name: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.name == name)
    }

    pub fn get_profile_mut(&mut self, name: &str) -> Option<&mut Profile> {
        self.profiles.iter_mut().find(|p| p.name == name)
    }
}
