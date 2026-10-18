//! 子 Agent 交接物 — 中断/触顶/失败时留给主 Agent 的「做到了哪里」。
//!
//! # 为什么是交接物而不是会话恢复
//!
//! 恢复（resume）要重放整段子 Agent 历史，那与「子 Agent 存在的理由」
//! ——把噪音挡在主上下文之外——直接矛盾：resume 不是把它捞回来，而是让它
//! 继续膨胀，只是换了个名字。
//!
//! 交接物走另一条路：**落盘全量，注入摘要**。原始读取内容留在 trace 里
//! （可再生、可能已过期、体积大），交给主 Agent 的是**不可再生**的那部分
//! ——结论、已排除的路径、改了哪些文件、卡在哪。
//!
//! # 为什么必须包含 assistant 文本
//!
//! [`crate::trace::TraceEvent`] 的 `ToolStart`/`ToolEnd` 只说明「它读了什么」，
//! 说明不了「它由此得出了什么」。读到的原文可以重读一遍，但一条推导出来的
//! 结论无法从原始输入重新推导。所以交接物的第一现场是 assistant 文本，工具
//! 清单只是从 trace 机械提取的佐证。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::trace::TraceEvent;

/// 交接原因。决定注入给主 Agent 的措辞与「能不能直接续」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffStatus {
    /// 正常完成——不需要交接，写出来只为让面板与审计看到同一份终态。
    Completed,
    /// 用户 ESC / `/agent-control interrupt` 中断。
    Interrupted,
    /// 推理轮数触顶（`max_turns`）。
    TurnLimit,
    /// 运行时错误（provider 失败、工具层 panic 等）。
    Failed,
}

impl HandoffStatus {
    /// 这个终态是否意味着「任务没做完、存在可继续的剩余工作」。
    pub fn is_incomplete(self) -> bool {
        !matches!(self, Self::Completed)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Interrupted => "interrupted",
            Self::TurnLimit => "turn_limit",
            Self::Failed => "failed",
        }
    }
}

/// 从 trace 机械提取的工具摘要。
///
/// 全部可由既有 `TraceEvent` 推导，不需要子 Agent 配合，因此不增加模型的
/// 任何负担——它照常跑，摘要由 harness 生成。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDigest {
    /// 读过的文件（去重、保序、限量）
    pub files_read: Vec<String>,
    /// 改过的文件（Write/Edit 的目标）
    pub files_modified: Vec<String>,
    /// 执行过的命令（Bash 的 command 首行）
    pub commands: Vec<String>,
    /// 搜索模式（Grep/Glob 的 pattern）
    pub searches: Vec<String>,
    /// 失败的工具调用摘要（工具名 + 首行错误）
    pub errors: Vec<String>,
    /// 工具调用总次数
    pub total_calls: usize,
}

/// 列表条数上限：交接物是摘要，不是清单。超出部分只保留计数。
pub const DIGEST_LIST_LIMIT: usize = 30;
/// 单条命令/错误摘要的字符上限。
pub const DIGEST_ITEM_CHARS: usize = 120;

impl ToolDigest {
    /// 增量记录一次工具调用的输入（子 Agent 运行期实时调用）。
    ///
    /// 与 [`Self::from_trace`] 二选一：运行期用本方法，读盘复盘用那个。
    /// 两条入口共用同一份提取规则，避免「实时摘要」与「事后摘要」不一致。
    pub fn record_start(&mut self, tool_name: &str, input: &serde_json::Value) {
        self.total_calls += 1;
        let field = |key: &str| {
            input
                .get(key)
                .and_then(|v| v.as_str())
                .map(|s| truncate_chars(s, DIGEST_ITEM_CHARS))
        };
        match tool_name {
            "Read" => push_unique(&mut self.files_read, field("file_path")),
            "Write" | "Edit" => push_unique(&mut self.files_modified, field("file_path")),
            "Bash" => push_unique(&mut self.commands, field("command")),
            "Grep" | "Glob" => push_unique(&mut self.searches, field("pattern")),
            _ => {}
        }
    }

    /// 增量记录一次失败的工具调用（只取输出首行）。
    pub fn record_error(&mut self, tool_name: &str, output: &str) {
        let first = output.lines().next().unwrap_or_default();
        push_unique(
            &mut self.errors,
            Some(format!(
                "{tool_name}: {}",
                truncate_chars(first, DIGEST_ITEM_CHARS)
            )),
        );
    }

