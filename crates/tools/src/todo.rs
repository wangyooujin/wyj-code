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
}

#[derive(Default)]
pub struct TodoStore {
    pub items: Vec<TodoItem>,
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
}

#[async_trait]
impl Tool for TodoWriteTool {
    fn name(&self) -> &str {
        "TodoWrite"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "创建或更新结构化任务列表。每次调用会覆盖整个列表。\
                status 可为 pending、in_progress 或 completed。\
                priority 可为 high、medium、low（可选）。"
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "description": "完整的任务列表（覆盖式写入）",
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
                                }
                            }
                        }
                    }
                },
                "required": ["todos"]
            }),
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
