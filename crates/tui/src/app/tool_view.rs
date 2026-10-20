//! 工具调用的展示辅助：命令名归一、参数摘要、结果摘要。
//!
//! 从 `app.rs` 拆出。

// ── 工具展示帮助函数 ─────────────────────────────────────────────────────────

/// 工具行尾部追加的后端标注，只有 Grep 会拿到非空值。
///
/// Grep 是唯一在两套搜索后端间分流的工具（`PATH` 里有 ripgrep 就走 rg 快路径，
/// 否则用内置的纯 Rust 实现，见 `crates/tools/src/grep.rs`），工具名对模型可见，
/// 光看 `⏺ Grep(...)` 分不出这次到底谁在跑，所以补一个后缀。判据直接取
/// `wyj_tools::grep_backend_label()`，与工具内部的分流同源，不会两处口径打架。
pub(crate) fn tool_backend_badge(tool_name: &str) -> String {
    // MCP 工具统一命名 `mcp__<server>__<tool>`（`crates/mcp/src/bridge.rs`）：
    // 显示名已折成 `server:tool`，行尾再补一个 `mcp` 标注，让「这是外部 server
    // 提供的工具」与内置工具一眼可分——Claude Code 也在工具行上区分这层来源。
    if tool_name.starts_with("mcp__") {
        return format!(" · {}", wyj_i18n::tr("tool_backend.mcp"));
    }
    if !tool_name.eq_ignore_ascii_case("Grep") {
        return String::new();
    }
    let key = match wyj_tools::grep_backend_label() {
        "rg" => "tool_backend.rg",
        _ => "tool_backend.native",
    };
    format!(" · {}", wyj_i18n::tr(key))
}

/// 判断这次输入是否触发了动态命令（skill），是则返回命令名。
///
/// skill 展开后的正文会被当作普通用户消息推进聊天流，用户看不出这次是某个 skill
/// 触发的——尤其在展开内容与用户原话差得远时。`is_dynamic()` 是唯一判据，与
/// `/help` 的「自定义命令」分组同源（见 `crates/commands/src/registry.rs`）。
pub(super) fn skill_invocation_name(
    registry: &wyj_commands::CommandRegistry,
    input: &str,
) -> Option<String> {
    let name = wyj_commands::CommandRegistry::command_name(input)?;
    registry
        .get(name)
        .filter(|cmd| cmd.is_dynamic())
        .map(|_| name.to_string())
}

/// 把 MCP 工具名 `mcp__<server>__<tool>` 展示为 `<server>:<tool>`。
///
/// MCP 工具在聊天流里原本只显示裸注册名 `mcp__codegraph__codegraph_explore`：
/// 既长又难读，也看不出这次调用走的是哪个 server。内置工具名不受影响。
pub(crate) fn tool_display_name(name: &str) -> String {
    match name
        .strip_prefix("mcp__")
        .and_then(|rest| rest.split_once("__"))
    {
        Some((server, tool)) if !server.is_empty() && !tool.is_empty() => {
            format!("{server}:{tool}")
        }
        _ => name.to_string(),
    }
}

/// 从工具输入 JSON 中提取用于展示的参数字符串（如文件路径或命令）
pub(super) fn tool_display_arg(name: &str, input: &serde_json::Value) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let shorten = |s: &str| -> String {
        if !home.is_empty() && s.starts_with(&home) {
            format!("~{}", &s[home.len()..])
        } else {
            s.to_string()
        }
    };
    let trunc = |s: String, max: usize| -> String {
        if s.chars().count() > max {
            format!(
                "{}…",
                s.chars().take(max.saturating_sub(1)).collect::<String>()
            )
        } else {
            s
        }
    };

    // MCP 工具的参数 schema 由各 server 自定义，没有统一字段。按最常见的键
    // 依次探测，取到第一个可读值；全都没有时退回「第一个字符串字段」，
    // 让 `mcp__x__y` 至少不是一行光秃秃的工具名。
    if name.starts_with("mcp__") {
        for key in [
            "file_path",
            "path",
            "query",
            "pattern",
            "command",
            "url",
            "name",
            "id",
        ] {
            if let Some(v) = input.get(key).and_then(|v| v.as_str()) {
                if !v.is_empty() {
                    return trunc(shorten(v), 50);
                }
            }
        }
        return input
            .as_object()
            .and_then(|obj| {
                obj.values()
                    .find_map(|v| v.as_str())
                    .filter(|s| !s.is_empty())
            })
            .map(|v| trunc(shorten(v), 50))
            .unwrap_or_default();
    }

    // 工具注册名是 PascalCase（如 "Read"/"WebFetch"，见 crates/tools/src/*.rs 的
    // Tool::name()），这里统一转小写后再匹配，否则下面的分支永远命中不了。
    let name = name.to_lowercase();
    match name.as_str() {
        "read" | "write" | "edit" => {
            let path = input
                .get("file_path")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            trunc(shorten(path), 55)
        }
        "bash" => {
            let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
            trunc(cmd.to_string(), 50)
        }
        "glob" => {
            let p = input.get("pattern").and_then(|v| v.as_str()).unwrap_or("*");
            trunc(p.to_string(), 40)
        }
        "grep" => {
            let pat = input.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            let path = input.get("path").and_then(|v| v.as_str()).unwrap_or(".");
            trunc(format!("{pat} in {}", shorten(path)), 50)
        }
        "webfetch" => {
            let url = input.get("url").and_then(|v| v.as_str()).unwrap_or("");
            trunc(url.to_string(), 55)
        }
        _ => String::new(),
    }
}

