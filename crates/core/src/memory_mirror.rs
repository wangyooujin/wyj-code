//! 记忆的 **markdown 镜像** —— 让每条 claim 成为 git 里可 diff 的文件。
//!
//! # 为什么需要
//!
//! 真源是 `records.json`（结构化、含 evidence / supersede 链 / access 计数）。
//! 它对机器友好，对人**不可读、不可 diff、不可手改**：
//!
//! - 用户想看「我的记忆里到底记了什么」，只能打开 `INDEX.md` 看摘要；
//! - 想改一条，只能改 JSON——一屏就是几百行，改错格式整份解析失败；
//! - 想放进 git 做版本管理，diff 出来的是 JSON 数组的整块重排，
//!   看不出「哪条记忆变了什么」。
//!
//! Claude Code 的 auto memory 是**纯 markdown 目录**，用户可以直接
//! `cat` / `vim` / `git diff`。那正是它最大的体验优势。
//!
//! # 设计取舍：镜像而非取代
//!
//! 真源仍是 `records.json`。理由：claim 有结构化字段（evidence 数组、
//! supersede 链、access 计数），而 markdown frontmatter 表达不了这些，
//! 强行表达会让 markdown 变成「另一种 JSON」，既不可读也没换来 diff 友好。
//!
//! 所以镜像的定位是：
//!   - **可读**：每条 claim 一个文件，标题即文件名；
//!   - **可 diff**：一条记忆变化只动一个文件；
//!   - **可手改**（受限）：只允许改正文（`content` 后的 Markdown 区块），
//!     结构化字段改不动——手改那些等于绕过 `validate_write` 的约束。
//!
//! # 同步时机
//!
//! `upsert` / `forget` / `confirm` / `reject` 之后各写一次该条的文件。
//! **不做全量重建**：那会在每次记忆更新时重写整个目录，git diff 会
//! 因为时间戳抖动而全量变化。
//!
//! # 删除
//!
//! claim 变 Superseded / Rejected 时，对应文件**保留**（它的历史仍然有
//! 意义），只在文件名后加 `.archived.md` 后缀。真删只在 `clear_all` 时。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::memory_v3::{MemoryClaimKind, MemoryClaimScope, MemoryClaimStatus, MemoryRecord};

/// 镜像目录名（位于 memory-v3 根下）。
pub const MIRROR_DIR_NAME: &str = "mirror";

/// 已归档 claim 的文件后缀。
const ARCHIVED_SUFFIX: &str = ".archived.md";

/// 单条 claim 的 markdown 渲染。
///
/// frontmatter 只放**人类可读且可编辑**的字段：kind / scope / status /
/// tags / entities。结构化字段（id / evidence / supersede / access_count）
/// 留在 `records.json`——它们在 markdown 里只会变成噪声。
pub fn render_claim(record: &MemoryRecord) -> String {
    let mut out = String::new();
    out.push_str("---\n");
    out.push_str(&format!("kind: {}\n", kind_label(record.kind)));
    out.push_str(&format!("scope: {}\n", scope_label(record.scope)));
    out.push_str(&format!("status: {}\n", status_label(record.status)));
    if !record.tags.is_empty() {
        out.push_str(&format!("tags: [{}]\n", record.tags.join(", ")));
    }
    if !record.entities.is_empty() {
        out.push_str(&format!("entities: [{}]\n", record.entities.join(", ")));
    }
    if let Some(task_status) = record.task_status {
        out.push_str(&format!("task_status: {task_status:?}\n"));
    }
    out.push_str("---\n\n");
    out.push_str(&format!("# {}\n\n", record.title));
    out.push_str(record.content.trim());
    out.push('\n');

    // 未完成的步骤单列：它们是「下一步做什么」的直接来源，
    // 混在正文里会被 summary 摊平。
    let pending: Vec<&crate::memory_v3::TaskStep> =
        record.task_steps.iter().filter(|s| !s.done).collect();
    if !pending.is_empty() {
        out.push_str("\n## Next steps\n\n");
        for step in pending {
            out.push_str(&format!("- [ ] {}\n", step.description));
        }
    }
    let done = record.task_steps.iter().filter(|s| s.done).count();
    if done > 0 {
        out.push_str(&format!("\n({done} 个步骤已完成)\n"));
    }
    out
}

