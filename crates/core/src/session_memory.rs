//! Session memory —— 会话内滚动工作文档。
//!
//! # 它解决什么问题
//!
//! 压缩是全链路唯一不可逆的信息损失点，而当前实现是**一次性全量摘要**：
//! 撞到阈值时把整段历史（可能是 900K token）读一遍，产出一份 16K 摘要。
//! 两个后果：
//!
//! 1. **成本随历史长度线性增长**，而压缩往往要跑好几轮；
//! 2. **摘要的摘要逐代降质**——上一次的摘要必然落在下一次的 `to_compact`
//!    段里，精确数值、否定知识、先后因果最先死。
//!
//! Session memory 把这件事摊平：每 N 轮用一次**增量**调用维护一份滚动文档，
//! 压缩时直接 splice 它，而不是再付一次全量往返。原始信息因此只被摘要一次。
//!
//! # 与压缩摘要的关系
//!
//! 文档沿用压缩摘要的**同一套章节**（见 [`crate::prompts::compact_instructions`]），
//! 所以 splice 出来的结果与真实压缩摘要同构——模型不需要区分「这是滚动文档
//! 还是压缩产物」，`summary_generation` 也能照常沿用。
//!
//! # 为什么不做成 Memory v3 的 claim
//!
//! claim 是**跨会话**的、结构化的、可有 TTL 的事实；滚动文档是**本会话**
//! 「做到哪了」的连续叙述，生命周期与 session 绑定、压缩后即被消费。两者
//! 的淘汰策略、作用域语义、注入位置都不同，混进同一个存储会让
//! `project_brief` 的 8KB 预算挤掉真正长期有效的事实。
//!
//! # 失败模式（都按「退化为旧行为」处理）
//!
//!   - 写盘失败 → 下次 `update` 重试；
//!   - 更新调用失败 → 保留上一版文档（旧文档比没有好，但绝不能把「更新失败」
//!     写成空文档）；
//!   - 文档为空或过短 → 压缩时**不**用它，回退到全量摘要。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use wyj_api::provider::Provider;
use wyj_api::types::Message;

use crate::session::Session;

/// 滚动文档文件名的**前缀**（完整名 = `<prefix>-<session_id>.md`）。
///
/// 必须带 session_id：文档目录是**按项目**分的，同一项目下并行着多个会话，
/// 用固定文件名会让它们互相覆盖——后写的会话抹掉先写会话的现场，而先写的
/// 会话下次压缩时读到的是**别人的**文档。
pub const SESSION_MEMORY_FILENAME_PREFIX: &str = "session_memory";

/// 文档的最小可用长度（字节）。
///
/// 低于它说明还没真正积累出内容（首轮刚初始化、或模型返回了一句空话）。
/// 压缩侧据此决定「直接 splice」还是「退回全量摘要」——一份 200 字节的
/// 「文档」当摘要用，比没有摘要更糟。
pub const MIN_USABLE_BYTES: usize = 400;

/// 文档的硬上限（字节）。
///
/// 滚动文档的价值在于「小到能整段塞进上下文」。超过这个尺寸它自己就成了
/// 上下文问题的一部分，所以更新时要求模型压缩已有内容而不是持续追加。
pub const MAX_DOC_BYTES: usize = 12_000;

/// 一次更新的输入：上一版文档 + 新增的这一段对话。
#[derive(Debug, Clone)]
pub struct UpdateRequest<'a> {
    pub previous: &'a str,
    /// 自上次更新以来新增的消息（**不是**全量历史）。
    pub new_messages: &'a [Message],
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionMemoryState {
    /// 已覆盖到的消息条数。下次更新只喂 `messages[covered..]`。
    #[serde(default)]
    pub covered_messages: usize,
    /// 上次成功更新的时间戳（RFC3339）。
    #[serde(default)]
    pub updated_at: Option<String>,
    /// 已更新过多少次（诊断用：`/context` 可据此区分「没开」与「开了但没跑」）。
    #[serde(default)]
    pub update_count: u32,
}

