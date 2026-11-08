//! TodoWrite 工具 — 结构化任务列表（TUI 面板 + `.wyj-code/progress.md` 双写）。

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use wyj_api::types::ToolDefinition;
use wyj_core::tool::{Tool, ToolContext, ToolResult};

use crate::progress::todos_to_progress_markdown;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum TodoStatus {
    Pending,
    #[serde(rename = "in_progress")]
    InProgress,
    Completed,
}

impl std::fmt::Display for TodoStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TodoStatus::Pending => write!(f, "[ ]"),
            TodoStatus::InProgress => write!(f, "[~]"),
            TodoStatus::Completed => write!(f, "[x]"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoItem {
    pub id: String,
    pub content: String,
    pub status: TodoStatus,
    pub priority: Option<String>,
    /// 进行时文案（如 "Running tests"），in_progress 时 TUI 优先展示
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_form: Option<String>,
}

#[derive(Default)]
pub struct TodoStore {
    pub items: Vec<TodoItem>,
}

/// 有非空清单即可 Ctrl+T 折叠/展开。
///
/// 「默认是否折叠」不在这里决定：执行中默认展开，仅全部完成时由
/// `apply_todo_snapshot` 把 `todo_panel_expanded` 收成 false。
pub fn is_todo_collapsible(items: &[TodoItem]) -> bool {
    !items.is_empty()
}

/// 收口任务状态，让 UI「当前任务」与列表字段一致。
///
/// 1. **最多一个** `in_progress`：保留列表里**最后一个**，更早的改成 `completed`
///    （新开一项 = 自动收口旧项）。
/// 2. 若更靠后的项已经是 `completed`，而前面还有 `in_progress`，视为模型忘了
///    收口旧项（典型：A 仍 in_progress，却把 B/C 标成完成）——把那些过期的
///    `in_progress` 改成 `completed`。
///
/// 返回被自动改写的条数（供 tool_result 提示模型）。
pub fn normalize_todo_statuses(items: &mut [TodoItem]) -> usize {
    let mut changed = 0;

    // 先处理「后面已完成、前面还挂着 in_progress」：从尾部找最后一个
    // Completed 的下标，把它之前的 InProgress 全部收口。
    let last_completed = items
        .iter()
        .rposition(|t| t.status == TodoStatus::Completed);
    if let Some(last_c) = last_completed {
        for item in items.iter_mut().take(last_c) {
            if item.status == TodoStatus::InProgress {
                item.status = TodoStatus::Completed;
                changed += 1;
            }
        }
    }

    // 再保证最多一个 InProgress：从后往前，留下第一个碰到的，其余收口。
    let mut kept = false;
    for item in items.iter_mut().rev() {
        if item.status == TodoStatus::InProgress {
            if kept {
                item.status = TodoStatus::Completed;
                changed += 1;
            } else {
                kept = true;
            }
        }
    }
    changed
}

impl TodoStore {
    pub fn render_text(&self) -> String {
        if self.items.is_empty() {
            return "任务列表为空".to_string();
        }
        self.items
            .iter()
            .map(|t| {
                let prio = t
                    .priority
                    .as_deref()
                    .map(|p| format!("[{p}] "))
                    .unwrap_or_default();
                format!("{} {}{} ({})", t.status, prio, t.content, t.id)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

pub struct TodoWriteTool {
    store: Arc<Mutex<TodoStore>>,
    /// 项目根：每次写入同步到 `<root>/.wyj-code/progress.md`（同会话压缩注入；
    /// 新对话会清掉，不把上一会话 tasklist 带进面板）。
    project_root: PathBuf,
}

impl TodoWriteTool {
    pub fn new(store: Arc<Mutex<TodoStore>>, project_root: impl Into<PathBuf>) -> Self {
        Self {
            store,
            project_root: project_root.into(),
        }
    }
}

#[derive(Deserialize)]
struct Input {
    todos: Vec<TodoItemInput>,
}

#[derive(Deserialize)]
struct TodoItemInput {
    id: String,
    content: String,
    status: TodoStatus,
    #[serde(default)]
    priority: Option<String>,
    #[serde(default, rename = "activeForm")]
    active_form: Option<String>,
}

#[async_trait]
impl Tool for TodoWriteTool {
    fn name(&self) -> &str {
        "TodoWrite"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: crate::descriptions::TODO_WRITE.to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "description": "The complete task list (each call replaces the whole list)",
                        "items": {
                            "type": "object",
                            "required": ["id", "content", "status"],
                            "properties": {
                                "id": { "type": "string" },
                                "content": { "type": "string" },
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "completed"]
                                },
                                "priority": {
                                    "type": "string",
                                    "enum": ["high", "medium", "low"]
                                },
                                "activeForm": {
                                    "type": "string",
                                    "description": "Present-continuous form shown while in_progress, e.g. \"Running tests\""
                                }
                            }
                        }
                    }
                },
                "required": ["todos"]
            }),
            native: None,
        }
    }

    fn parallel_safe(&self) -> bool {
        // 纯状态写入，与 Read/Bash/Edit 等真实工作工具没有先后依赖。实测 102 个
        // 历史会话里 132 次 TodoWrite 调用**没有一次**与其它工具同轮发出——默认的
        // `parallel_safe() = false` 逼模型每次都单独占一个完整 LLM 往返，而回给
        // 模型的 tool_result（"任务列表已更新: N 项…"）信息量为零：模型自己刚写的
        // 列表，它自己知道。置 true 让模型可以把 todo 更新和真实工作合并进同一轮。
        true
    }

    async fn run(&self, input: Value, _ctx: &dyn ToolContext) -> Result<ToolResult> {
        let inp: Input = serde_json::from_value(input)?;
        let mut items: Vec<TodoItem> = inp
            .todos
            .into_iter()
            .map(|t| TodoItem {
                id: t.id,
                content: t.content,
                status: t.status,
                priority: t.priority,
                active_form: t.active_form,
            })
            .collect();

        let auto_fixed = normalize_todo_statuses(&mut items);

        let count = items.len();
        let pending = items
            .iter()
            .filter(|t| t.status == TodoStatus::Pending)
            .count();
        let in_progress = items
            .iter()
            .filter(|t| t.status == TodoStatus::InProgress)
            .count();
        let done = items
            .iter()
            .filter(|t| t.status == TodoStatus::Completed)
            .count();

        {
            let mut store = self.store.lock().unwrap();
            store.items = items.clone();
        }

        // 双写磁盘：压缩后由框架从 progress.md 重新注入；同会话 `-c` resume
        // 也可回填面板。新对话 / `/new` / `/clear` 会清掉该文件。
        // 全部完成或空清单时也清掉——否则陈旧 `[x]` 会挡住新一轮清面板，
        // 也会被压缩注入背进上下文。
        let disk_note = if items.iter().any(|t| t.status != TodoStatus::Completed) {
            let md = todos_to_progress_markdown(&items);
            if let Err(e) = wyj_core::progress::write_progress(&self.project_root, &md) {
                return Ok(ToolResult::err(format!(
                    "任务列表已写入内存，但落盘 {} 失败: {e}",
                    wyj_core::progress::PROGRESS_REL_PATH
                )));
            }
            format!("已同步 {}", wyj_core::progress::PROGRESS_REL_PATH)
        } else {
            wyj_core::progress::clear_progress(&self.project_root);
            format!("已清除 {}", wyj_core::progress::PROGRESS_REL_PATH)
        };

        let mut msg = format!(
            "任务列表已更新: {} 项（待处理 {pending}，进行中 {in_progress}，已完成 {done}）；\
             {disk_note}",
            count
        );
        if auto_fixed > 0 {
            msg.push_str(&format!(
                "；已自动收口 {auto_fixed} 项过期的 in_progress（同时只允许一项进行中；\
                 后面的项已 completed 时，前面残留的 in_progress 也会被收口）"
            ));
        }
        Ok(ToolResult::ok(msg))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::ToolCtx;
    use serde_json::json;

    fn empty_store() -> Arc<Mutex<TodoStore>> {
        Arc::new(Mutex::new(TodoStore::default()))
    }

    fn make_tool(store: Arc<Mutex<TodoStore>>) -> (TodoWriteTool, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (TodoWriteTool::new(store, dir.path()), dir)
    }

    fn make_ctx() -> ToolCtx {
        ToolCtx::new(std::env::temp_dir())
    }

    #[test]
    fn status_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&TodoStatus::Pending).unwrap(),
            "\"pending\""
        );
        assert_eq!(
            serde_json::to_string(&TodoStatus::InProgress).unwrap(),
            "\"in_progress\""
        );
        assert_eq!(
            serde_json::to_string(&TodoStatus::Completed).unwrap(),
            "\"completed\""
        );
    }

    #[test]
    fn status_deserializes_in_progress_with_underscore() {
        let s: TodoStatus = serde_json::from_str("\"in_progress\"").unwrap();
        assert_eq!(s, TodoStatus::InProgress);
        assert!(serde_json::from_str::<TodoStatus>("\"in-progress\"").is_err());
    }

    #[test]
    fn status_display_format() {
        assert_eq!(TodoStatus::Pending.to_string(), "[ ]");
        assert_eq!(TodoStatus::InProgress.to_string(), "[~]");
        assert_eq!(TodoStatus::Completed.to_string(), "[x]");
    }

    #[test]
    fn render_text_empty_store() {
        assert_eq!(TodoStore::default().render_text(), "任务列表为空");
    }

    #[test]
    fn render_text_includes_priority_and_id() {
        let store = TodoStore {
            items: vec![TodoItem {
                id: "t1".into(),
                content: "写文档".into(),
                status: TodoStatus::Pending,
                priority: Some("high".into()),
                active_form: None,
            }],
        };
        let out = store.render_text();
        assert!(out.contains("[ ]"));
        assert!(out.contains("[high]"));
        assert!(out.contains("写文档"));
        assert!(out.contains("(t1)"));
    }

    #[tokio::test]
    async fn run_writes_items_and_returns_summary() {
        let store = empty_store();
        let (tool, dir) = make_tool(store.clone());
        // 正常顺序：已完成在前，进行中，待处理在后——收口启发式不得误伤。
        let r = tool
            .run(
                json!({
                    "todos": [
                        {"id":"a","content":"task-a","status":"completed"},
                        {"id":"b","content":"task-b","status":"in_progress"},
                        {"id":"c","content":"task-c","status":"pending"}
                    ]
                }),
                &make_ctx(),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        assert!(r.content.contains("3 项"));
        assert!(r.content.contains("待处理 1"));
        assert!(r.content.contains("进行中 1"));
        assert!(r.content.contains("已完成 1"));
        assert!(r.content.contains(wyj_core::progress::PROGRESS_REL_PATH));
        let g = store.lock().unwrap();
        assert_eq!(g.items.len(), 3);
        assert_eq!(g.items[1].status, TodoStatus::InProgress);
        let md = wyj_core::progress::read_progress(dir.path()).expect("应落盘 progress.md");
        assert!(md.contains("[~]"));
        assert!(md.contains("task-b"));
    }

    #[tokio::test]
    async fn run_is_overwrite_not_append() {
        let store = empty_store();
        let (tool, dir) = make_tool(store.clone());
        tool.run(
            json!({"todos":[
                {"id":"a","content":"x","status":"pending"},
                {"id":"b","content":"y","status":"pending"},
                {"id":"c","content":"z","status":"pending"}
            ]}),
            &make_ctx(),
        )
        .await
        .unwrap();
        assert!(
            wyj_core::progress::read_progress(dir.path()).is_some(),
            "未完成清单应落盘"
        );
        let r = tool
            .run(
                json!({"todos":[{"id":"d","content":"only","status":"completed"}]}),
                &make_ctx(),
            )
            .await
            .unwrap();
        let g = store.lock().unwrap();
        assert_eq!(g.items.len(), 1);
        assert_eq!(g.items[0].id, "d");
        // 全部完成后清掉 progress.md，避免新一轮对话被陈旧 `[x]` 挡住清面板。
        assert!(r.content.contains("已清除"));
        assert!(wyj_core::progress::read_progress(dir.path()).is_none());
    }

    #[tokio::test]
    async fn run_rejects_invalid_status() {
        let (tool, _dir) = make_tool(empty_store());
        let r = tool
            .run(
                json!({"todos":[{"id":"a","content":"x","status":"done"}]}),
                &make_ctx(),
            )
            .await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn run_rejects_missing_required_field() {
        let (tool, _dir) = make_tool(empty_store());
        let r = tool
            .run(
                json!({"todos":[{"id":"a","status":"pending"}]}),
                &make_ctx(),
            )
            .await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn run_zero_items_is_valid() {
        let store = empty_store();
        let (tool, _dir) = make_tool(store.clone());
        tool.run(
            json!({"todos":[{"id":"x","content":"y","status":"pending"}]}),
            &make_ctx(),
        )
        .await
        .unwrap();
        let r = tool.run(json!({"todos":[]}), &make_ctx()).await.unwrap();
        assert!(!r.is_error);
        assert!(r.content.contains("0 项"));
        assert!(store.lock().unwrap().items.is_empty());
    }

    #[tokio::test]
    async fn run_priority_defaults_to_none() {
        let store = empty_store();
        let (tool, _dir) = make_tool(store.clone());
        tool.run(
            json!({"todos":[{"id":"a","content":"x","status":"pending"}]}),
            &make_ctx(),
        )
        .await
        .unwrap();
        assert!(store.lock().unwrap().items[0].priority.is_none());
    }

    #[tokio::test]
    async fn run_preserves_priority_when_provided() {
        let store = empty_store();
        let (tool, _dir) = make_tool(store.clone());
        tool.run(
            json!({"todos":[{"id":"a","content":"x","status":"pending","priority":"low"}]}),
            &make_ctx(),
        )
        .await
        .unwrap();
        assert_eq!(
            store.lock().unwrap().items[0].priority.as_deref(),
            Some("low")
        );
    }

    fn item(id: &str, status: TodoStatus) -> TodoItem {
        TodoItem {
            id: id.into(),
            content: format!("task-{id}"),
            status,
            priority: None,
            active_form: None,
        }
    }

    #[test]
    fn is_todo_collapsible_true_for_few_incomplete_items() {
        let items = vec![
            item("a", TodoStatus::Pending),
            item("b", TodoStatus::InProgress),
        ];
        assert!(is_todo_collapsible(&items));
    }

    #[test]
    fn is_todo_collapsible_true_when_more_than_three() {
        let items = vec![
            item("a", TodoStatus::Pending),
            item("b", TodoStatus::Pending),
            item("c", TodoStatus::Pending),
            item("d", TodoStatus::Pending),
        ];
        assert!(is_todo_collapsible(&items));
    }

    #[test]
    fn is_todo_collapsible_true_when_all_completed_even_if_few() {
        let items = vec![
            item("a", TodoStatus::Completed),
            item("b", TodoStatus::Completed),
        ];
        assert!(is_todo_collapsible(&items));
    }

    #[test]
    fn is_todo_collapsible_false_for_empty() {
        assert!(!is_todo_collapsible(&[]));
    }

    #[test]
    fn normalize_keeps_only_the_last_in_progress() {
        let mut items = vec![
            item("a", TodoStatus::InProgress),
            item("b", TodoStatus::InProgress),
            item("c", TodoStatus::Pending),
        ];
        assert_eq!(normalize_todo_statuses(&mut items), 1);
        assert_eq!(items[0].status, TodoStatus::Completed);
        assert_eq!(items[1].status, TodoStatus::InProgress);
        assert_eq!(items[2].status, TodoStatus::Pending);
    }

    #[test]
    fn normalize_completes_stale_in_progress_before_later_completed() {
        let mut items = vec![
            item("a", TodoStatus::InProgress),
            item("b", TodoStatus::Completed),
            item("c", TodoStatus::Pending),
        ];
        assert_eq!(normalize_todo_statuses(&mut items), 1);
        assert_eq!(items[0].status, TodoStatus::Completed);
        assert_eq!(items[1].status, TodoStatus::Completed);
        assert_eq!(items[2].status, TodoStatus::Pending);
    }

    #[tokio::test]
    async fn run_auto_normalizes_multiple_in_progress() {
        let store = empty_store();
        let (tool, _dir) = make_tool(store.clone());
        let r = tool
            .run(
                json!({
                    "todos": [
                        {"id":"a","content":"task-a","status":"in_progress"},
                        {"id":"b","content":"task-b","status":"in_progress"}
                    ]
                }),
                &make_ctx(),
            )
            .await
            .unwrap();
        assert!(r.content.contains("自动收口"));
        let g = store.lock().unwrap();
        assert_eq!(g.items[0].status, TodoStatus::Completed);
        assert_eq!(g.items[1].status, TodoStatus::InProgress);
    }

    #[test]
    fn tool_name_is_todowrite() {
        let (tool, _dir) = make_tool(empty_store());
        assert_eq!(tool.name(), "TodoWrite");
    }

    #[test]
    fn definition_declares_required_todos_field() {
        let (tool, _dir) = make_tool(empty_store());
        let def = tool.definition();
        assert_eq!(def.name, "TodoWrite");
        let req = def
            .input_schema
            .pointer("/required")
            .and_then(|v| v.as_array())
            .unwrap();
        assert!(req.iter().any(|v| v == "todos"));
    }
}