fn kind_label(kind: MemoryClaimKind) -> &'static str {
    match kind {
        MemoryClaimKind::Instruction => "instruction",
        MemoryClaimKind::Preference => "preference",
        MemoryClaimKind::Fact => "fact",
        MemoryClaimKind::MutableState => "mutable_state",
        MemoryClaimKind::Event => "event",
        MemoryClaimKind::Workflow => "workflow",
        MemoryClaimKind::Hypothesis => "hypothesis",
        MemoryClaimKind::Reference => "reference",
        MemoryClaimKind::Task => "task",
    }
}

fn scope_label(scope: MemoryClaimScope) -> &'static str {
    match scope {
        MemoryClaimScope::Global => "global",
        MemoryClaimScope::Project => "project",
    }
}

fn status_label(status: MemoryClaimStatus) -> &'static str {
    match status {
        MemoryClaimStatus::Active => "active",
        MemoryClaimStatus::Superseded => "superseded",
        MemoryClaimStatus::Rejected => "rejected",
        MemoryClaimStatus::PendingGlobalCandidate => "pending_global",
    }
}

/// 文件名：`<slug>-<id 后 8 位>.md`。
///
/// 带 id 后缀是为了**唯一性**：两条标题相同的记忆（不同 scope 或不同
/// supersede 世代）不能覆盖到同一个文件上。带 slug 是为了**可读**——
/// 光一个 id 的文件名在 `ls` 里毫无意义。
pub fn claim_filename(record: &MemoryRecord) -> String {
    format!("{}-{}.md", slugify(&record.title), short_id(&record.id))
}

fn slugify(title: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in title.chars() {
        if ch.is_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
        if out.chars().count() >= 40 {
            break;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "untitled".to_string()
    } else {
        trimmed
    }
}

fn short_id(id: &str) -> String {
    // `mem_<uuid>` / `mem_<...>`：取最后 8 位即可，配合 slug 保证可读。
    id.chars()
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

/// 单条 claim 的镜像文件路径。
pub fn claim_path(mirror_dir: &Path, record: &MemoryRecord) -> PathBuf {
    mirror_dir.join(claim_filename(record))
}

/// 写入（或更新）一条 claim 的镜像文件。
///
/// 归档态（Superseded / Rejected）写到 `.archived.md`：它的历史仍有意义，
/// 但用户 `ls` 时第一眼看到的应当只有活着的记忆。
pub fn write_claim(mirror_dir: &Path, record: &MemoryRecord) -> Result<()> {
    std::fs::create_dir_all(mirror_dir)
        .with_context(|| format!("创建记忆镜像目录 {}", mirror_dir.display()))?;
    let body = render_claim(record);
    let archived = !matches!(
        record.status,
        MemoryClaimStatus::Active | MemoryClaimStatus::PendingGlobalCandidate
    );
    let filename = if archived {
        let base = claim_filename(record);
        format!(
            "{}{}",
            base.strip_suffix(".md").unwrap_or(&base),
            ARCHIVED_SUFFIX
        )
    } else {
        claim_filename(record)
    };
    let path = mirror_dir.join(filename);
    crate::serialize::write_atomic(&path, body.as_bytes())
        .with_context(|| format!("写入记忆镜像 {}", path.display()))?;
    // 从活档案位挪走后，旧的非归档文件必须删掉——否则同一份记忆在
    // `ls` 里出现两次，用户不知道该改哪个。
    if archived {
        let live = claim_path(mirror_dir, record);
        if live != path {
            let _ = std::fs::remove_file(live);
        }
    }
    Ok(())
}

/// 删除一条 claim 的镜像文件（`clear_all` 用）。
pub fn remove_claim(mirror_dir: &Path, record: &MemoryRecord) {
    let _ = std::fs::remove_file(claim_path(mirror_dir, record));
    let base = claim_filename(record);
    let archived_name = format!(
        "{}{}",
        base.strip_suffix(".md").unwrap_or(&base),
        ARCHIVED_SUFFIX
    );
    let _ = std::fs::remove_file(mirror_dir.join(archived_name));
}

/// 从一条 markdown 镜像里读回**正文**（frontmatter 之后的部分）。
///
/// 用途：用户手改了文件，这里把改后的正文读出来，交给
/// `MemoryV3Store::update_from_mirror` 覆盖到真源上。
///
/// 只回正文：结构化字段改不了（`update_from_mirror` 会带上原记录的
/// id / kind / scope），这是刻意的——手改那些等于绕过 `validate_write`。
pub fn read_body(path: &Path) -> Result<String> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("读取记忆镜像 {}", path.display()))?;
    let body = match raw.split_once("---") {
        Some((_, rest)) => {
            // frontmatter 是 `---\n...\n---\n`，第二个 `---` 之后才是正文。
            rest.split_once("---").map_or(rest, |(_, body)| body)
        }
        None => raw.as_str(),
    };
    Ok(extract_content(body))
}

