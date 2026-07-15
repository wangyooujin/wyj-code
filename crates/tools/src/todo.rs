//! TodoWrite 工具 — 管理结构化任务列表

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use wyj_api::types::ToolDefinition;
use wyj_core::tool::{Tool, ToolContext, ToolResult};

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

/// 任务面板是否应默认折叠：条数超过 3，或全部任务已完成。
pub fn is_todo_collapsible(items: &[TodoItem]) -> bool {
    items.len() > 3
        || (!items.is_empty() && items.iter().all(|t| t.status == TodoStatus::Completed))
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
}

impl TodoWriteTool {
    pub fn new(store: Arc<Mutex<TodoStore>>) -> Self {
        Self { store }
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

    async fn run(&self, input: Value, _ctx: &dyn ToolContext) -> Result<ToolResult> {
        let inp: Input = serde_json::from_value(input)?;
        let items: Vec<TodoItem> = inp
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
            store.items = items;
        }

        Ok(ToolResult::ok(format!(
            "任务列表已更新: {} 项（待处理 {pending}，进行中 {in_progress}，已完成 {done}）",
            count
        )))
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
        let tool = TodoWriteTool::new(store.clone());
        let r = tool
            .run(
                json!({
                    "todos": [
                        {"id":"a","content":"task-a","status":"pending"},
                        {"id":"b","content":"task-b","status":"in_progress"},
                        {"id":"c","content":"task-c","status":"completed"}
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
        let g = store.lock().unwrap();
        assert_eq!(g.items.len(), 3);
        assert_eq!(g.items[1].status, TodoStatus::InProgress);
    }

    #[tokio::test]
    async fn run_is_overwrite_not_append() {
        let store = empty_store();
        let tool = TodoWriteTool::new(store.clone());
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
        tool.run(
            json!({"todos":[{"id":"d","content":"only","status":"completed"}]}),
            &make_ctx(),
        )
        .await
        .unwrap();
        let g = store.lock().unwrap();
        assert_eq!(g.items.len(), 1);
        assert_eq!(g.items[0].id, "d");
    }

    #[tokio::test]
    async fn run_rejects_invalid_status() {
        let r = TodoWriteTool::new(empty_store())
            .run(
                json!({"todos":[{"id":"a","content":"x","status":"done"}]}),
                &make_ctx(),
            )
            .await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn run_rejects_missing_required_field() {
        let r = TodoWriteTool::new(empty_store())
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
        let tool = TodoWriteTool::new(store.clone());
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
        TodoWriteTool::new(store.clone())
            .run(
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
        TodoWriteTool::new(store.clone())
            .run(
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
    fn is_todo_collapsible_false_for_few_incomplete_items() {
        let items = vec![
            item("a", TodoStatus::Pending),
            item("b", TodoStatus::InProgress),
        ];
        assert!(!is_todo_collapsible(&items));
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
    fn tool_name_is_todowrite() {
        assert_eq!(TodoWriteTool::new(empty_store()).name(), "TodoWrite");
    }

    #[test]
    fn definition_declares_required_todos_field() {
        let def = TodoWriteTool::new(empty_store()).definition();
        assert_eq!(def.name, "TodoWrite");
        let req = def
            .input_schema
            .pointer("/required")
            .and_then(|v| v.as_array())
            .unwrap();
        assert!(req.iter().any(|v| v == "todos"));
    }
}
