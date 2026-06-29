//! zshrc alias 块解析器:识别 `alias model_xxx='export ...'`,提取 export 语句映射成 profile。
//! 手写扫描,零 regex 依赖。健壮性:多行单引号体、混合缩进、尾随空格、值中含分号(引号感知切分)。

use crate::config::Profile;

/// 从 zshrc 文本解析所有 `alias <name>='...'`(默认仅 `model_` 前缀,除非 `prefix=None`)。
/// 返回 (alias 名, profile) 列表。
pub fn parse_zshrc(text: &str, name_filter: Option<&str>) -> Vec<(String, Profile)> {
    let mut out = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut i = 0;

    while i < n {
        // 跳过行首空白
        while i < n && (chars[i] == ' ' || chars[i] == '\t') {
            i += 1;
        }
        // 尝试匹配 `alias `
        if !matches_keyword(&chars, i, "alias") {
            // 跳到行尾
            while i < n && chars[i] != '\n' {
                i += 1;
            }
            if i < n {
                i += 1;
            }
            continue;
        }
        i += "alias".len();
        // alias 后须是空白
        if i >= n || !(chars[i] == ' ' || chars[i] == '\t') {
            while i < n && chars[i] != '\n' {
                i += 1;
            }
            if i < n {
                i += 1;
            }
            continue;
        }
        while i < n && (chars[i] == ' ' || chars[i] == '\t') {
            i += 1;
        }
        // 读取 alias 名
        let name_start = i;
        while i < n && is_ident_char(chars[i]) {
            i += 1;
        }
        let alias_name: String = chars[name_start..i].iter().collect();
        if alias_name.is_empty() {
            while i < n && chars[i] != '\n' {
                i += 1;
            }
            if i < n {
                i += 1;
            }
            continue;
        }
        // 可选空白
        while i < n && (chars[i] == ' ' || chars[i] == '\t') {
            i += 1;
        }
        // 期望 '='
        if i >= n || chars[i] != '=' {
            while i < n && chars[i] != '\n' {
                i += 1;
            }
            if i < n {
                i += 1;
            }
            continue;
        }
        i += 1;
        while i < n && (chars[i] == ' ' || chars[i] == '\t') {
            i += 1;
        }
        // 取引号体(单引号优先,也支持双引号)
        if i >= n {
            continue;
        }
        let (body, next_i) = match chars[i] {
            '\'' => read_single_quoted(&chars, i),
            '"' => read_double_quoted(&chars, i),
            _ => {
                // 无引号:取到行尾(非典型,尽力而为)
                let s = i;
                while i < n && chars[i] != '\n' {
                    i += 1;
                }
                let body: String = chars[s..i].iter().collect();
                (body, i)
            }
        };
        i = next_i;

        // 名字过滤:默认仅 model_ 前缀;若指定 name_filter 则精确匹配
        let accept = match name_filter {
            Some(f) => alias_name == *f,
            None => alias_name.starts_with("model_"),
        };
        if !accept {
            continue;
        }

        let profile_name = alias_name.strip_prefix("model_").unwrap_or(&alias_name).to_string();
        let profile = build_profile(&profile_name, &body);
        out.push((alias_name, profile));
    }

    out
}

fn matches_keyword(chars: &[char], i: usize, kw: &str) -> bool {
    let kc: Vec<char> = kw.chars().collect();
    if i + kc.len() > chars.len() {
        return false;
    }
    for (k, c) in kc.iter().enumerate() {
        if chars[i + k] != *c {
            return false;
        }
    }
    true
}

fn is_ident_char(c: char) -> bool {
    c == '_' || c.is_alphanumeric()
}

/// 读取单引号体。zsh 单引号无转义,下一个 `'` 即结束。支持跨多行。
fn read_single_quoted(chars: &[char], i: usize) -> (String, usize) {
    // chars[i] == '\''
    let mut j = i + 1;
    let mut buf = String::new();
    while j < chars.len() {
        if chars[j] == '\'' {
            return (buf, j + 1);
        }
        buf.push(chars[j]);
        j += 1;
    }
    (buf, j) // 未闭合,返回已读
}

/// 读取双引号体(字面,不展开 $)。处理 `\"` 转义。
fn read_double_quoted(chars: &[char], i: usize) -> (String, usize) {
    // chars[i] == '"'
    let mut j = i + 1;
    let mut buf = String::new();
    while j < chars.len() {
        if chars[j] == '\\' && j + 1 < chars.len() {
            buf.push(chars[j + 1]);
            j += 2;
            continue;
        }
        if chars[j] == '"' {
            return (buf, j + 1);
        }
        buf.push(chars[j]);
        j += 1;
    }
    (buf, j)
}