/// 滚动工作文档的存储与更新器。
///
/// `session_id` 是**可变**的：TUI 在同一进程里会切换会话（`/resume`、
/// `/new`、`/branch`），而文档是按会话存的。固定成启动时的 id 会让新会话
/// 继承上一个会话的现场，且 `covered_messages` 对不上——增量会算错，
/// 文档会混进别的会话的内容。
#[derive(Debug, Clone)]
pub struct SessionMemory {
    dir: PathBuf,
    session_id: std::sync::Arc<std::sync::RwLock<String>>,
    /// 辅助往返用量累加器。`update()` 是后台 `tokio::spawn` 出来的，拿不到
    /// `&mut Session`，只能靠共享 sink 记账——文档更新每 4 轮一次，长会话
    /// 几十次，不记就是 `/cost` 上的系统性低估。
    aux_usage: Option<crate::aux_usage::SharedAuxUsage>,
}

impl SessionMemory {
    pub fn new(dir: impl Into<PathBuf>, session_id: impl Into<String>) -> Self {
        Self {
            dir: dir.into(),
            session_id: std::sync::Arc::new(std::sync::RwLock::new(session_id.into())),
            aux_usage: None,
        }
    }

    /// 注入辅助用量累加器（装配期调用）。
    pub fn with_aux_usage(mut self, sink: Option<crate::aux_usage::SharedAuxUsage>) -> Self {
        self.aux_usage = sink;
        self
    }

    fn current_id(&self) -> String {
        self.session_id
            .read()
            .map(|id| id.clone())
            .unwrap_or_default()
    }

    fn doc_path(&self) -> PathBuf {
        self.dir.join(format!(
            "{SESSION_MEMORY_FILENAME_PREFIX}-{}.md",
            self.current_id()
        ))
    }

    fn state_path(&self) -> PathBuf {
        self.dir.join(format!(
            "{SESSION_MEMORY_FILENAME_PREFIX}-{}.json",
            self.current_id()
        ))
    }

    /// 读出当前文档。不存在（还没跑过更新）时返回空串。
    ///
    /// 不把「文件不存在」当成错误：新会话的第一轮就处在这个状态，
    /// 调用方据此判断「还没有可 splice 的内容」。
    pub fn read_doc(&self) -> String {
        std::fs::read_to_string(self.doc_path()).unwrap_or_default()
    }

