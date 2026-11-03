//! 项目级进度文件 —— 穿透压缩的磁盘状态载体。
//!
//! TodoWrite 每轮重放进上下文、实测 100% 独占 LLM 往返；压缩摘要又会把
//! 「做到哪了」洗成一句话。本模块把进度落到 `<git-root>/.wyj-code/progress.md`：
//!
//! - 模型经 `Progress` 工具读写；
//! - 压缩成功后由框架从磁盘**重新注入**进最新摘要消息；
//! - 跨 session 保留（同项目续跑仍在）。
//!
//! 与 [`crate::session_memory`] 的区别：滚动文档是 LLM 增量摘要、按 session
//! 分文件；本文件是模型自己维护的权威清单，压缩不得改写磁盘原文。

use std::path::{Path, PathBuf};

use wyj_api::types::{ContentBlock, Message, Role};

/// 相对项目根的进度文件路径。
pub const PROGRESS_REL_PATH: &str = ".wyj-code/progress.md";

/// 进度文件硬上限（字节）。超过则写入被拒绝——它要整段塞进压缩后上下文。
pub const MAX_PROGRESS_BYTES: usize = 16_000;

/// 注入块开标签（压缩后摘要里识别用）。
pub const PROGRESS_MARKER_OPEN: &str = "<project-progress path=\".wyj-code/progress.md\">";

/// 注入块闭标签。
pub const PROGRESS_MARKER_CLOSE: &str = "</project-progress>";

/// `<git-root-or-cwd>/.wyj-code/progress.md`。
pub fn progress_path(project_root: &Path) -> PathBuf {
    project_root.join(PROGRESS_REL_PATH)
}

/// 读出现行进度；空文件 / 不存在 / 读失败 → `None`。
pub fn read_progress(project_root: &Path) -> Option<String> {
    let path = progress_path(project_root);
    let text = std::fs::read_to_string(&path).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

/// 写入进度（整文件替换）。超长或 IO 失败返回错误字符串给工具层。
pub fn write_progress(project_root: &Path, content: &str) -> Result<(), String> {
    if content.len() > MAX_PROGRESS_BYTES {
        return Err(format!(
            "progress.md exceeds {MAX_PROGRESS_BYTES} bytes (got {}); shrink it before writing",
            content.len()
        ));
    }
    let path = progress_path(project_root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    // 与会话 JSON 同款：先写临时文件再 rename，避免半截内容被下一轮压缩读到。
    let tmp = path.with_extension("md.tmp");
    std::fs::write(&tmp, content).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("rename {}: {e}", path.display()))?;
    Ok(())
}

/// 把磁盘进度拼进压缩摘要消息正文（替换旧注入块）。
///
/// 只改**第一条**带压缩前缀的 user 文本消息——那正是刚写进去的摘要。
/// 没有摘要或文件为空时是 no-op。
pub fn splice_into_latest_summary(messages: &mut [Message], project_root: &Path) {
    let Some(content) = read_progress(project_root) else {
        // 文件空了：仍要清掉历史里残留的旧注入块，否则模型会信过期进度。
        strip_progress_blocks(messages);
        return;
    };
    let block = format_progress_block(&content);
    let Some(msg) = messages.iter_mut().find(|m| is_compact_summary(m)) else {
        return;
    };
    for part in &mut msg.content {
        if let ContentBlock::Text { text } = part {
            *text = replace_or_append_progress(text, &block);
            return;
        }
    }
}

fn is_compact_summary(msg: &Message) -> bool {
    if !matches!(msg.role, Role::User) {
        return false;
    }
    msg.content.iter().any(|b| {
        matches!(
            b,
            ContentBlock::Text { text }
                if text.contains(crate::prompts::COMPACT_SUMMARY_PREFIX)
        )
    })
}

fn format_progress_block(content: &str) -> String {
    format!(
        "\n\n{PROGRESS_MARKER_OPEN}\n\
         Persistent project progress (authoritative; re-read from disk after compaction). \
         Update via the Progress tool — do not invent a parallel todo list for long-running work.\n\
         {content}\n\
         {PROGRESS_MARKER_CLOSE}"
    )
}

fn replace_or_append_progress(text: &str, block: &str) -> String {
    if let Some(stripped) = strip_one_progress_block(text) {
        format!("{stripped}{block}")
    } else {
        format!("{text}{block}")
    }
}

fn strip_progress_blocks(messages: &mut [Message]) {
    for msg in messages {
        for part in &mut msg.content {
            if let ContentBlock::Text { text } = part {
                if let Some(stripped) = strip_one_progress_block(text) {
                    *text = stripped;
                }
            }
        }
    }
}

fn strip_one_progress_block(text: &str) -> Option<String> {
    let start = text.find(PROGRESS_MARKER_OPEN)?;
    let after_open = start + PROGRESS_MARKER_OPEN.len();
    let end_rel = text[after_open..].find(PROGRESS_MARKER_CLOSE)?;
    let end = after_open + end_rel + PROGRESS_MARKER_CLOSE.len();
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..start]);
    out.push_str(&text[end..]);
    Some(out.trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn write_read_round_trip() {
        let dir = tempdir().unwrap();
        write_progress(dir.path(), "# done\n- a\n").unwrap();
        assert_eq!(read_progress(dir.path()).as_deref(), Some("# done\n- a"));
    }

    #[test]
    fn rejects_oversized_writes() {
        let dir = tempdir().unwrap();
        let big = "x".repeat(MAX_PROGRESS_BYTES + 1);
        assert!(write_progress(dir.path(), &big).is_err());
    }

    #[test]
    fn splice_appends_and_replaces() {
        let dir = tempdir().unwrap();
        write_progress(dir.path(), "step 1").unwrap();
        let mut messages = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: format!(
                    "{} 3 earlier messages were compacted into the following]\n\nsummary body",
                    crate::prompts::COMPACT_SUMMARY_PREFIX
                ),
            }],
        }];
        splice_into_latest_summary(&mut messages, dir.path());
        let text = match &messages[0].content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!(),
        };
        assert!(text.contains("step 1"));
        assert!(text.contains(PROGRESS_MARKER_OPEN));

        write_progress(dir.path(), "step 2").unwrap();
        splice_into_latest_summary(&mut messages, dir.path());
        let text = match &messages[0].content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!(),
        };
        assert!(text.contains("step 2"));
        assert!(!text.contains("step 1"));
        assert_eq!(text.matches(PROGRESS_MARKER_OPEN).count(), 1);
    }
}