/// 把 alias body(多行 export 语句)解析成 profile。
fn build_profile(name: &str, body: &str) -> Profile {
    let mut p = Profile {
        name: name.to_string(),
        ..Default::default()
    };
    for stmt in split_statements(body) {
        let s = stmt.trim();
        if s.is_empty() || s.starts_with('#') {
            continue;
        }
        if let Some(rest) = s.strip_prefix("export") {
            let rest = rest.trim_start();
            if let Some(eq) = rest.find('=') {
                let key = rest[..eq].trim();
                let val_raw = rest[eq + 1..].trim();
                if !key.is_empty() && is_valid_key(key) {
                    let val = clean_value(val_raw);
                    if !p.set_named_field(key, val.clone()) {
                        p.env.insert(key.to_string(), val);
                    }
                }
            }
        }
    }
    p
}

fn is_valid_key(k: &str) -> bool {
    !k.is_empty() && k.chars().next().map(|c| c == '_' || c.is_alphabetic()).unwrap_or(false)
        && k.chars().all(|c| c == '_' || c.is_alphanumeric())
}

/// 引号感知地按 `;` 和换行切分语句(引号内的分隔符不切)。
fn split_statements(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut prev = '\0';
    for c in body.chars() {
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single && prev != '\\' => in_double = !in_double,
            ';' | '\n' if !in_single && !in_double => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                prev = c;
                continue;
            }
            _ => {}
        }
        cur.push(c);
        prev = c;
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// 清理值:剥尾随 `;`/空白,去外层引号。
fn clean_value(raw: &str) -> String {
    let mut s = raw.trim();
    // 去尾随分号(可能多个)
    while s.ends_with(';') {
        s = s[..s.len() - 1].trim_end();
    }
    // 去外层引号
    if s.len() >= 2 {
        let bytes = s.as_bytes();
        if (bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\'')
        {
            s = &s[1..s.len() - 1];
        }
    }
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ZSHRC: &str = r#"
alias clauded='claude --dangerously-skip-permissions'
alias model_huoshan='
        export ANTHROPIC_BASE_URL="https://ark.cn-beijing.volces.com/api/plan";
        export ANTHROPIC_AUTH_TOKEN="ark-dd72d2c0-147e-4a2c-957a-192555863b2c-80312";
        export API_TIMEOUT_MS=3000000;
        export CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1;
        export ANTHROPIC_MODEL="glm-5.2";
        export ANTHROPIC_SMALL_FAST_MODEL="doubao-seed-2.0-lite";
        export ANTHROPIC_DEFAULT_HAIKU_MODEL="glm-5.2";
        export CLAUDE_CODE_MAX_CONTEXT_TOKENS=100000'

alias model_minimax='
	  export ANTHROPIC_BASE_URL="https://api.minimaxi.com/anthropic";
	  export ANTHROPIC_AUTH_TOKEN="sk-cp-v_7HCwW76skojM13";
	  export ANTHROPIC_MODEL="MiniMax-M3";
	  export CLAUDE_CODE_MAX_CONTEXT_TOKENS=256000'
"#;

    #[test]
    fn parses_huoshan_alias() {
        let profiles = parse_zshrc(ZSHRC, None);
        assert_eq!(profiles.len(), 2);
        let (name, p) = &profiles[0];
        assert_eq!(name, "model_huoshan");
        assert_eq!(p.name, "huoshan");
        assert_eq!(p.base_url.as_deref(), Some("https://ark.cn-beijing.volces.com/api/plan"));
        assert_eq!(p.auth_token.as_deref(), Some("ark-dd72d2c0-147e-4a2c-957a-192555863b2c-80312"));
        assert_eq!(p.model.as_deref(), Some("glm-5.2"));
        assert_eq!(p.small_fast_model.as_deref(), Some("doubao-seed-2.0-lite"));
        assert_eq!(p.timeout_ms.as_deref(), Some("3000000"));
        assert_eq!(p.env.get("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC").unwrap(), "1");
        assert_eq!(p.haiku_model.as_deref(), Some("glm-5.2"));
        assert_eq!(p.max_context_tokens.as_deref(), Some("100000"));
    }

    #[test]
    fn parses_minimax_with_messy_indent() {
        let profiles = parse_zshrc(ZSHRC, None);
        let (_, p) = &profiles[1];
        assert_eq!(p.name, "minimax");
        assert_eq!(p.model.as_deref(), Some("MiniMax-M3"));
        assert_eq!(p.max_context_tokens.as_deref(), Some("256000"));
    }

    #[test]
    fn ignores_non_model_alias() {
        let profiles = parse_zshrc(ZSHRC, None);
        assert!(profiles.iter().all(|(n, _)| n.starts_with("model_")));
    }

    #[test]
    fn value_with_semicolon_not_misplit() {
        let body = "export X=\"a;b\"; export Y=c";
        let p = build_profile("t", body);
        assert_eq!(p.env.get("X").unwrap(), "a;b");
        assert_eq!(p.env.get("Y").unwrap(), "c");
    }

    #[test]
    fn name_filter_matches_exact() {
        let profiles = parse_zshrc(ZSHRC, Some("clauded"));
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].0, "clauded");
    }
}
