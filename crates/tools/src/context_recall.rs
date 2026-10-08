//! `ContextRecall`：把被 context editing 外部化掉的工具输出取回。
//!
//! 清理旧工具输出是"移出模型视野"而不是"删除"——原文始终在 CAS 里，占位符里
//! 带着 `cas://<hash>`。这个工具就是模型取回它的入口。没有它，清理会变成不可逆
//! 的信息损失（对 Bash / WebFetch 这类一次性命令尤其致命，重跑也拿不回）。
//!
//! 不注册时不影响主流程：占位符里仍然写着 `cas://<hash>`，模型至少知道
//! "这里曾经有内容、来自哪个工具、大概多大"，可以决定重跑工具而不是凭空编造。

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;
use wyj_api::types::ToolDefinition;
use wyj_core::tool::{Tool, ToolContext, ToolResult};
use wyj_core::workspace_cas::WorkspaceCas;

pub struct ContextRecallTool {
    cas: Arc<WorkspaceCas>,
}

impl ContextRecallTool {
    pub fn new(cas: Arc<WorkspaceCas>) -> Self {
        Self { cas }
    }
}

#[derive(Debug, Deserialize)]
struct Input {
    /// 占位符 `原文: cas://<hash>` 里的那串 SHA-256 hex。
    hash: String,
    /// 可选的字节偏移，用于分页取回超长结果。
    #[serde(default)]
    offset: Option<usize>,
    /// 可选的最大返回字节数，默认 64 KiB。
    #[serde(default)]
    max_bytes: Option<usize>,
}

const DEFAULT_MAX_BYTES: usize = 64 * 1024;

#[async_trait]
impl Tool for ContextRecallTool {
    fn name(&self) -> &str {
        "ContextRecall"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: crate::descriptions::context_recall_description(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "hash": {
                        "type": "string",
                        "description": "The SHA-256 hash shown after 'cas://' in a [context-edited] placeholder"
                    },
                    "offset": {"type": "integer", "minimum": 0, "description": "byte offset to start reading from, for paginating long results"},
                    "max_bytes": {"type": "integer", "minimum": 1, "description": "max bytes to return (default 65536)"}
                },
                "required": ["hash"]
            }),
            native: None,
        }
    }

    async fn run(&self, input: Value, _ctx: &dyn ToolContext) -> Result<ToolResult> {
        let input: Input = serde_json::from_value(input)?;
        let hash = input.hash.trim();
        // 只接受 hex，避免模型传进路径分隔符之类的东西做目录穿越。
        let looks_valid =
            (32..=64).contains(&hash.len()) && hash.chars().all(|c| c.is_ascii_hexdigit());
        if !looks_valid {
            return Ok(ToolResult::err(
                "hash 必须是 32-64 位十六进制 SHA-256。占位符里 'cas://' 后面那一串。",
            ));
        }

        let full = match wyj_core::context_edit::recall_elided(&self.cas, hash) {
            Ok(text) => text,
            Err(error) => {
                // blob 可能已被 gc（会话早已结束）。告诉模型"取不回来，去重跑工具"，
                // 而不是让它把一次失败的召回当成"内容本来就不存在"。
                return Ok(ToolResult::err(format!(
                    "取回失败（{hash}）：{error}\n该内容可能已被清理回收。若确实需要，请重新执行产生它的工具。"
                )));
            }
        };

        let offset = input.offset.unwrap_or(0);
        let max_bytes = input.max_bytes.unwrap_or(DEFAULT_MAX_BYTES).max(1);
        // 字节切分必须回退到 char boundary，否则 CJK/emoji 会被切坏。
        let start = std::cmp::min(offset, full.len());
        let start = wyj_core::textutil::floor_char_boundary(&full, start);
        let end = std::cmp::min(start.saturating_add(max_bytes), full.len());
        let end = wyj_core::textutil::floor_char_boundary(&full, end);

        let slice = &full[start..end];
        let header = if end < full.len() {
            format!(
                "[{hash} 字节 {start}-{end} / 共 {} 字节；取后半段请传 offset={end}]\n",
                full.len()
            )
        } else {
            format!("[{hash} 全文 {} 字节]\n", full.len())
        };
        Ok(ToolResult::ok(format!("{header}{slice}")))
    }

    /// 只读、按 hash 取回，不碰工作区任何文件 —— 与 Read 不同，它不发起
    /// 任何外部动作，因此不需要权限确认。`needs_permission` 保持默认 false。
    fn parallel_safe(&self) -> bool {
        true
    }

    fn action_summary(&self, _input: &Value) -> String {
        "取回被上下文清理掉的工具输出".to_string()
    }
}

