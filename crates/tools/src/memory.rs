//! 单一 Memory 工具：用 action 暴露搜索、阅读、写入、纠正、撤销和状态。
//! 保持 schema 紧凑，避免把固定记忆流程硬编码进 harness。

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;
use wyj_api::types::ToolDefinition;
use wyj_core::tool::{Tool, ToolContext, ToolResult};
use wyj_core::{
    MemoryClaimKind, MemoryClaimScope, MemoryEvidence, MemorySource, MemorySourceKind,
    MemoryV3Store, MemoryWriteRequest, TaskStatus, TaskStep,
};

pub struct MemoryTool {
    store: Arc<MemoryV3Store>,
}

impl MemoryTool {
    pub fn new(store: Arc<MemoryV3Store>) -> Self {
        Self { store }
    }
}

#[derive(Debug, Deserialize)]
struct Input {
    action: String,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    recent_context: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    kind: Option<MemoryClaimKind>,
    #[serde(default)]
    scope: Option<MemoryClaimScope>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    entities: Vec<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    source_kind: Option<MemorySourceKind>,
    #[serde(default)]
    source_locator: Option<String>,
    #[serde(default)]
    observed_at: Option<String>,
    #[serde(default)]
    expires_at: Option<String>,
    #[serde(default)]
    confidence: Option<f32>,
    #[serde(default)]
    evidence: Vec<MemoryEvidence>,
    #[serde(default)]
    task_status: Option<TaskStatus>,
    #[serde(default)]
    task_steps: Vec<TaskStep>,
    #[serde(default)]
    blocked_reason: Option<String>,
}