    pub fn read_state(&self) -> SessionMemoryState {
        std::fs::read_to_string(self.state_path())
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    /// 文档是否足够完整到能替代一次全量摘要。
    pub fn is_usable(&self) -> bool {
        let doc = self.read_doc();
        doc.len() >= MIN_USABLE_BYTES && !doc.trim().is_empty()
    }

    /// 自上次更新以来新增了多少条消息。
    pub fn pending_messages(&self, session: &Session) -> usize {
        session
            .messages
            .len()
            .saturating_sub(self.read_state().covered_messages)
    }

    /// 文档是否已经覆盖到 `message_count` 之前的历史。
    ///
    /// 压缩侧用它做**硬门禁**：只有文档覆盖了即将被丢弃的那一段，才能拿它
    /// 当摘要。否则 splice 出来的摘要里缺的正是马上被删掉的内容——那是一次
    /// 静默的对话丢失，比多跑一次全量往返昂贵得多。
    pub fn covers(&self, message_count: usize) -> bool {
        self.read_state().covered_messages >= message_count
    }

    /// 历史被整体改写（压缩 / 截断）后，把覆盖指针清零。
    ///
    /// `covered_messages` 是**消息下标**，历史长度骤变后它就失去意义了：
    /// 不清零会让 `update()` 算出 `covered.min(len) == len`，增量恒空，
    /// 文档从此永不更新；等历史重新长回旧长度，中间那一整段也永远进不了
    /// 文档。清零的代价只是下一次更新全量重读一遍（此时历史已被压短，
    /// 成本可以忽略）。
    pub fn reset_coverage(&self) {
        let state = self.read_state();
        let next = SessionMemoryState {
            covered_messages: 0,
            updated_at: state.updated_at,
            update_count: state.update_count,
        };
        if std::fs::create_dir_all(&self.dir).is_err() {
            return;
        }
        if let Err(error) = crate::serialize::write_json_atomic(&self.state_path(), &next) {
            tracing::debug!("重置 session memory 覆盖指针失败: {error}");
        }
    }

    /// 用增量方式更新文档。
    ///
    /// 只把 `messages[covered..]` 喂给模型，并要求它**就地修订**上一版文档
    /// （而不是追加）。这是与「全量摘要」的根本差别：单次成本只与增量有关，
    /// 而文档本身稳定在 `MAX_DOC_BYTES` 以内。
    ///
    /// 失败时保留旧文档：一份略旧的现场远比空文档有用，而写盘失败会让
    /// `covered_messages` 不前进，下次自动重试同一段增量。
    pub async fn update(
        &self,
        session: &Session,
        provider: &dyn Provider,
        max_tokens: u32,
    ) -> Result<bool> {
        let state = self.read_state();
        let covered = state.covered_messages.min(session.messages.len());
        let new_messages = &session.messages[covered..];
        if new_messages.is_empty() {
            return Ok(false);
        }
        let previous = self.read_doc();
        let transcript = messages_to_transcript(new_messages);
        if transcript.trim().is_empty() {
            return Ok(false);
        }

        let prompt = update_prompt(&previous, &transcript);
        let result = provider
            .complete(
                &wyj_api::SystemPrompt::stable_only(crate::prompts::SESSION_MEMORY_SYSTEM),
                &[Message::user(prompt)],
                &[],
                &wyj_api::provider::RequestOptions::text_only(max_tokens),
            )
            .await
            .context("session memory 更新往返失败")?;

        // 记账必须放在检查内容之前：走到这里说明往返已经完成、token 已经
        // 花掉，哪怕下面因为空输出而失败也照样要计。与 `summary.rs`、
        // `evolution.rs` 的记账时机同一条纪律。
        if let Some(sink) = self.aux_usage.as_ref() {
            sink.record_result(&result);
        }

        let doc: String = result
            .content
            .iter()
            .filter_map(|block| match block {
                wyj_api::types::ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let doc = doc.trim();
        if doc.is_empty() {
            // 空输出绝不能落盘：它会把一份可用的旧文档覆盖成空，
            // 而下面的 `covered_messages` 又会前进——现场永久丢失。
            anyhow::bail!("session memory 更新返回空内容");
        }
        let doc = truncate_to_bytes(doc, MAX_DOC_BYTES);

        if let Err(error) = std::fs::create_dir_all(&self.dir) {
            anyhow::bail!("session memory 目录不可写: {error}");
        }
        crate::serialize::write_atomic(&self.doc_path(), doc.as_bytes())
            .context("写入 session memory 文档")?;
        let next = SessionMemoryState {
            covered_messages: session.messages.len(),
            updated_at: Some(Utc::now().to_rfc3339()),
            update_count: state.update_count.saturating_add(1),
        };
        crate::serialize::write_json_atomic(&self.state_path(), &next)
            .context("写入 session memory 状态")?;
        Ok(true)
    }

    /// 压缩时取用：返回可直接充当摘要正文的文档。
    ///
    /// 只在文档足够完整时返回 `Some`。**不**在这里删除文档——压缩之后
    /// 对话还在继续，滚动文档应当继续累积（`covered_messages` 已经覆盖到
    /// 压缩前的全部消息，所以压缩产生的那条摘要消息会被算作「新消息」，
    /// 下次更新时会把它揉进文档）。
    pub fn summary_text(&self) -> Option<String> {
        if !self.is_usable() {
            return None;
        }
        let doc = self.read_doc();
        let doc = doc.trim().to_string();
        (!doc.is_empty()).then_some(doc)
    }

    /// 会话结束后清理。滚动文档只在会话存活期间有意义——
    /// 留着它既占空间，又会在下次同名会话里被当成「上一版」喂给模型。
    pub fn clear(&self) {
        let _ = std::fs::remove_file(self.doc_path());
        let _ = std::fs::remove_file(self.state_path());
    }

    /// 切换到另一个会话（`/resume`、`/new`、`/branch` 之后）。
    ///
    /// 文档是**按会话**存的，所以换会话必须换目标文件；沿用旧文档会让新会话
    /// 继承上一个会话的现场（而 `covered_messages` 也对不上，增量会算错）。
    pub fn retarget(&self, session_id: &str) {
        if let Ok(mut current) = self.session_id.write() {
            if current.as_str() != session_id {
                *current = session_id.to_string();
            }
        }
    }

    pub fn session_id(&self) -> String {
        self.current_id()
    }
}

/// 把一段消息渲染成更新用的转录文本。
///
/// 与 `compact::messages_to_text` 的取捨不同：那边是**为了丢弃**（越短越好），
/// 这边是**为了吸收**（要留住工具名、路径、错误信息）。所以截断更宽，
/// 且工具调用保留参数名而不丢弃整段。
pub fn messages_to_transcript(messages: &[Message]) -> String {
    use wyj_api::types::{ContentBlock, Role, ToolResultContent};
    const TEXT_LIMIT: usize = 2_000;
    const TOOL_LIMIT: usize = 1_200;
    let mut out = String::new();
    for message in messages {
        let role = match message.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        for block in &message.content {
            match block {
                ContentBlock::Text { text } => {
                    // 规约注入的碎片不进滚动文档：它们是**每轮重新注入**的，
                    // 记进来只会让文档越滚越大，而信息本就在 system 里。
                    if text.contains("<system-reminder>") {
                        continue;
                    }
                    out.push_str(&format!("[{role}] {}\n", truncate_chars(text, TEXT_LIMIT)));
                }
                ContentBlock::ToolUse { name, input, .. } => {
                    let args = truncate_chars(&input.to_string(), TOOL_LIMIT);
                    out.push_str(&format!("[tool_call] {name} {args}\n"));
                }
                ContentBlock::ToolResult {
                    content, is_error, ..
                } => {
                    let marker = if *is_error {
                        "[tool_error]"
                    } else {
                        "[tool_result]"
                    };
                    let body = match content {
                        ToolResultContent::Text(text) => text.clone(),
                        ToolResultContent::Parts(parts) => parts
                            .iter()
                            .map(|part| match part {
                                wyj_api::types::ToolResultPart::Text { text } => text.clone(),
                                wyj_api::types::ToolResultPart::Image { .. } => "[image]".into(),
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                        ToolResultContent::Blocks(blocks) => {
                            serde_json::to_string(blocks).unwrap_or_default()
                        }
                    };
                    out.push_str(&format!("{marker} {}\n", truncate_chars(&body, TOOL_LIMIT)));
                }
                ContentBlock::Thinking { thinking, .. } => {
                    // 思考链是「为什么这么改」的唯一来源，但也很长：
                    // 只留开头，够模型记起当时的判断路径即可。
                    out.push_str(&format!(
                        "[thinking] {}\n",
                        truncate_chars(thinking, TOOL_LIMIT)
                    ));
                }
                ContentBlock::Image { .. } => out.push_str("[image]\n"),
                ContentBlock::RedactedThinking { .. } => {}
            }
        }
    }
    out
}

fn update_prompt(previous: &str, transcript: &str) -> String {
    crate::prompts::session_memory_prompt(previous, transcript)
}

/// 按**字符**截断（不是字节），并在末尾标注。
fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let head: String = text.chars().take(max_chars).collect();
    format!("{head}…[truncated]")
}

/// 按**字节**截断到字符边界，用于文档总长封顶。
fn truncate_to_bytes(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let cut = crate::textutil::floor_char_boundary(text, max_bytes);
    format!("{}\n…[session memory truncated]", &text[..cut])
}

/// 判断这次回合该不该更新滚动文档。
///
/// 频率取 [`crate::agent::SESSION_MEMORY_EVERY_TURNS`]，与记忆提取错开：
/// 两者都是辅助 LLM 往返，撞在同一轮会让主循环多等一趟。
pub fn update_due(user_turns: usize, every: usize) -> bool {
    every > 0 && user_turns > 0 && user_turns % every == 0
}

/// 清理某个会话残留的滚动文档（`storage prune` / 会话删除时用）。
pub fn purge_session_memory(dir: &Path, session_id: &str) {
    let _ =
        std::fs::remove_file(dir.join(format!("{SESSION_MEMORY_FILENAME_PREFIX}-{session_id}.md")));
    let _ = std::fs::remove_file(dir.join(format!(
        "{SESSION_MEMORY_FILENAME_PREFIX}-{session_id}.json"
    )));
}

#[cfg(test)]
mod tests {
    use super::*;
    use wyj_api::provider::EventStream;
    use wyj_api::types::{
        CompletionResult, ContentBlock, StopReason, ToolDefinition, ToolResultContent,
    };

    struct FixedProvider(String);
    #[async_trait::async_trait]
    impl Provider for FixedProvider {
        async fn stream(
            &self,
            _system: &wyj_api::SystemPrompt<'_>,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            _opts: &wyj_api::provider::RequestOptions,
        ) -> Result<EventStream> {
            Ok(Box::pin(futures::stream::empty()))
        }

        async fn complete(
            &self,
            _system: &wyj_api::SystemPrompt<'_>,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            _opts: &wyj_api::provider::RequestOptions,
        ) -> Result<CompletionResult> {
            Ok(CompletionResult {
                content: vec![ContentBlock::Text {
                    text: self.0.clone(),
                }],
                stop_reason: StopReason::EndTurn,
                input_tokens: 10,
                output_tokens: 10,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
                reasoning_tokens: 0,
            })
        }
    }

    fn session_with(messages: Vec<Message>) -> Session {
        let mut session = Session::new();
        session.messages = messages;
        session
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wyj-sessmem-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 核心场景：首轮更新把文档写出来，第二轮只喂增量。
    ///
    /// 「只喂增量」是它比全量摘要便宜的全部原因——这条断言如果退化成
    /// 全量，成本优势就没了，而功能看起来毫无变化。
    #[tokio::test]
    async fn update_feeds_only_the_new_messages() {
        let dir = temp_dir("incremental");
        let memory = SessionMemory::new(&dir, "s1");
        let session = session_with(vec![
            Message::user("第一条需求"),
            Message::assistant_text("第一轮回应"),
        ]);
        let provider = FixedProvider("## Task & Intent\n做某件事。".repeat(30));
        assert!(memory.update(&session, &provider, 512).await.unwrap());
        assert_eq!(memory.read_state().covered_messages, 2);

        // 第二轮：worker 只应看到新增的那两条。
        struct RecordingProvider(std::sync::Mutex<Vec<String>>);
        #[async_trait::async_trait]
        impl Provider for RecordingProvider {
            async fn stream(
                &self,
                _system: &wyj_api::SystemPrompt<'_>,
                _messages: &[Message],
                _tools: &[ToolDefinition],
                _opts: &wyj_api::provider::RequestOptions,
            ) -> Result<EventStream> {
                Ok(Box::pin(futures::stream::empty()))
            }
            async fn complete(
                &self,
                _system: &wyj_api::SystemPrompt<'_>,
                messages: &[Message],
                _tools: &[ToolDefinition],
                _opts: &wyj_api::provider::RequestOptions,
            ) -> Result<CompletionResult> {
                let text = messages
                    .iter()
                    .map(|m| m.text())
                    .collect::<Vec<_>>()
                    .join("\n");
                self.0.lock().unwrap().push(text);
                Ok(CompletionResult {
                    content: vec![ContentBlock::Text {
                        text: "## Task & Intent\n更新后的文档。".repeat(30),
                    }],
                    stop_reason: StopReason::EndTurn,
                    input_tokens: 1,
                    output_tokens: 1,
                    cache_read_input_tokens: 0,
                    cache_creation_input_tokens: 0,
                    reasoning_tokens: 0,
                })
            }
        }
        let mut session = session;
        session.messages.push(Message::user("第二条需求"));
        session.messages.push(Message::assistant_text("第二轮回应"));
        let recorder = RecordingProvider(Default::default());
        memory.update(&session, &recorder, 512).await.unwrap();
        let seen = recorder.0.lock().unwrap()[0].clone();
        assert!(seen.contains("第二条需求"), "应包含新增消息: {seen}");
        assert!(
            !seen.contains("第一条需求"),
            "不应重发已覆盖的消息（那就是全量摘要了）: {seen}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 空输出绝不能覆盖旧文档：那会让 `covered_messages` 前进而现场丢失。
    #[tokio::test]
    async fn empty_model_output_never_overwrites_a_good_document() {
        let dir = temp_dir("empty");
        let memory = SessionMemory::new(&dir, "s1");
        let session = session_with(vec![Message::user("需求")]);
        let good = FixedProvider("## Current State\n一切正常。".repeat(40));
        memory.update(&session, &good, 512).await.unwrap();
        let before = memory.read_doc();
        assert!(!before.is_empty());

        let mut session = session;
        session.messages.push(Message::user("又一条"));
        let empty = FixedProvider("   \n  ".to_string());
        assert!(memory.update(&session, &empty, 512).await.is_err());
        assert_eq!(memory.read_doc(), before, "旧文档必须原样保留");
        assert_eq!(
            memory.read_state().covered_messages,
            1,
            "失败时覆盖进度不前进，下次重试同一段增量"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 文档太短时不能当摘要用——一句「做某件事」比没有摘要更误导。
    #[test]
    fn a_too_short_document_is_not_usable_as_a_summary() {
        let dir = temp_dir("short");
        let memory = SessionMemory::new(&dir, "s1");
        std::fs::write(memory.doc_path(), "## Task & Intent\n做事").unwrap();
        assert!(!memory.is_usable());
        assert_eq!(memory.summary_text(), None);

        std::fs::write(
            memory.doc_path(),
            "## Task & Intent\n".to_string() + &"细节。".repeat(200),
        )
        .unwrap();
        assert!(memory.is_usable());
        assert!(memory.summary_text().unwrap().contains("细节"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 转录要留住工具名、路径与错误信息——这些正是压缩最容易杀掉的东西。
    #[test]
    fn transcript_keeps_tool_names_paths_and_errors() {
        let messages = vec![
            Message {
                role: wyj_api::types::Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "Read".into(),
                    input: serde_json::json!({"file_path": "crates/core/src/compact.rs"}),
                }],
            },
            Message {
                role: wyj_api::types::Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: ToolResultContent::text("error[E0308]: mismatched types"),
                    is_error: true,
                }],
            },
        ];
        let text = messages_to_transcript(&messages);
        assert!(text.contains("Read"), "{text}");
        assert!(text.contains("crates/core/src/compact.rs"), "{text}");
        assert!(text.contains("[tool_error]"), "{text}");
        assert!(text.contains("E0308"), "{text}");
    }

    /// 规约注入的碎片不进文档：它们每轮都会重新注入，记进来只会让文档膨胀。
    #[test]
    fn transcript_skips_system_reminder_fragments() {
        let messages = vec![Message::user(
            "<system-reminder>这是每轮重注入的规约</system-reminder>",
        )];
        assert!(messages_to_transcript(&messages).trim().is_empty());
    }

    /// 文档总长有硬上限：它自己不能变成新的上下文问题。
    #[test]
    fn document_is_capped_and_stays_on_a_char_boundary() {
        let dir = temp_dir("cap");
        let huge = "中文内容。".repeat(10_000);
        let capped = truncate_to_bytes(&huge, MAX_DOC_BYTES);
        assert!(capped.len() <= MAX_DOC_BYTES + 64);
        assert!(capped.ends_with("[session memory truncated]"));
        // 能正常当 &str 用就说明没切在字符中间。
        assert!(capped.chars().count() > 100);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn update_due_fires_on_a_fixed_cadence() {
        assert!(!update_due(0, 5));
        assert!(!update_due(1, 5));
        assert!(update_due(5, 5));
        assert!(update_due(10, 5));
        assert!(!update_due(5, 0), "every = 0 表示关闭");
    }
}
