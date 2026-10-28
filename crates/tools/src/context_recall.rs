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

/// 压缩归档是「被压段全部消息」的 JSON 数组，比一条工具结果大得多：
/// 默认给 64 KiB 时，一次真实压缩（几十条消息）永远只能看到开头一小段，
/// 模型得手动猜 offset 才能翻到想要的那一轮。这里放宽到 128 KiB，
/// 仍远小于一次工具输出上限（Bash 30K 字符 / WebFetch 50K 字符）。
const DEFAULT_ARCHIVE_MAX_BYTES: usize = 128 * 1024;

/// 归档命中时返回人类可读的转录文本，而不是原始 JSON。
///
/// 归档存的是 `serde_json::to_vec(&to_compact)`（消息数组），直接吐 JSON 会
/// 让模型浪费大量 token 读结构符号。这里按 `role` + 文本内容还原成近似对话
/// 记录；解析失败时原样退回 JSON —— 归档格式是内部的，未来变体不该让召回
/// 整个失效。
fn render_archive(bytes: &[u8]) -> String {
    let Ok(messages) = serde_json::from_slice::<Vec<wyj_api::types::Message>>(bytes) else {
        return String::from_utf8_lossy(bytes).into_owned();
    };
    let mut out = String::new();
    for message in messages {
        let role = match message.role {
            wyj_api::types::Role::User => "user",
            wyj_api::types::Role::Assistant => "assistant",
        };
        for block in message.content {
            match block {
                wyj_api::types::ContentBlock::Text { text } => {
                    out.push_str(&format!("[{role}]\n{text}\n\n"));
                }
                wyj_api::types::ContentBlock::ToolUse { name, input, .. } => {
                    out.push_str(&format!("[{role} → tool call] {name} {input}\n\n"));
                }
                wyj_api::types::ContentBlock::ToolResult {
                    content, is_error, ..
                } => {
                    let marker = if is_error {
                        "[tool result · error]"
                    } else {
                        "[tool result]"
                    };
                    let body = match content {
                        wyj_api::types::ToolResultContent::Text(text) => text,
                        wyj_api::types::ToolResultContent::Parts(parts) => parts
                            .iter()
                            .map(|part| match part {
                                wyj_api::types::ToolResultPart::Text { text } => text.clone(),
                                wyj_api::types::ToolResultPart::Image { .. } => "[image]".into(),
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                        // 旧会话文件里的原始 JSON 透传形态（见 `ToolResultContent`
                        // 的文档）。渲染成紧凑 JSON 而不是丢弃：归档的价值恰恰
                        // 是「原文还在」，把这一段吞掉等于局部失忆。
                        wyj_api::types::ToolResultContent::Blocks(blocks) => {
                            serde_json::to_string(&blocks).unwrap_or_default()
                        }
                    };
                    out.push_str(&format!("{marker}\n{body}\n\n"));
                }
                wyj_api::types::ContentBlock::Image { .. } => {
                    out.push_str(&format!("[{role} · image]\n\n"));
                }
                _ => {}
            }
        }
    }
    if out.trim().is_empty() {
        // 纯 thinking / 无法渲染的块：宁可回退到原始 JSON，也不要返回空串
        // 让模型以为归档是空的。
        return String::from_utf8_lossy(bytes).into_owned();
    }
    out
}

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
                        "description": "The SHA-256 hash shown after 'cas://' — either in a [context-edited] placeholder or in the '## Compacted transcript archive' section of a compaction summary"
                    },
                    "offset": {"type": "integer", "minimum": 0, "description": "byte offset to start reading from, for paginating long results"},
                    "max_bytes": {"type": "integer", "minimum": 1, "description": "max bytes to return (default 65536, or 131072 for a compaction archive)"}
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

        let raw = match self.cas.get(hash) {
            Ok(bytes) => bytes,
            Err(error) => {
                // blob 可能已被 gc（会话早已结束）。告诉模型"取不回来，去重跑工具"，
                // 而不是让它把一次失败的召回当成"内容本来就不存在"。
                return Ok(ToolResult::err(format!(
                    "取回失败（{hash}）：{error}\n该内容可能已被清理回收。若确实需要，请重新执行产生它的工具。"
                )));
            }
        };

        // 同一个 CAS 池里混着两类 blob：被外置的单条工具结果（纯文本），
        // 和压缩归档（`Vec<Message>` 的 JSON）。先按消息数组试解析一次，
        // 成功就还原成对话记录——直接吐 JSON 会让模型白读大量结构符号。
        let is_archive = serde_json::from_slice::<Vec<wyj_api::types::Message>>(&raw).is_ok();
        let full = if is_archive {
            render_archive(&raw)
        } else {
            String::from_utf8_lossy(&raw).into_owned()
        };

        let offset = input.offset.unwrap_or(0);
        let default_max = if is_archive {
            DEFAULT_ARCHIVE_MAX_BYTES
        } else {
            DEFAULT_MAX_BYTES
        };
        let max_bytes = input.max_bytes.unwrap_or(default_max).max(1);
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

    /// 压缩归档与工具结果共用同一个 CAS 池，但内容是 `Vec<Message>` 的 JSON。
    /// 召回时必须还原成可读转录，而不是把 JSON 结构符号原样塞回上下文。
    #[tokio::test]
    async fn recalls_compaction_archive_as_readable_transcript() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cas = Arc::new(WorkspaceCas::open(dir.path(), 16 * 1024 * 1024).expect("cas"));
        let archived = vec![
            Message {
                role: wyj_api::types::Role::User,
                content: vec![ContentBlock::Text {
                    text: "把 compact 的保留预算改大".into(),
                }],
            },
            Message {
                role: wyj_api::types::Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "Read".into(),
                    input: serde_json::json!({ "file_path": "crates/core/src/compact.rs" }),
                }],
            },
            Message {
                role: wyj_api::types::Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: ToolResultContent::text("fn compact_keep_budget(..) { .. }"),
                    is_error: false,
                }],
            },
        ];
        let hash = cas
            .intern(&serde_json::to_vec(&archived).expect("serialize"))
            .expect("intern");
        let tool = ContextRecallTool::new(cas);

        let out = tool
            .run(serde_json::json!({ "hash": hash }), &NullCtx)
            .await
            .expect("run");
        let text = out.content;
        assert!(
            text.contains("把 compact 的保留预算改大"),
            "应还原用户原文: {text}"
        );
        assert!(
            text.contains("compact_keep_budget"),
            "应还原工具结果: {text}"
        );
        assert!(
            text.contains("[assistant → tool call] Read"),
            "应标出工具调用: {text}"
        );
        assert!(
            !text.contains("{\"role\""),
            "不应把原始 JSON 结构吐给模型: {text}"
        );
    }
}