    /// 从一份完整 trace 事件序列提取摘要。
    ///
    /// 只看 `ToolStart`（拿到 name + input）与 `ToolEnd`（拿到 is_error +
    /// output 首行）。`ToolStart` 与 `ToolEnd` 由 Hub 保证 FIFO 成对，但为
    /// 稳妥起见错误摘要按工具名配对而不是按索引。
    pub fn from_trace(events: &[TraceEvent]) -> Self {
        let mut digest = ToolDigest::default();
        for ev in events {
            match ev {
                TraceEvent::ToolStart {
                    tool_name,
                    input_json,
                    ..
                } => {
                    if let Ok(input) = serde_json::from_str::<serde_json::Value>(input_json) {
                        digest.record_start(tool_name, &input);
                    } else {
                        digest.total_calls += 1;
                    }
                }
                TraceEvent::ToolEnd {
                    tool_name,
                    is_error: true,
                    output,
                    ..
                } => digest.record_error(tool_name, output),
                _ => {}
            }
        }
        digest
    }

    pub fn is_empty(&self) -> bool {
        self.total_calls == 0
    }
}

fn push_unique(target: &mut Vec<String>, value: Option<String>) {
    let Some(value) = value else { return };
    if value.trim().is_empty() || target.len() >= DIGEST_LIST_LIMIT {
        return;
    }
    if !target.iter().any(|existing| existing == &value) {
        target.push(value);
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    format!("{}…", s.chars().take(max).collect::<String>())
}

/// 运行期进度快照：既是交接物的素材，也是「任务被 abort 时唯一还活着的东西」。
///
/// 为什么单独抽出来：正常结束时任务自己会组装完整的 [`SubAgentHandoff`]，
/// 但 `interrupt()` / `abort_foreground()` 会 `JoinHandle::abort()` —— 那之后
/// 收尾代码一行都不会执行，`conclusion` 与 `digest` 只存在于这份共享快照里。
/// ESC 中断恰恰是最需要交接的场景，所以进度必须**边跑边记**，不能等收尾再算。
#[derive(Debug, Default)]
pub struct HandoffProgress {
    /// 最近一轮交出的 assistant 文本。
    pub conclusion: String,
    /// 实时累积的工具摘要（与运行期回调同步更新）。
    pub digest: ToolDigest,
}

impl HandoffProgress {
    pub fn record_turn_text(&mut self, text: &str) {
        if !text.trim().is_empty() {
            self.conclusion = text.to_string();
        }
    }

    pub fn record_tool_start(&mut self, tool_name: &str, input: &serde_json::Value) {
        self.digest.record_start(tool_name, input);
    }

    pub fn record_tool_error(&mut self, tool_name: &str, output: &str) {
        self.digest.record_error(tool_name, output);
    }
}

/// 落盘的交接物。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAgentHandoff {
    pub version: u32,
    pub id: u64,
    pub agent_type: String,
    pub description: String,
    pub background: bool,
    pub status: HandoffStatus,
    /// 中断/失败的**运行时**原因（如 provider 错误文本）。正常完成时为 None。
    #[serde(default)]
    pub reason: Option<String>,
    /// 最后一轮 assistant 文本——交接物里最有价值的部分。
    #[serde(default)]
    pub conclusion: String,
    #[serde(default)]
    pub digest: ToolDigest,
    pub elapsed_secs: f64,
}

/// 交接物版本号。结构变更时递增，读侧据此决定是否还有效。
pub const HANDOFF_VERSION: u32 = 1;
/// `conclusion` 落盘字节上限（超限保头）。
pub const CONCLUSION_MAX_BYTES: usize = 8_000;

/// 交接物文件路径：`<sessions_dir>/<session_id>.subagents/a<id>.handoff.json`。
///
/// 与 trace 同目录、不同后缀：既有的 `a<id>.jsonl` 解析完全不受影响，
/// `list_trace_ids` 也只会挑 `.jsonl`，不会把交接物当成一个子 Agent。
pub fn handoff_file(sessions_dir: &Path, session_id: &str, id: u64) -> PathBuf {
    crate::trace::trace_dir(sessions_dir, session_id).join(format!("a{id}.handoff.json"))
}

