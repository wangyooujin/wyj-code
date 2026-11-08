//! Progress 文件解析 — `.wyj-code/progress.md` ↔ [`TodoItem`]。
//!
//! 独立 Progress **工具**已并入 [`crate::todo::TodoWriteTool`]（内存面板 + 磁盘
//! 双写）。本模块只保留解析/序列化，供 TodoWrite 落盘、TUI 启动回填、TurnDone
//! 收口写回使用。压缩注入仍走 `wyj_core::progress`。

use crate::todo::{TodoItem, TodoStatus};

#[derive(Clone, Copy)]
enum Section {
    Done,
    Doing,
    Next,
    Blockers,
    None,
}

/// 把 progress.md 解析成任务列表面板用的 [`TodoItem`]。
///
/// 支持：
/// - GitHub 勾选框：`- [ ]` pending、`- [x]` completed、`- [~]`/`- [/]`/`- [-]` in_progress
/// - 分段标题（大小写不敏感）：`Done` / `Completed` / `已完成`、
///   `In progress` / `Doing` / `进行中`、`Next` / `Todo` / `Pending` / `下一步`、
///   `Blockers` / `Blocked` / `阻塞` —— 标题下的 `- item` 无勾选框时继承段状态
/// - 纯 `- item` 且无段上下文：一律 pending；若全文恰好一项则标 in_progress
pub fn parse_progress_to_todos(content: &str) -> Vec<TodoItem> {
    let mut items = Vec::new();
    let mut section = Section::None;

    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(title) = heading_title(line) {
            section = classify_section(title);
            continue;
        }
        if let Some((status, text)) = parse_checkbox_line(line) {
            push_item(&mut items, text, status);
            continue;
        }
        if let Some(text) = bullet_text(line) {
            let status = match section {
                Section::Done => TodoStatus::Completed,
                Section::Doing => TodoStatus::InProgress,
                Section::Next | Section::Blockers | Section::None => TodoStatus::Pending,
            };
            push_item(&mut items, text, status);
        }
    }

    if items.len() == 1 && items[0].status == TodoStatus::Pending {
        items[0].status = TodoStatus::InProgress;
    }
    // 与 TodoWrite 同款收口，避免 markdown 里写出多个 [~]。
    crate::todo::normalize_todo_statuses(&mut items);
    items
}

/// 把面板条目序列化回 progress.md（TodoWrite 落盘 / TurnDone 收口写盘）。
pub fn todos_to_progress_markdown(items: &[TodoItem]) -> String {
    let mut out = String::from("# Progress\n\n");
    for item in items {
        let mark = match item.status {
            TodoStatus::Completed => "[x]",
            TodoStatus::InProgress => "[~]",
            TodoStatus::Pending => "[ ]",
        };
        out.push_str("- ");
        out.push_str(mark);
        out.push(' ');
        out.push_str(item.content.trim());
        out.push('\n');
    }
    out
}

fn push_item(items: &mut Vec<TodoItem>, text: &str, status: TodoStatus) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    let id = format!("p{}", items.len() + 1);
    items.push(TodoItem {
        id,
        content: text.to_string(),
        status,
        active_form: None,
    });
}

fn heading_title(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix('#')?;
    let rest = rest.trim_start_matches('#').trim();
    if rest.is_empty() {
        None
    } else {
        Some(rest)
    }
}

