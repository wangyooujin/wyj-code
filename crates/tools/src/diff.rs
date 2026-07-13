//! 共享 diff 生成 — 用 similar crate 计算行级 diff
//!
//! 供 Edit / Write 工具把代码改动以 unified-diff 风格输出，
//! 便于 TUI 直接带 `+`/`-` 配色渲染，也便于 AI 在下轮对话中看到具体改动。

use similar::{ChangeTag, TextDiff};

/// 生成行级 diff 文本。
///
/// 格式：上下文行以两个空格开头，删除行以 `- ` 开头，新增行以 `+ ` 开头。
/// 不输出 `@@ hunk` 头，保持紧凑。
///
/// - `old` / `new` 为原始文本（按行切分）。
/// - `max_lines` 限制输出行数（含截断提示行），避免超长 diff 撑爆输出。
///   超限时末尾追加 `…(truncated, N more lines)`。
pub fn make_diff(old: &str, new: &str, max_lines: usize) -> String {
    let diff = TextDiff::from_lines(old, new);
    let mut out = String::new();
    let mut emitted = 0usize;

    for (emitted_idx, change) in diff.iter_all_changes().enumerate() {
        if emitted >= max_lines {
            let rest = diff.iter_all_changes().count().saturating_sub(emitted_idx);
            out.push_str(&format!("…(truncated, {rest} more lines)\n"));
            break;
        }
        let prefix = match change.tag() {
            ChangeTag::Equal => "  ",
            ChangeTag::Delete => "- ",
            ChangeTag::Insert => "+ ",
        };
        let value = change.value();
        out.push_str(prefix);
        out.push_str(value);
        // similar 按行切，value 已含尾部换行（末行可能没有），保证行尾
        if !value.ends_with('\n') {
            out.push('\n');
        }
        emitted = emitted_idx + 1;
    }

    // 移除末尾多余换行
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

/// 新文件专用：整份内容作为纯新增（全行 `+`），用于 Write 创建新文件的场景。
pub fn make_added(content: &str, max_lines: usize) -> String {
    let mut out = String::new();
    let mut emitted = 0usize;
    let total = content.lines().count();

    for (emitted_idx, line) in content.lines().enumerate() {
        if emitted >= max_lines {
            let rest = total.saturating_sub(emitted_idx);
            out.push_str(&format!("…(truncated, {rest} more lines)\n"));
            break;
        }
        out.push_str("+ ");
        out.push_str(line);
        out.push('\n');
        emitted = emitted_idx + 1;
    }

    if out.ends_with('\n') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_basic_change() {
        let d = make_diff("a\nb\nc\n", "a\nB\nc\n", 200);
        let lines: Vec<&str> = d.lines().collect();
        assert!(lines.contains(&"  a"));
        assert!(lines.contains(&"- b"));
        assert!(lines.contains(&"+ B"));
        assert!(lines.contains(&"  c"));
    }

    #[test]
    fn diff_insert_only() {
        let d = make_diff("a\n", "a\nb\n", 200);
        let lines: Vec<&str> = d.lines().collect();
        assert!(lines.contains(&"  a"));
        assert!(lines.contains(&"+ b"));
        assert!(!lines.iter().any(|l| l.starts_with("- ")));
    }

    #[test]
    fn diff_truncation() {
        let old = (0..50)
            .map(|i| format!("old{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let new = (0..50)
            .map(|i| format!("new{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let d = make_diff(&old, &new, 10);
        assert!(d.contains("…(truncated"));
    }

    #[test]
    fn added_all_plus() {
        let d = make_added("hello\nworld\n", 200);
        let lines: Vec<&str> = d.lines().collect();
        assert_eq!(lines, vec!["+ hello", "+ world"]);
    }

    #[test]
    fn added_truncation() {
        let content = (0..30)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let d = make_added(&content, 5);
        assert!(d.contains("…(truncated"));
        let emitted = d.lines().filter(|l| l.starts_with("+ ")).count();
        assert_eq!(emitted, 5);
    }

    #[test]
    fn diff_identical_empty_output() {
        let d = make_diff("same\n", "same\n", 200);
        // 仅有上下文行
        assert_eq!(d, "  same");
    }
}
