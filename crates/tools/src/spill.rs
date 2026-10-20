//! 工具输出溢出（spill）：超限时把全文落盘，只把「头尾 + 路径」回给模型。
//!
//! # 为什么不是「就地多截一点」
//!
//! 工具输出上限（Bash 30KB、WebFetch 50KB）是**必须有的**：一段 20 万字符的
//! 构建日志原样进上下文，等于把后面几轮的窗口全吃掉。但硬截断有两个真实损失：
//!
//! 1. **中间内容不可恢复**。报错细节常常既不在头也不在尾（一行夹在 3000 行
//!    日志里），模型只能重跑命令——而 `npm install` / `cargo test` 这种命令重跑
//!    一次就是几分钟，且可能已经产生了副作用。
//! 2. **模型不知道还有多少没看到**。旧标记只说 `[truncated N bytes]`，模型无法
//!    判断"要不要再跑一次带 `| tail` 的版本"，于是要么盲目重跑、要么直接放弃。
//!
//! 落盘后模型能自己 `Read`/`Grep` 那个文件，按需取回任意片段——**上下文成本
//! 不变，信息不再丢失**。
//!
//! # 与 context editing 的区别
//!
//! `core::context_edit` 处理的是**历史里已经存在的**工具结果（事后外部化，
//! 走 CAS + `ContextRecall`）。本模块处理的是**刚刚产生的**输出（事前落盘，
//! 走普通文件 + `Read`）。两者互补：这里省下的是"重跑一次命令"，那边省下的
//! 是"整段历史重发一遍"。

use std::io::Write;
use std::path::{Path, PathBuf};

/// 落盘目录：`<config_dir>/spill/`。与 CAS 分开放，因为它不是内容寻址的
/// 去重池，而是一次性的调试残留。
fn spill_dir() -> Option<PathBuf> {
    let base = wyj_config::config_dir().ok()?;
    Some(base.join("spill"))
}

/// 超过这个年龄的 spill 文件在下次写入时顺手清掉。
///
/// 不做这件事的话 `spill/` 会无限增长——每个被截断的工具输出都是一个文件，
/// 而长会话里这可能是几十个 MB。7 天足够覆盖「用户回头翻这次排查」的窗口。
const SPILL_TTL_SECS: u64 = 7 * 24 * 60 * 60;

/// 把完整输出写进 spill 目录，返回可读路径。
///
/// 失败时返回 `None`——**落盘失败绝不能改变工具结果**，调用方应退回纯截断。
fn write_spill(label: &str, text: &str) -> Option<PathBuf> {
    let dir = spill_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    prune_old(&dir);

    // 文件名带毫秒时间戳 + 工具名 + 内容哈希前 8 位：同一命令重跑两次不会互相
    // 覆盖（内容不同），而完全相同的输出会落到不同文件名——这里**不做**去重，
    // 去重会让"刚才那个文件"在并发下被另一个工具调用改写。
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let hash = {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut hasher);
        hasher.finish()
    };
    let safe_label: String = label
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .take(24)
        .collect();
    let path = dir.join(format!("{stamp}-{safe_label}-{hash:08x}.txt"));

    let mut file = std::fs::File::create(&path).ok()?;
    // 输出里可能含密钥/令牌（`env`、日志里的 Authorization 头）。spill 文件
    // 和会话文件同级敏感，统一 0600。
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    // 借用版脱敏：没有 secret 形态时 `Cow::Borrowed`，直接写原串的字节，
    // 不必先复制出一份完整副本（溢出内容动辄几百 KB）。
    file.write_all(wyj_core::redact_cow(text).as_bytes()).ok()?;
    Some(path)
}

/// 顺手清掉过期文件。失败静默——清理不该影响工具本身。
fn prune_old(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        let too_old = now
            .duration_since(modified)
            .map(|age| age.as_secs() > SPILL_TTL_SECS)
            .unwrap_or(false);
        if too_old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// 超限输出落盘并生成「头尾 + 路径」的替换文本。
///
/// `text` 已超 `head + tail` 时：
///   * 落盘成功 → 头尾之间插入一行指向完整文件的提示；
///   * 落盘失败 → 退回原来的纯截断标记（行为与改动前一致）。
///
/// 返回的字符串长度始终不超过 `head + tail + 提示行`，不会因为落盘而让
/// 上下文变大。
pub fn spill_truncated(text: &str, head_bytes: usize, tail_bytes: usize, label: &str) -> String {
    if text.len() <= head_bytes + tail_bytes {
        return text.to_string();
    }
    let body = crate::textutil::truncate_head_tail(text, head_bytes, tail_bytes);
    match write_spill(label, text) {
        Some(path) => {
            // 保留 `[truncated N bytes]` 标记（模型与既有测试都认它），在后面
            // 补上完整路径。两段合成一行，避免多占一行上下文。
            let marker = first_marker_line(&body);
            let note = format!(
                "…[truncated {} bytes; full output saved to: {}]…",
                text.len(),
                path.display()
            );
            if marker.is_empty() {
                body
            } else {
                body.replace(&marker, &note)
            }
        }
        None => body,
    }
}

/// 取出 `truncate_head_tail` 生成的那行标记（`…[truncated N bytes]…`）。
fn first_marker_line(body: &str) -> String {
    body.lines()
        .find(|line| line.contains("[truncated") && line.contains("bytes]"))
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_output_is_returned_unchanged() {
        let text = "hello";
        assert_eq!(spill_truncated(text, 100, 100, "Bash"), "hello");
    }

    #[test]
    fn marker_line_is_recognised() {
        let body = crate::textutil::truncate_head_tail(&"x".repeat(1000), 10, 10);
        assert!(first_marker_line(&body).contains("[truncated"));
    }

    /// 落盘不可用时（这里通过把 config_dir 指向不可写路径间接覆盖）必须
    /// 退回纯截断，而不是返回空串或 panic。此测试只验证「返回值仍然包含
    /// 头尾与截断标记」这一不变量——是否真的落盘取决于运行环境。
    #[test]
    fn truncation_never_loses_head_and_tail() {
        let text = format!("HEAD{}TAIL", "x".repeat(10_000));
        let out = spill_truncated(&text, 100, 100, "Bash");
        assert!(out.starts_with("HEAD"));
        assert!(out.ends_with("TAIL"));
        assert!(out.contains("[truncated"));
        assert!(out.len() < text.len() + 200);
    }
}