fn classify_section(title: &str) -> Section {
    let t = title.trim().to_ascii_lowercase();
    if title.contains("已完成") || (title.contains("完成") && !title.contains("未完成")) {
        return Section::Done;
    }
    if title.contains("进行中") || title.contains("正在") {
        return Section::Doing;
    }
    if title.contains("下一步") || title.contains("待办") || title.contains("未完成") {
        return Section::Next;
    }
    if title.contains("阻塞") {
        return Section::Blockers;
    }
    if t == "done" || t == "completed" || t == "complete" || t.starts_with("done ") {
        return Section::Done;
    }
    if t == "in progress"
        || t == "in-progress"
        || t == "doing"
        || t == "wip"
        || t.starts_with("in progress")
        || t.starts_with("doing")
    {
        return Section::Doing;
    }
    if t == "next"
        || t == "todo"
        || t == "pending"
        || t == "todos"
        || t == "up next"
        || t.starts_with("next ")
        || t.starts_with("todo")
        || t.starts_with("pending")
    {
        return Section::Next;
    }
    if t == "blockers" || t == "blocked" || t == "blocker" || t.starts_with("block") {
        return Section::Blockers;
    }
    Section::None
}

fn parse_checkbox_line(line: &str) -> Option<(TodoStatus, &str)> {
    let rest = strip_bullet(line)?;
    let rest = rest.trim_start();
    let t = rest.strip_prefix('[')?;
    let close = t.find(']')?;
    let mark = t[..close].trim();
    let text = t[close + 1..].trim_start();
    let status = match mark {
        "x" | "X" => TodoStatus::Completed,
        "~" | "/" | "-" | "*" | "o" | "O" => TodoStatus::InProgress,
        "" | " " => TodoStatus::Pending,
        _ => return None,
    };
    Some((status, text))
}

fn bullet_text(line: &str) -> Option<&str> {
    let rest = strip_bullet(line)?;
    let rest = rest.trim();
    if rest.starts_with('[') {
        return None; // 勾选框已由 parse_checkbox_line 处理；到这里说明格式怪，跳过
    }
    if rest.is_empty() {
        None
    } else {
        Some(rest)
    }
}

fn strip_bullet(line: &str) -> Option<&str> {
    let t = line.trim_start();
    t.strip_prefix("- ")
        .or_else(|| t.strip_prefix("* "))
        .or_else(|| t.strip_prefix("+ "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_checkbox_statuses() {
        let items = parse_progress_to_todos(
            "# Progress\n\n- [x] shipped\n- [~] wiring panel\n- [ ] polish\n",
        );
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].status, TodoStatus::Completed);
        assert_eq!(items[0].content, "shipped");
        assert_eq!(items[1].status, TodoStatus::InProgress);
        assert_eq!(items[2].status, TodoStatus::Pending);
    }

    #[test]
    fn parse_section_bullets_without_checkboxes() {
        let items = parse_progress_to_todos("## Done\n- a\n## In progress\n- b\n## Next\n- c\n");
        assert_eq!(items[0].status, TodoStatus::Completed);
        assert_eq!(items[1].status, TodoStatus::InProgress);
        assert_eq!(items[2].status, TodoStatus::Pending);
    }

    #[test]
    fn parse_chinese_sections() {
        let items = parse_progress_to_todos("## 已完成\n- 甲\n## 进行中\n- 乙\n## 下一步\n- 丙\n");
        assert_eq!(items[0].content, "甲");
        assert_eq!(items[0].status, TodoStatus::Completed);
        assert_eq!(items[1].status, TodoStatus::InProgress);
        assert_eq!(items[2].status, TodoStatus::Pending);
    }

    #[test]
    fn parse_normalizes_multiple_in_progress() {
        let items = parse_progress_to_todos("- [~] a\n- [~] b\n");
        assert_eq!(items[0].status, TodoStatus::Completed);
        assert_eq!(items[1].status, TodoStatus::InProgress);
    }

    #[test]
    fn round_trip_markdown() {
        let src = "- [x] a\n- [~] b\n- [ ] c\n";
        let items = parse_progress_to_todos(src);
        let out = todos_to_progress_markdown(&items);
        let again = parse_progress_to_todos(&out);
        assert_eq!(again.len(), 3);
        assert_eq!(again[0].status, TodoStatus::Completed);
        assert_eq!(again[1].status, TodoStatus::InProgress);
        assert_eq!(again[2].status, TodoStatus::Pending);
    }
}
