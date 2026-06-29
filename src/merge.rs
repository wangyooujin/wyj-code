//! 三层合并:defaults.env + defaults 具名字段 → profile 具名字段 → profile.env。
//! 后者覆盖前者。输出最终注入 claude 的 env 表。

use crate::config::{Config, EnvMap, Profile};

/// 把具名字段投影到 env key(仅当字段为 Some)。
fn insert(map: &mut EnvMap, key: &str, val: &Option<String>) {
    if let Some(v) = val {
        map.insert(key.to_string(), v.clone());
    }
}

fn project_defaults(map: &mut EnvMap, d: &crate::config::Defaults) {
    insert(map, "ANTHROPIC_MODEL", &d.model);
    insert(map, "ANTHROPIC_SMALL_FAST_MODEL", &d.small_fast_model);
    insert(map, "CLAUDE_CODE_MAX_CONTEXT_TOKENS", &d.max_context_tokens);
    insert(map, "CLAUDE_CODE_MAX_OUTPUT_TOKENS", &d.max_output_tokens);
    insert(map, "MAX_THINKING_TOKENS", &d.max_thinking_tokens);
    insert(map, "API_TIMEOUT_MS", &d.timeout_ms);
}

fn project_profile(map: &mut EnvMap, p: &Profile) {
    insert(map, "ANTHROPIC_BASE_URL", &p.base_url);
    insert(map, "ANTHROPIC_AUTH_TOKEN", &p.auth_token);
    insert(map, "ANTHROPIC_API_KEY", &p.api_key);
    insert(map, "ANTHROPIC_MODEL", &p.model);
    insert(map, "ANTHROPIC_SMALL_FAST_MODEL", &p.small_fast_model);
    insert(map, "ANTHROPIC_DEFAULT_HAIKU_MODEL", &p.haiku_model);
    insert(map, "ANTHROPIC_DEFAULT_SONNET_MODEL", &p.sonnet_model);
    insert(map, "ANTHROPIC_DEFAULT_OPUS_MODEL", &p.opus_model);
    insert(map, "CLAUDE_CODE_MAX_CONTEXT_TOKENS", &p.max_context_tokens);
    insert(map, "CLAUDE_CODE_MAX_OUTPUT_TOKENS", &p.max_output_tokens);
    insert(map, "MAX_THINKING_TOKENS", &p.max_thinking_tokens);
    insert(map, "API_TIMEOUT_MS", &p.timeout_ms);
}

/// 合并出 profile 的最终 env 表。
pub fn merge_env(config: &Config, profile: &Profile) -> EnvMap {
    let mut map = EnvMap::new();
    // 层 1: defaults.env
    for (k, v) in &config.defaults.env {
        map.insert(k.clone(), v.clone());
    }
    // 层 2: defaults 具名字段
    project_defaults(&mut map, &config.defaults);
    // 层 3: profile 具名字段
    project_profile(&mut map, profile);
    // 层 4: profile.env(最高优先级)
    for (k, v) in &profile.env {
        map.insert(k.clone(), v.clone());
    }
    map
}

/// 把 env 表格式化为 `export KEY='VALUE'` 语句,值按 POSIX 单引号转义。
pub fn format_exports(map: &EnvMap) -> String {
    map.iter()
        .map(|(k, v)| format!("export {}='{}'", k, shell_quote_single(v)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// POSIX 单引号转义:把内部的 `'` 替换为 `'\''`。
pub fn shell_quote_single(s: &str) -> String {
    s.replace('\'', "'\\''")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_layers_with_profile_env_on_top() {
        let mut config = Config::default();
        config.defaults.env.insert("API_TIMEOUT_MS".into(), "3000000".into());
        config.defaults.env.insert("DISABLE_TELEMETRY".into(), "1".into());

        let mut p = Profile {
            name: "x".into(),
            base_url: Some("https://e".into()),
            auth_token: Some("t".into()),
            model: Some("m".into()),
            ..Default::default()
        };
        p.env.insert("API_TIMEOUT_MS".into(), "9999".into());
        p.env.insert("CLAUDE_CODE_MAX_CONTEXT_TOKENS".into(), "100000".into());

        let m = merge_env(&config, &p);
        assert_eq!(m.get("API_TIMEOUT_MS").unwrap(), "9999"); // profile.env 覆盖 defaults
        assert_eq!(m.get("DISABLE_TELEMETRY").unwrap(), "1"); // 继承 defaults
        assert_eq!(m.get("ANTHROPIC_BASE_URL").unwrap(), "https://e");
        assert_eq!(m.get("ANTHROPIC_MODEL").unwrap(), "m");
        assert_eq!(m.get("CLAUDE_CODE_MAX_CONTEXT_TOKENS").unwrap(), "100000");
    }

    #[test]
    fn quote_single_quote() {
        assert_eq!(shell_quote_single("a'b"), "a'\\''b");
        assert_eq!(format!("export X='{}'", shell_quote_single("a'b")).contains("'\\''"), true);
    }
}