/// 供 CLI / 子 Agent 复用的构造：CAS 不可用时返回 `None`（该机制整体关闭）。
pub fn tool_for_cas(cas: Option<Arc<WorkspaceCas>>) -> Option<ContextRecallTool> {
    cas.map(ContextRecallTool::new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wyj_api::types::{ContentBlock, Message, ToolResultContent};
    use wyj_core::context_edit::{elide_tool_results, ElideOptions};
    use wyj_core::session::Session;

    struct NullCtx;
    #[async_trait::async_trait]
    impl ToolContext for NullCtx {
        fn cwd(&self) -> &std::path::Path {
            std::path::Path::new(".")
        }
        fn is_allowed(&self, _name: &str, _input: &Value) -> bool {
            true
        }
    }

    fn build() -> (tempfile::TempDir, ContextRecallTool, String) {
        let dir = tempfile::tempdir().expect("tempdir");
        let cas = Arc::new(WorkspaceCas::open(dir.path(), 16 * 1024 * 1024).expect("cas"));
        let mut session = Session::new();
        for i in 0..6 {
            session.messages.push(Message {
                role: wyj_api::types::Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: format!("t{i}"),
                    name: "Bash".into(),
                    input: serde_json::json!({}),
                }],
            });
            let payload = format!("line {i}\n").repeat(500);
            session.messages.push(Message {
                role: wyj_api::types::Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: format!("t{i}"),
                    content: ToolResultContent::text(&payload),
                    is_error: false,
                }],
            });
        }
        let stats = elide_tool_results(&mut session, &cas, &ElideOptions::default());
        assert_eq!(stats.elided, 3);
        (dir, ContextRecallTool::new(cas), stats.blobs[0].clone())
    }

    #[tokio::test]
    async fn recalls_full_content_by_hash() {
        let (_dir, tool, hash) = build();
        let out = tool
            .run(serde_json::json!({ "hash": hash }), &NullCtx)
            .await
            .expect("run");
        let text = out.content;
        assert!(text.contains("line 0"), "应取回原文: {text}");
    }

    /// 分页：超长结果不能一次全塞回上下文，否则「召回」本身又变成新的膨胀源。
    #[tokio::test]
    async fn paginates_with_offset_and_max_bytes() {
        let (_dir, tool, hash) = build();
        let out = tool
            .run(
                serde_json::json!({ "hash": hash, "offset": 0, "max_bytes": 100 }),
                &NullCtx,
            )
            .await
            .expect("run");
        let text = out.content;
        assert!(text.contains("offset="), "应提示如何取后半段: {text}");
    }

    /// hash 直接进 CAS 路径，必须挡住路径穿越。
    #[tokio::test]
    async fn rejects_non_hex_hash() {
        let (_dir, tool, _) = build();
        let out = tool
            .run(
                serde_json::json!({ "hash": "../../../etc/passwd" }),
                &NullCtx,
            )
            .await
            .expect("run");
        assert!(out.content.contains("十六进制"));
    }

    /// 取不回来时必须明确说"去重跑工具"，不能让模型把失败当成"内容本来就没有"。
    #[tokio::test]
    async fn missing_blob_tells_model_to_rerun_the_tool() {
        let (_dir, tool, _) = build();
        let out = tool
            .run(serde_json::json!({ "hash": "a".repeat(64) }), &NullCtx)
            .await
            .expect("run");
        assert!(out.content.contains("重新执行"));
    }
}