/// 写交接物。失败只记 warn——交接物是尽力而为的附加信息，绝不能让它的写入
/// 失败把一次正常的子 Agent 运行判成失败。
pub fn write_handoff(
    sessions_dir: &Path,
    session_id: &str,
    handoff: &SubAgentHandoff,
) -> std::io::Result<()> {
    let path = handoff_file(sessions_dir, session_id, handoff.id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(handoff)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    wyj_config::filelock::acquire(&path.with_extension("json.lock"), "subagent handoff")
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    wyj_core::serialize::write_atomic(&path, json.as_bytes())
        .map_err(|e| std::io::Error::other(e.to_string()))
}

/// 列出某会话下所有有交接物的子 Agent id（升序）。
///
/// 与 `trace::list_trace_ids` 分开：`trace_enabled = false` 时会话可能只有交接物
/// 没有 trace，此时面板与 CLI 仍需能列出这些 agent。
pub fn list_handoff_ids(sessions_dir: &Path, session_id: &str) -> Vec<u64> {
    let dir = crate::trace::trace_dir(sessions_dir, session_id);
    let mut ids: Vec<u64> = match std::fs::read_dir(&dir) {
        Ok(entries) => entries
            .flatten()
            .filter_map(|e| {
                let name = e.file_name();
                let name = name.to_str()?;
                name.strip_prefix('a')?
                    .strip_suffix(".handoff.json")?
                    .parse::<u64>()
                    .ok()
            })
            .collect(),
        Err(_) => vec![],
    };
    ids.sort_unstable();
    ids
}

/// 读交接物。文件不存在/损坏时返回 None（调用方据此提示「无交接记录」）。
pub fn read_handoff(sessions_dir: &Path, session_id: &str, id: u64) -> Option<SubAgentHandoff> {
    let path = handoff_file(sessions_dir, session_id, id);
    let text = std::fs::read_to_string(path).ok()?;
    let handoff: SubAgentHandoff = serde_json::from_str(&text).ok()?;
    (handoff.version == HANDOFF_VERSION).then_some(handoff)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_start(name: &str, input: serde_json::Value) -> TraceEvent {
        TraceEvent::ToolStart {
            tool_name: name.to_string(),
            input_json: input.to_string(),
            truncated: false,
        }
    }

    #[test]
    fn digest_separates_reads_modifications_and_errors() {
        let events = vec![
            tool_start("Read", serde_json::json!({"file_path": "a.rs"})),
            TraceEvent::ToolEnd {
                tool_name: "Read".into(),
                is_error: false,
                elapsed_secs: 0.1,
                output: "ok".into(),
                truncated: false,
            },
            tool_start("Edit", serde_json::json!({"file_path": "a.rs"})),
            tool_start("Bash", serde_json::json!({"command": "cargo test"})),
            TraceEvent::ToolEnd {
                tool_name: "Bash".into(),
                is_error: true,
                elapsed_secs: 2.0,
                output: "error[E0308]: mismatched types\nmore".into(),
                truncated: false,
            },
            tool_start("Grep", serde_json::json!({"pattern": "fn main"})),
        ];
        let d = ToolDigest::from_trace(&events);
        assert_eq!(d.total_calls, 4);
        assert_eq!(d.files_read, vec!["a.rs".to_string()]);
        assert_eq!(d.files_modified, vec!["a.rs".to_string()]);
        assert_eq!(d.commands, vec!["cargo test".to_string()]);
        assert_eq!(d.searches, vec!["fn main".to_string()]);
        assert_eq!(d.errors.len(), 1);
        assert!(d.errors[0].starts_with("Bash: error[E0308]"));
    }

    #[test]
    fn digest_dedups_and_caps() {
        let mut events = Vec::new();
        for _ in 0..50 {
            events.push(tool_start(
                "Read",
                serde_json::json!({"file_path": "same.rs"}),
            ));
        }
        for i in 0..50 {
            events.push(tool_start(
                "Read",
                serde_json::json!({"file_path": format!("f{i}.rs")}),
            ));
        }
        let d = ToolDigest::from_trace(&events);
        assert_eq!(d.files_read.len(), DIGEST_LIST_LIMIT);
        assert_eq!(d.files_read[0], "same.rs");
        assert_eq!(d.total_calls, 100);
    }

    #[test]
    fn digest_ignores_unparseable_input() {
        let events = vec![TraceEvent::ToolStart {
            tool_name: "Read".into(),
            input_json: "{not json".into(),
            truncated: false,
        }];
        let d = ToolDigest::from_trace(&events);
        assert_eq!(d.total_calls, 1);
        assert!(d.files_read.is_empty());
    }

    #[test]
    fn handoff_round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!(
            "wyj-handoff-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let handoff = SubAgentHandoff {
            version: HANDOFF_VERSION,
            id: 3,
            agent_type: "general-purpose".into(),
            description: "调查鉴权".into(),
            background: false,
            status: HandoffStatus::Interrupted,
            reason: Some("interrupted by user".into()),
            conclusion: "鉴权走 middleware/auth.rs:88".into(),
            digest: ToolDigest {
                files_read: vec!["auth.rs".into()],
                total_calls: 1,
                ..Default::default()
            },
            elapsed_secs: 12.5,
        };
        write_handoff(&dir, "s1", &handoff).unwrap();
        let read = read_handoff(&dir, "s1", 3).expect("应能读回");
        assert_eq!(read.status, HandoffStatus::Interrupted);
        assert_eq!(read.conclusion, "鉴权走 middleware/auth.rs:88");
        assert_eq!(read.digest.files_read, vec!["auth.rs".to_string()]);
        // 不存在的 id 不 panic，只是 None
        assert!(read_handoff(&dir, "s1", 99).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn incomplete_statuses_cover_everything_but_completion() {
        assert!(!HandoffStatus::Completed.is_incomplete());
        assert!(HandoffStatus::Interrupted.is_incomplete());
        assert!(HandoffStatus::TurnLimit.is_incomplete());
        assert!(HandoffStatus::Failed.is_incomplete());
    }
}
