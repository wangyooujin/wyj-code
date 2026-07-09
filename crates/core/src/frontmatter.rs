//! 轻量 Markdown frontmatter 解析器 —— 供 agent_def（`~/.claude/agents/*.md`）
//! 与 commands::skill（`~/.claude/commands/*.md` / skill 文件）共用。
//!
//! 不是完整 YAML，只支持逐行 `key: value`，与真实 Claude Code 的 frontmatter 字段
//! （如 `tools: Read, Grep, Glob` 逗号分隔字符串）风格一致，无需引入重量级 yaml 依赖。

/// 解析 markdown 文件头部的 `---` frontmatter 块。
/// 返回 (key-value 对列表, 正文)。无 frontmatter 或未闭合时字段列表为空、整个内容作为正文。
pub fn parse(content: &str) -> (Vec<(String, String)>, &str) {
    let Some(rest) = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))
    else {
        return (vec![], content);
    };
    // 找到关闭的 --- 行
    let mut fields = vec![];
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed.trim() == "---" {
            let body = &rest[offset + line.len()..];
            return (fields, body);
        }
        if let Some((key, value)) = trimmed.split_once(':') {
            let key = key.trim().to_string();
            let value = value.trim().to_string();
            if !key.is_empty() {
                fields.push((key, value));
            }
        }
        offset += line.len();
    }
    // 没有关闭的 ---：视作无 frontmatter，整个内容当正文
    (vec![], content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_frontmatter_whole_file_is_body() {
        let (fields, body) = parse("# 标题\n\n正文");
        assert!(fields.is_empty());
        assert_eq!(body, "# 标题\n\n正文");
    }

    #[test]
    fn unclosed_frontmatter_treated_as_body() {
        let (fields, body) = parse("---\nname: x\n没有关闭");
        assert!(fields.is_empty());
        assert!(body.contains("name: x"));
    }

    #[test]
    fn value_with_colon_kept_intact() {
        let (fields, _) = parse("---\ndescription: 用法: 先读后写\n---\nbody");
        assert_eq!(
            fields,
            vec![("description".to_string(), "用法: 先读后写".to_string())]
        );
    }

    #[test]
    fn crlf_frontmatter() {
        let (fields, body) = parse("---\r\nname: win\r\n---\r\nbody");
        assert_eq!(fields, vec![("name".to_string(), "win".to_string())]);
        assert_eq!(body, "body");
    }

    #[test]
    fn multiple_fields_parsed_in_order() {
        let (fields, body) = parse("---\nname: x\ndescription: d\n---\nbody");
        assert_eq!(
            fields,
            vec![
                ("name".to_string(), "x".to_string()),
                ("description".to_string(), "d".to_string()),
            ]
        );
        assert_eq!(body, "body");
    }
}