#[async_trait]
impl Tool for MemoryTool {
    fn name(&self) -> &str {
        "Memory"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Search and manage durable cross-session memory claims. Two scopes only: `global` (cross-project, requires user natural-language confirmation via `confirm_global_candidate` before activation) and `project` (auto-managed for the current working directory). Search before recurring/continuation work; write project facts directly; write inferences only as expiring hypotheses. Use `kind: \"task\"` to track ongoing work and recovery points (Project scope only, requires task_status); use supersede for changed state, forget to undo a bad write, and reject_global_candidate when the user does not want a pending Global preference promoted. Reference claims never enter the Project Brief, so prefer facts over reference for hot context.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["search", "read", "write", "supersede", "forget", "status", "list_pending_global_candidates", "confirm_global_candidate", "reject_global_candidate"],
                        "description": "Operation to perform"
                    },
                    "query": {"type": "string", "description": "search: semantic/lexical query in the user's language"},
                    "recent_context": {"type": "string", "description": "search: recent task/topic, especially for 'continue' requests"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 50},
                    "id": {"type": "string", "description": "read/forget target, or supersede predecessor"},
                    "reason": {"type": "string", "description": "forget audit reason"},
                    "kind": {
                        "type": "string",
                        "enum": ["instruction", "preference", "fact", "mutable_state", "event", "workflow", "hypothesis", "reference", "task"]
                    },
                    "scope": {"type": "string", "enum": ["global", "project"]},
                    "title": {"type": "string"},
                    "content": {"type": "string"},
                    "entities": {"type": "array", "items": {"type": "string"}, "description": "stable identifiers, names, tickers, paths"},
                    "tags": {"type": "array", "items": {"type": "string"}},
                    "source_kind": {"type": "string", "enum": ["user", "tool", "assistant", "external", "legacy"]},
                    "source_locator": {"type": "string", "description": "session/tool/path/URL and claim location"},
                    "observed_at": {"type": "string", "description": "RFC3339; required for mutable_state"},
                    "expires_at": {"type": "string", "description": "RFC3339; required for hypothesis and recommended for external facts"},
                    "confidence": {"type": "number", "minimum": 0, "maximum": 1},
                    "evidence": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "quote": {"type": "string"},
                                "locator": {"type": "string"},
                                "observed_at": {"type": "string"}
                            },
                            "required": ["quote", "locator"]
                        }
                    },
                    "task_status": {
                        "type": "string",
                        "enum": ["in_progress", "completed", "cancelled", "blocked"],
                        "description": "Required when kind=task; Project scope only."
                    },
                    "task_steps": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "description": {"type": "string"},
                                "done": {"type": "boolean"},
                                "updated_at": {"type": "string"}
                            },
                            "required": ["description"]
                        },
                        "description": "Ordered steps for the task; the first !done entry is the next step."
                    },
                    "blocked_reason": {
                        "type": "string",
                        "description": "Required when task_status=blocked."
                    }
                },
                "required": ["action"]
            }),
            native: None,
        }
    }

    async fn run(&self, input: Value, _ctx: &dyn ToolContext) -> Result<ToolResult> {
        let input: Input = serde_json::from_value(input)?;
        // store 上的所有错误（schema 校验失败、fingerprint 被拒、id 不存在等）
        // 都是正常业务错误，必须返回 ToolResult::err 让模型看到可读错误文本，
        // 而不是向上抛成 harness 级 Err 终止当前 turn。
        fn map_store<T: serde::Serialize>(result: anyhow::Result<T>) -> ToolResult {
            match result {
                Ok(value) => match serde_json::to_value(value) {
                    Ok(json) => match serde_json::to_string_pretty(&json) {
                        Ok(text) => ToolResult::ok(text),
                        Err(error) => ToolResult::err(format!("序列化失败: {error}")),
                    },
                    Err(error) => ToolResult::err(format!("序列化失败: {error}")),
                },
                Err(error) => ToolResult::err(error.to_string()),
            }
        }
        match input.action.as_str() {
            "search" => {
                let query = input.query.as_deref().context("search 需要 query")?;
                Ok(map_store(self.store.search(
                    query,
                    input.recent_context.as_deref(),
                    input.limit,
                )))
            }
            "read" => {
                let id = input.id.as_deref().context("read 需要 id")?;
                Ok(map_store(self.store.read(id)))
            }
            "clear_all" => {
                // AI 不可静默触发破坏性批操作；必须由用户走 /memory clear-all
                // 或 `wyj-code memory clear-all --yes`，前者走 TUI 二级确认。
                Ok(ToolResult::err(
                    "AI 不能直接清空记忆库；请通过 /memory clear-all 让用户确认，或在 CLI 用 `wyj-code memory clear-all --yes`。",
                ))
            }
            "write" | "supersede" => {
                let supersedes = if input.action == "supersede" {
                    Some(input.id.clone().context("supersede 需要旧记忆 id")?)
                } else {
                    None
                };
                let request = MemoryWriteRequest {
                    kind: input.kind.context("write/supersede 需要 kind")?,
                    scope: input.scope.context("write/supersede 需要 scope")?,
                    title: input.title.context("write/supersede 需要 title")?,
                    content: input.content.context("write/supersede 需要 content")?,
                    entities: input.entities,
                    tags: input.tags,
                    source: MemorySource {
                        kind: input
                            .source_kind
                            .context("write/supersede 需要 source_kind")?,
                        locator: input
                            .source_locator
                            .context("write/supersede 需要 source_locator")?,
                        observed_at: input.observed_at,
                    },
                    evidence: input.evidence,
                    confidence: input.confidence.unwrap_or(0.8),
                    expires_at: input.expires_at,
                    supersedes,
                    task_status: input.task_status,
                    task_steps: input.task_steps,
                    blocked_reason: input.blocked_reason,
                };
                Ok(map_store(self.store.upsert(request)))
            }
            "forget" => {
                let id = input.id.as_deref().context("forget 需要 id")?;
                let reason = input.reason.as_deref().unwrap_or("Memory tool undo");
                Ok(map_store(self.store.forget(id, reason)))
            }
            "status" => Ok(map_store(self.store.status())),
            "list_pending_global_candidates" => {
                Ok(map_store(self.store.list_pending_global_candidates()))
            }
            "confirm_global_candidate" => {
                let id = input
                    .id
                    .as_deref()
                    .context("confirm_global_candidate 需要 id")?;
                Ok(map_store(self.store.confirm_global_candidate(id)))
            }
            "reject_global_candidate" => {
                let id = input
                    .id
                    .as_deref()
                    .context("reject_global_candidate 需要 id")?;
                let reason = input.reason.as_deref().unwrap_or("user rejected");
                Ok(map_store(self.store.reject_global_candidate(id, reason)))
            }
            other => Ok(ToolResult::err(format!("未知 Memory action: {other}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::{Path, PathBuf};

    struct TestCtx(PathBuf);

    #[async_trait]
    impl ToolContext for TestCtx {
        fn cwd(&self) -> &Path {
            &self.0
        }

        fn is_allowed(&self, _name: &str, _input: &Value) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn write_then_search_returns_provenance() {
        let base = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let store = Arc::new(MemoryV3Store::new(base.path(), project.path()).unwrap());
        let tool = MemoryTool::new(store);
        let ctx = TestCtx(project.path().to_path_buf());
        let written = tool
            .run(
                json!({
                    "action": "write",
                    "kind": "fact",
                    "scope": "project",
                    "title": "项目入口",
                    "content": "CLI entry is crates/cli/src/main.rs",
                    "entities": ["crates/cli/src/main.rs"],
                    "source_kind": "tool",
                    "source_locator": "Read:crates/cli/src/main.rs",
                    "observed_at": "2026-08-20T10:00:00+08:00"
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!written.is_error);

        let searched = tool
            .run(json!({"action":"search", "query":"CLI入口"}), &ctx)
            .await
            .unwrap();
        assert!(searched.content.contains("crates/cli/src/main.rs"));
        assert!(searched.content.contains("source"));
    }

    #[tokio::test]
    async fn list_confirm_reject_global_candidate_round_trip() {
        let base = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let store = Arc::new(MemoryV3Store::new(base.path(), project.path()).unwrap());
        let tool = MemoryTool::new(store.clone());
        let ctx = TestCtx(project.path().to_path_buf());

        // 助手背景提取 → 落到 Pending，不参与搜索。
        let pending = tool
            .run(
                json!({
                    "action": "write",
                    "kind": "preference",
                    "scope": "global",
                    "title": "持仓分析偏好",
                    "content": "持仓分析默认逐只打分并汇总",
                    "source_kind": "assistant",
                    "source_locator": "session:test#assistant-1",
                    "observed_at": "2026-08-20T09:30:00+08:00"
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(pending.content.contains("pending_global_candidate"));

        let list = tool
            .run(json!({"action":"list_pending_global_candidates"}), &ctx)
            .await
            .unwrap();
        assert!(list.content.contains("持仓分析偏好"));

        let search = tool
            .run(json!({"action":"search","query":"持仓分析"}), &ctx)
            .await
            .unwrap();
        assert!(
            !search.content.contains("持仓分析偏好"),
            "pending must not surface"
        );

        // confirm：状态翻 Active，重新搜索命中。
        let pending_id = serde_json::from_str::<serde_json::Value>(&pending.content)
            .unwrap()
            .get("id")
            .unwrap()
            .as_str()
            .unwrap()
            .to_string();
        let confirmed = tool
            .run(
                json!({"action":"confirm_global_candidate","id": pending_id}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(confirmed.content.contains("\"status\": \"active\""));

        let search2 = tool
            .run(json!({"action":"search","query":"持仓分析"}), &ctx)
            .await
            .unwrap();
        assert!(search2.content.contains("持仓分析偏好"));
    }

    #[tokio::test]
    async fn reject_global_candidate_blocks_repeat_upsert() {
        let base = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let store = Arc::new(MemoryV3Store::new(base.path(), project.path()).unwrap());
        let tool = MemoryTool::new(store.clone());
        let ctx = TestCtx(project.path().to_path_buf());

        let pending = tool
            .run(
                json!({
                    "action": "write",
                    "kind": "preference",
                    "scope": "global",
                    "title": "持仓分析偏好",
                    "content": "持仓分析默认逐只打分并汇总",
                    "source_kind": "assistant",
                    "source_locator": "session:test#assistant-1",
                    "observed_at": "2026-08-20T09:30:00+08:00"
                }),
                &ctx,
            )
            .await
            .unwrap();
        let pending_id = serde_json::from_str::<serde_json::Value>(&pending.content)
            .unwrap()
            .get("id")
            .unwrap()
            .as_str()
            .unwrap()
            .to_string();

        let rejected = tool
            .run(
                json!({
                    "action": "reject_global_candidate",
                    "id": pending_id,
                    "reason": "user rejected"
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!rejected.is_error);
        assert!(rejected.content.contains("\"status\": \"rejected\""));

        // 同 fingerprint 再次 upsert 应返回 is_error。
        let again = tool
            .run(
                json!({
                    "action": "write",
                    "kind": "preference",
                    "scope": "global",
                    "title": "持仓分析偏好",
                    "content": "持仓分析默认逐只打分并汇总",
                    "source_kind": "assistant",
                    "source_locator": "session:test#assistant-2",
                    "observed_at": "2026-08-20T10:30:00+08:00"
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(again.is_error);
        assert!(again.content.contains("已被用户拒绝过"));
    }

    #[tokio::test]
    async fn write_task_through_tool_appears_in_brief() {
        let base = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let store = Arc::new(MemoryV3Store::new(base.path(), project.path()).unwrap());
        let tool = MemoryTool::new(store.clone());
        let ctx = TestCtx(project.path().to_path_buf());

        let written = tool
            .run(
                json!({
                    "action": "write",
                    "kind": "task",
                    "scope": "project",
                    "title": "迁移到 Memory v3 final 设计",
                    "content": "删除 Workspace、收敛 Task/Brief、补 clear-all",
                    "entities": ["memory-v3"],
                    "source_kind": "assistant",
                    "source_locator": "session:test#assistant-task",
                    "observed_at": "2026-08-22T09:00:00+08:00",
                    "task_status": "in_progress",
                    "task_steps": [
                        {"description": "删除 Workspace scope", "done": true, "updated_at": "2026-08-22T08:30:00+08:00"},
                        {"description": "补 Task + Brief + 继续", "done": false}
                    ]
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!written.is_error);

        let brief = store.project_brief("");
        assert!(brief.contains("### Open Tasks"));
        assert!(brief.contains("迁移到 Memory v3 final 设计"));
        assert!(brief.contains("next: 补 Task + Brief + 继续"));
    }

    #[tokio::test]
    async fn write_task_blocked_requires_blocked_reason() {
        let base = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let store = Arc::new(MemoryV3Store::new(base.path(), project.path()).unwrap());
        let tool = MemoryTool::new(store);
        let ctx = TestCtx(project.path().to_path_buf());

        let bad = tool
            .run(
                json!({
                    "action": "write",
                    "kind": "task",
                    "scope": "project",
                    "title": "等用户确认",
                    "content": "等用户回复偏好",
                    "source_kind": "assistant",
                    "source_locator": "session:test#assistant-task",
                    "observed_at": "2026-08-22T09:00:00+08:00",
                    "task_status": "blocked",
                    "task_steps": [{"description": "等回复", "done": false}]
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(bad.is_error, "Blocked 缺 blocked_reason 应被拒");
        assert!(bad.content.contains("blocked_reason"));
    }

    #[tokio::test]
    async fn memory_tool_clear_all_is_rejected_for_ai() {
        let base = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let store = Arc::new(MemoryV3Store::new(base.path(), project.path()).unwrap());
        let tool = MemoryTool::new(store.clone());
        let ctx = TestCtx(project.path().to_path_buf());

        let result = tool.run(json!({"action":"clear_all"}), &ctx).await.unwrap();
        assert!(result.is_error, "AI 调 clear_all 必须被拒");
        assert!(result.content.contains("AI 不能直接清空"));
        // 库不被任何动作改变。
        let status = store.status().unwrap();
        assert_eq!(status.active_records, 0);
    }
}