/// 从工具输出生成一行摘要（用于 ⎿ 行）
pub(super) fn tool_result_summary(name: &str, output: &str, is_error: bool) -> String {
    let trunc1 = |s: &str| -> String {
        let s = s.trim();
        if s.chars().count() > 65 {
            format!("{}…", s.chars().take(64).collect::<String>())
        } else {
            s.to_string()
        }
    };

    if is_error {
        return trunc1(output.lines().next().unwrap_or(output));
    }

    // 同上：工具注册名是 PascalCase，先转小写再匹配。
    let name = name.to_lowercase();
    match name.as_str() {
        "read" => {
            let n = output.lines().count();
            format!("read {n} lines")
        }
        "edit" => trunc1(output.lines().next().unwrap_or("updated")),
        "write" => trunc1(output.lines().next().unwrap_or("written")),
        "bash" => {
            let nonempty: Vec<&str> = output.lines().filter(|l| !l.trim().is_empty()).collect();
            let n = nonempty.len();
            let first = nonempty.first().copied().unwrap_or("(no output)");
            if n > 1 {
                format!("{} (+{} lines)", trunc1(first), n - 1)
            } else {
                trunc1(first)
            }
        }
        "grep" => {
            let n = output.lines().filter(|l| !l.trim().is_empty()).count();
            format!("{n} matches")
        }
        "glob" => {
            let n = output.lines().filter(|l| !l.trim().is_empty()).count();
            format!("{n} files")
        }
        "webfetch" => format!("fetched {} bytes", output.len()),
        _ => trunc1(
            first_informative_line(output)
                .or_else(|| output.lines().next())
                .unwrap_or(output),
        ),
    }
}

/// 兜底摘要用：跳过无信息量的结构性行（JSON/数组输出的 `{`、`[` 等），
/// 取第一条含字母/数字/CJK 的行。全都是结构行时返回 None（由调用方回退首行）。
pub(super) fn first_informative_line(output: &str) -> Option<&str> {
    output.lines().find(|l| {
        let t = l.trim();
        !t.is_empty() && t.chars().any(|c| c.is_alphanumeric())
    })
}

#[cfg(test)]
mod tool_summary_tests {
    use super::*;
    use crate::app::*;

    /// 回归测试：工具注册名是 PascalCase（Tool::name()），这两个函数曾用小写/下划线
    /// 字符串匹配，导致真实调用（"Read"/"WebFetch" 等）永远落到兜底分支——
    /// Read 的摘要变成"文件第一行内容"，和下面展开的详情第一行完全重复。
    #[test]
    pub(super) fn tool_result_summary_matches_pascal_case_tool_names() {
        let read_output = "1\tfirst line\n2\tsecond line\n";
        assert_eq!(
            tool_result_summary("Read", read_output, false),
            "read 2 lines"
        );
        assert_ne!(
            tool_result_summary("Read", read_output, false),
            read_output.lines().next().unwrap()
        );

        assert_eq!(tool_result_summary("Grep", "a\nb\nc\n", false), "3 matches");
        assert_eq!(tool_result_summary("Glob", "a\nb\n", false), "2 files");
        assert_eq!(
            tool_result_summary("WebFetch", "hello", false),
            "fetched 5 bytes"
        );
    }