/// 从渲染出的正文里取出**用户可编辑的那一段**（`content` 本身）。
///
/// 渲染会额外加上 `# 标题`、`## Next steps` 与「N 个步骤已完成」——
/// 那些是从结构化字段**生成**的，不属于 `content`。回灌时若把它们
/// 一起写进 `content`，下一次渲染就会出现「标题套标题」的复利。
///
/// 边界：`# ` 开头的是标题行；`## Next steps` 之后的一段是步骤区。
fn extract_content(body: &str) -> String {
    let mut lines: Vec<&str> = Vec::new();
    // 标题行的位置不固定：前面可能有空行（`---\n` 之后就是一个换行）。
    // 用「还没见过任何非空行」作判据，而不是 `lines.is_empty()`。
    let mut seen_content = false;
    for line in body.lines() {
        if !seen_content {
            if line.trim().is_empty() {
                continue;
            }
            if line.starts_with("# ") {
                // 标题行由 `record.title` 生成，不属于 content。
                continue;
            }
            seen_content = true;
        }
        if line.trim() == "## Next steps" {
            break;
        }
        lines.push(line);
    }
    let joined = lines.join("\n");
    let trimmed = joined.trim();
    // 去掉尾部「(N 个步骤已完成)」这类生成出来的注记。
    let cleaned = trimmed
        .rsplit_once("\n(")
        .filter(|(_, tail)| tail.ends_with("个步骤已完成)"))
        .map(|(head, _)| head)
        .unwrap_or(trimmed);
    cleaned.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_v3::{MemorySource, MemorySourceKind};

    fn record(title: &str) -> MemoryRecord {
        MemoryRecord {
            schema_version: crate::memory_v3::MEMORY_V3_SCHEMA_VERSION,
            id: "mem_abcdef1234567890".to_string(),
            kind: MemoryClaimKind::Fact,
            scope: MemoryClaimScope::Project,
            scope_key: "v1-proj-abc".to_string(),
            title: title.to_string(),
            content: "Rust workspace，单二进制。".to_string(),
            entities: vec!["rust".to_string()],
            tags: vec!["语言".to_string()],
            source: MemorySource {
                kind: MemorySourceKind::User,
                locator: "session:s1".to_string(),
                observed_at: Some("2026-10-01T00:00:00Z".to_string()),
            },
            evidence: Vec::new(),
            confidence: 0.9,
            status: MemoryClaimStatus::Active,
            created_at: "2026-10-01T00:00:00Z".to_string(),
            updated_at: "2026-10-01T00:00:00Z".to_string(),
            expires_at: None,
            supersedes: None,
            superseded_by: None,
            task_status: None,
            task_steps: Vec::new(),
            blocked_reason: None,
            access_count: 0,
            last_accessed_at: None,
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wyj-memmirror-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 渲染必须带 frontmatter（人类可读）与正文（可手改）。
    #[test]
    fn render_has_both_frontmatter_and_body() {
        let text = render_claim(&record("项目语言"));
        assert!(text.starts_with("---\n"), "{text}");
        assert!(text.contains("kind: fact"), "{text}");
        assert!(text.contains("scope: project"), "{text}");
        assert!(text.contains("# 项目语言"), "{text}");
        assert!(text.contains("Rust workspace，单二进制。"), "{text}");
    }

    /// 文件名必须**可读且唯一**：slug 给人看，id 后缀防覆盖。
    #[test]
    fn the_filename_is_readable_and_unique() {
        let a = record("同一个标题");
        let mut b = record("同一个标题");
        b.id = "mem_zzzz9999aaaa1111".to_string();
        assert_ne!(claim_filename(&a), claim_filename(&b));
        assert!(claim_filename(&a).ends_with(".md"));
        assert!(claim_filename(&a).contains("同一个标题"));
    }

    /// 未完成的步骤要单列在 `## Next steps` 里——它们是「下一步做什么」
    /// 的直接来源，混在正文里会被 summary 摊平。
    #[test]
    fn pending_task_steps_are_listed_separately() {
        let mut r = record("迁移到 Memory v3");
        r.kind = MemoryClaimKind::Task;
        r.task_status = Some(crate::memory_v3::TaskStatus::InProgress);
        r.task_steps = vec![
            crate::memory_v3::TaskStep {
                description: "已完成的一步".to_string(),
                done: true,
                updated_at: None,
            },
            crate::memory_v3::TaskStep {
                description: "下一步要做".to_string(),
                done: false,
                updated_at: None,
            },
        ];
        let text = render_claim(&r);
        assert!(text.contains("## Next steps"), "{text}");
        assert!(text.contains("- [ ] 下一步要做"), "{text}");
        assert!(
            !text.contains("- [ ] 已完成的一步"),
            "已完成的步骤不该进 Next steps: {text}"
        );
    }

    /// 归档态的文件名带 `.archived.md`，且**不残留**非归档版本——
    /// 否则同一份记忆在 `ls` 里出现两次，用户不知道该改哪个。
    #[test]
    fn archived_claims_do_not_leave_a_live_file_behind() {
        let dir = temp_dir("archive");
        let mut r = record("被顶掉的记忆");
        write_claim(&dir, &r).unwrap();
        let live = claim_path(&dir, &r);
        assert!(live.exists());

        r.status = MemoryClaimStatus::Superseded;
        write_claim(&dir, &r).unwrap();
        assert!(!live.exists(), "归档后不得残留 live 文件");
        let archived = dir.join(format!(
            "{}{}",
            claim_filename(&r).strip_suffix(".md").unwrap(),
            ARCHIVED_SUFFIX
        ));
        assert!(archived.exists(), "应有归档文件");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `read_body` 只回正文：frontmatter 是机器读的，用户手改的是正文。
    #[test]
    fn read_body_returns_only_the_markdown_body() {
        let dir = temp_dir("body");
        let r = record("可手改的记忆");
        let path = claim_path(&dir, &r);
        write_claim(&dir, &r).unwrap();
        // 模拟用户手改正文
        let edited = render_claim(&r).replace("Rust workspace，单二进制。", "改成了 Python。");
        std::fs::write(&path, edited).unwrap();

        let body = read_body(&path).unwrap();
        assert!(body.contains("改成了 Python。"), "{body}");
        assert!(
            !body.contains("kind: fact"),
            "正文不该含 frontmatter: {body}"
        );
        assert!(!body.contains("---"), "正文不该含分隔线: {body}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn slugify_handles_punctuation_and_cjk() {
        assert_eq!(slugify("Hello, World!"), "hello-world");
        assert_eq!(slugify("  前后空格  "), "前后空格");
        assert_eq!(slugify("!!!"), "untitled");
    }
}