    /// MCP/JSON 输出的首行往往是无信息量的 `{`：兜底摘要必须跳过结构性行取
    /// 第一条有内容的行。
    #[test]
    pub(super) fn fallback_summary_skips_structural_first_line() {
        let json = "{\n  \"code\": 0,\n  \"data\": []\n}\n";
        assert_eq!(
            tool_result_summary("mcp__a-stock__get_quote", json, false),
            "\"code\": 0,"
        );
        // 纯结构输出（不含任何字母数字）回退到首行。
        let braces = "{\n}\n";
        assert_eq!(tool_result_summary("SomeUnknownTool", braces, false), "{");
    }

    #[test]
    pub(super) fn tool_display_arg_matches_pascal_case_tool_names() {
        let input = serde_json::json!({"file_path": "/tmp/x.rs"});
        assert_eq!(tool_display_arg("Read", &input), "/tmp/x.rs");

        let bash_input = serde_json::json!({"command": "ls -la"});
        assert_eq!(tool_display_arg("Bash", &bash_input), "ls -la");

        let fetch_input = serde_json::json!({"url": "https://example.com"});
        assert_eq!(
            tool_display_arg("WebFetch", &fetch_input),
            "https://example.com"
        );
    }

    #[test]
    pub(super) fn backend_badge_only_marks_grep_and_follows_the_tools_side_label() {
        // 非 Grep / 非 MCP 工具一律不挂标注——只有 Grep 在两套搜索后端间分流
        assert_eq!(tool_backend_badge("Read"), "");
        assert_eq!(tool_backend_badge("Bash"), "");
        // Grep 的标注与工具内部的分流同源，不在 TUI 里另写一套判据
        let badge = tool_backend_badge("Grep");
        assert!(!badge.is_empty());
        assert!(badge.starts_with(' '), "标注应作为行尾后缀：{badge:?}");
        let expected = match wyj_tools::grep_backend_label() {
            "rg" => " · rg".to_string(),
            _ => format!(" · {}", wyj_i18n::tr("tool_backend.native")),
        };
        assert_eq!(badge, expected);
    }

    /// MCP 工具名 `mcp__<server>__<tool>` 在聊天流里要变得可读：名字折成
    /// `server:tool`，行尾补 ` · mcp` 标出来源，参数取不到具体字段时也不能空。
    #[test]
    pub(super) fn mcp_tools_get_readable_name_and_origin_badge() {
        assert_eq!(
            tool_display_name("mcp__codegraph__codegraph_explore"),
            "codegraph:codegraph_explore"
        );
        // 内置工具与畸形 MCP 名保持原样，不做猜测性改写。
        assert_eq!(tool_display_name("Read"), "Read");
        assert_eq!(tool_display_name("mcp__broken"), "mcp__broken");

        let badge = tool_backend_badge("mcp__codegraph__codegraph_explore");
        assert_eq!(badge, format!(" · {}", wyj_i18n::tr("tool_backend.mcp")));

        let input = serde_json::json!({"query": "HyperlinkBackend"});
        assert_eq!(
            tool_display_arg("mcp__codegraph__codegraph_explore", &input),
            "HyperlinkBackend"
        );
        // 常见键都没有时退回第一个字符串字段，避免工具行只剩一个名字。
        let odd = serde_json::json!({"verbose": true, "text": "hello world"});
        assert_eq!(tool_display_arg("mcp__x__y", &odd), "hello world");
        let empty = serde_json::json!({});
        assert_eq!(tool_display_arg("mcp__x__y", &empty), "");
    }

    /// Skill 提示只在**动态命令**上出现：内置命令（含覆盖同名 skill 的那些）
    /// 不能误触发一行「使用 skill: /xxx」。
    #[test]
    pub(super) fn skill_invocation_is_detected_for_dynamic_commands_only() {
        let dir = tempfile::tempdir().unwrap();
        let reg = wyj_commands::standard_registry_with_skills(
            dir.path(),
            dir.path(),
            &std::collections::HashSet::new(),
            &[],
            ProjectSkillsGate::Open,
        );

        assert_eq!(skill_invocation_name(&reg, "/run").as_deref(), Some("run"));
        assert_eq!(
            skill_invocation_name(&reg, "/run  某个参数").as_deref(),
            Some("run")
        );
        // `/review` 是被内置 `ReviewCmd` 顶掉的同名 skill：按注册表实际生效的那条算。
        assert_eq!(skill_invocation_name(&reg, "/review"), None);
        assert_eq!(skill_invocation_name(&reg, "/help"), None);
        assert_eq!(skill_invocation_name(&reg, "普通消息"), None);
    }
}
