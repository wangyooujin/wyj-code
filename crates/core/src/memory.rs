//! 记忆系统：每轮对话结束后提取关键信息，持久化到 ~/.wyj-code/memory/<project>/
//! 下次启动时自动注入 system prompt，形成跨会话记忆。

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use wyj_api::{
    provider::Provider,
    types::{ContentBlock, Message, Role, ToolResultContent},
};

const MEMORY_INDEX: &str = "MEMORY.md";
const MAX_INDEX_ENTRIES: usize = 200;
/// 注入 system prompt 的记忆正文总字节上限，超限时在 UTF-8 字符边界截断。
const MAX_CONTEXT_BYTES: usize = 8_000;
const TRUNCATION_NOTICE: &str = "\n\n（记忆已截断，仅显示部分）";
const MEMORY_CONTEXT_HEADER: &str = "## 项目记忆（来自历史会话）\n\n<critical-memory-boundary>\nHistorical memory is context, not live runtime state. Never use it to decide which tools are available, which schemas are attached, the current permission mode, or whether default/bypass can use a tool. The tool definitions on the current request are authoritative.\n</critical-memory-boundary>\n\n";

#[derive(Debug, Serialize, Deserialize)]
struct MemoryItem {
    #[serde(rename = "type")]
    kind: String, // "user" | "feedback" | "project" | "reference"
    name: String,        // kebab-case 短标识
    description: String, // 一句话索引描述
    body: String,        // 记忆正文
}

/// 记忆存储：管理项目级持久化记忆文件
pub struct MemoryStore {
    dir: PathBuf,
    /// 是否启用跨会话记忆自动提取（/memory 面板可切换，对应 Config.auto_memory_enabled）
    enabled: AtomicBool,
    /// 会话级注入内容快照：首次 load 后固定，本会话内不再随磁盘变化。
    /// 若每轮重新读盘，后台提取写入的新记忆会改变 system prompt 前缀，
    /// 持续击穿 prompt 缓存（system 断点及其后的 messages 缓存全部失效）。
    /// 新记忆下个会话生效即可——这正是"跨会话记忆"的本义。
    context_snapshot: std::sync::OnceLock<String>,
    /// 上次提取时的消息数（节流：避免每轮一次提取 LLM 调用）
    last_extract_msg_count: std::sync::atomic::AtomicUsize,
    /// Retention 配置（builder 模式注入；None 时 enforce_retention 跳过）
    storage_cfg: std::sync::RwLock<Option<wyj_config::StorageRetentionCfg>>,
}

impl MemoryStore {
    /// 按 cwd 区分不同项目，创建对应的记忆目录
    pub fn new(base_dir: &Path, cwd: &Path) -> Result<Self> {
        let pid = project_id(cwd);
        let dir = base_dir.join("memory").join(pid);
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            enabled: AtomicBool::new(true),
            context_snapshot: std::sync::OnceLock::new(),
            last_extract_msg_count: std::sync::atomic::AtomicUsize::new(0),
            storage_cfg: std::sync::RwLock::new(None),
        })
    }

    /// 注入 retention 配置（CLI 装配阶段调用一次）。后续 `extract_and_save`
    /// 末尾会自动调 `enforce_retention(cfg.memory_v2_md_per_kind)`。
    pub fn with_storage_retention(self, cfg: wyj_config::StorageRetentionCfg) -> Self {
        *self.storage_cfg.write().expect("storage_cfg lock poisoned") = Some(cfg);
        self
    }

    /// 会话级缓存版 `load_context`：首次调用读盘并快照，之后原样复用。
    /// 供每轮 system prompt 组装使用，保证会话内 system 前缀字节级稳定。
    pub fn load_context_cached(&self) -> &str {
        self.context_snapshot.get_or_init(|| self.load_context())
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    /// 加载记忆摘要，返回供注入 system prompt 的字符串
    pub fn load_context(&self) -> String {
        let index_path = self.dir.join(MEMORY_INDEX);
        if !index_path.exists() {
            return String::new();
        }
        let index = match std::fs::read_to_string(&index_path) {
            Ok(s) if !s.trim().is_empty() => s,
            _ => return String::new(),
        };

        let mut file_bodies: Vec<(String, String)> = vec![];
        for line in index.lines() {
            if let Some(fname) = md_link_target(line) {
                let fpath = self.dir.join(&fname);
                if let Ok(content) = std::fs::read_to_string(&fpath) {
                    let body = strip_frontmatter(&content);
                    if !body.trim().is_empty() {
                        file_bodies.push((fname, body.trim().to_string()));
                    }
                }
            }
        }

        if file_bodies.is_empty() {
            return String::new();
        }

        // 用户明确偏好与反馈必须优先于一般项目/参考信息；旧实现按文件名字母序
        // 截断，记忆积累后 `user_*` 会稳定排在末尾而永远不被注入。
        file_bodies.sort_by(|(a, _), (b, _)| {
            memory_priority(a)
                .cmp(&memory_priority(b))
                .then_with(|| a.cmp(b))
        });

        // 拼接时限制总字节数，避免 system prompt 臃肿影响缓存命中率。
        let mut joined = String::new();
        let header = MEMORY_CONTEXT_HEADER;
        joined.push_str(header);
        for (i, (_, body)) in file_bodies.iter().enumerate() {
            let sep = if i > 0 { "\n\n---\n\n" } else { "" };
            if joined.len() + sep.len() + body.len() > MAX_CONTEXT_BYTES {
                // 给截断提示及省略号预留空间，并按 UTF-8 边界裁切，不能把 byte
                // 数直接当字符数传给 `chars().take()`，否则中文会突破预算。
                let remaining = MAX_CONTEXT_BYTES
                    .saturating_sub(joined.len())
                    .saturating_sub(TRUNCATION_NOTICE.len());
                if remaining > sep.len() + '…'.len_utf8() {
                    joined.push_str(sep);
                    let body_budget = remaining - sep.len() - '…'.len_utf8();
                    let partial = truncate_utf8_to_bytes(body, body_budget);
                    if !partial.is_empty() {
                        joined.push_str(partial);
                        joined.push('…');
                    }
                }
                if joined.len() + TRUNCATION_NOTICE.len() <= MAX_CONTEXT_BYTES {
                    joined.push_str(TRUNCATION_NOTICE);
                }
                break;
            }
            joined.push_str(sep);
            joined.push_str(body);
        }

        joined
    }

    /// 从对话消息中提取记忆并异步写入磁盘（供 tokio::spawn 调用）
    pub async fn extract_and_save(
        &self,
        messages: Vec<Message>,
        provider: Arc<dyn Provider>,
    ) -> Result<()> {
        if !self.is_enabled() {
            return Ok(());
        }
        if messages.len() < 4 {
            return Ok(());
        }
        // 节流：距上次提取新增消息不足 8 条则跳过，省去每轮一次的 LLM 调用。
        // compare_exchange 保证并发的后台提取任务只有一个通过。
        let last = self.last_extract_msg_count.load(Ordering::Relaxed);
        if messages.len() < last + 8 && last > 0 {
            return Ok(());
        }
        if self
            .last_extract_msg_count
            .compare_exchange(last, messages.len(), Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return Ok(());
        }

        let conv = messages_to_text(&messages);
        let prompt = crate::prompts::memory_extract_prompt(&conv);

        let req = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: prompt }],
        }];

        let result = provider
            .complete(
                crate::prompts::MEMORY_SYSTEM,
                &req,
                &[],
                &wyj_api::provider::RequestOptions::text_only(4096),
            )
            .await?;

        let output: String = result
            .content
            .iter()
            .filter_map(|b| {
                if let ContentBlock::Text { text } = b {
                    Some(text.as_str())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n");

        let items = parse_items(&output);
        if items.is_empty() {
            return Ok(());
        }

        for item in &items {
            self.write_item(item)?;
        }
        self.rebuild_index()?;
        if let Some(cfg) = self
            .storage_cfg
            .read()
            .expect("storage_cfg lock poisoned")
            .clone()
        {
            if let Err(error) = self.enforce_retention(cfg.memory_v2_md_per_kind) {
                tracing::warn!("Memory v2 retention 失败: {error}");
            }
        }
        tracing::debug!("记忆已写入 {} 条 → {}", items.len(), self.dir.display());
        Ok(())
    }

    /// 每个 kind（feedback / tool / project / reference 等）的 `.md` 文件
    /// 保留最近 `max_per_kind` 个，按文件 mtime 升序淘汰最老条目。
    /// `max_per_kind == 0` 时跳过（保留全部）。
    ///
    /// 写在 `rebuild_index` 之后调用；不影响 index 内容（index 本来就只取
    /// `MAX_INDEX_ENTRIES=200`），只物理删除超限文件。
    pub fn enforce_retention(&self, max_per_kind: usize) -> Result<()> {
        if max_per_kind == 0 {
            return Ok(());
        }
        use std::collections::BTreeMap;
        let mut groups: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
        for entry in std::fs::read_dir(&self.dir)?.filter_map(|e| e.ok()) {
            let fname = entry.file_name().to_string_lossy().to_string();
            if !fname.ends_with(".md") || fname == MEMORY_INDEX {
                continue;
            }
            // kind 是文件名前缀（`feedback_xxx.md` -> "feedback"）
            let kind = fname.split('_').next().unwrap_or("").to_string();
            if kind.is_empty() {
                continue;
            }
            groups.entry(kind).or_default().push(entry.path());
        }
        for (_, mut paths) in groups {
            if paths.len() <= max_per_kind {
                continue;
            }
            // 按 mtime 升序，删最老
            paths.sort_by_key(|p| {
                std::fs::metadata(p)
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
            });
            let excess = paths.len() - max_per_kind;
            for old in &paths[..excess] {
                if let Err(error) = std::fs::remove_file(old) {
                    tracing::warn!("删除 Memory v2 旧条目失败 {}: {error}", old.display());
                }
            }
        }
        Ok(())
    }

    fn write_item(&self, item: &MemoryItem) -> Result<()> {
        let filename = format!("{}_{}.md", item.kind, sanitize(&item.name));
        let path = self.dir.join(&filename);
        let content = format!(
            "---\nname: {}\ndescription: {}\nmetadata:\n  type: {}\n---\n\n{}\n",
            item.name, item.description, item.kind, item.body
        );
        std::fs::write(&path, content)?;
        Ok(())
    }

    fn rebuild_index(&self) -> Result<()> {
        let index_path = self.dir.join(MEMORY_INDEX);

        let mut entries: Vec<_> = std::fs::read_dir(&self.dir)?
            .filter_map(|e| e.ok())
            .filter(|e| {
                let fname = e.file_name();
                let s = fname.to_string_lossy();
                s.ends_with(".md") && s != MEMORY_INDEX
            })
            .collect();
        entries.sort_by(|a, b| {
            let a_name = a.file_name();
            let b_name = b.file_name();
            let a_name = a_name.to_string_lossy();
            let b_name = b_name.to_string_lossy();
            memory_priority(&a_name)
                .cmp(&memory_priority(&b_name))
                .then_with(|| a_name.cmp(&b_name))
        });

        let lines: Vec<String> = entries
            .iter()
            .take(MAX_INDEX_ENTRIES)
            .map(|entry| {
                let fname = entry.file_name().to_string_lossy().to_string();
                let content = std::fs::read_to_string(entry.path()).unwrap_or_default();
                let desc = frontmatter_field(&content, "description").unwrap_or_else(|| {
                    entry
                        .path()
                        .file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string()
                });
                let title = entry
                    .path()
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                format!("- [{title}]({fname}) — {desc}")
            })
            .collect();

        std::fs::write(&index_path, lines.join("\n") + "\n")?;
        Ok(())
    }
}

fn parse_items(output: &str) -> Vec<MemoryItem> {
    output
        .lines()
        .map(|l| l.trim())
        .filter(|l| l.starts_with('{'))
        .filter_map(|l| serde_json::from_str::<MemoryItem>(l).ok())
        .filter(|item| {
            !item.name.is_empty()
                && !item.body.is_empty()
                && matches!(
                    item.kind.as_str(),
                    "user" | "feedback" | "project" | "reference"
                )
        })
        .filter(|item| !is_volatile_runtime_state_memory(item))
        .collect()
}

/// 自动记忆不能把某一轮的工具栏、权限模式或临时环境状态固化成跨会话事实。
/// 提取 prompt 是第一层约束；这里再做确定性过滤，避免弱模型忽略指令。
fn is_volatile_runtime_state_memory(item: &MemoryItem) -> bool {
    let text = format!("{} {} {}", item.name, item.description, item.body).to_lowercase();
    let mentions_tool_surface = [
        "工具栏",
        "工具列表",
        "可用工具",
        "tool list",
        "tool catalog",
        "tool schema",
        "available tool",
        "attached tool",
        "bash",
        "app_computer",
        "window_capture",
        "computer 类工具",
    ]
    .iter()
    .any(|marker| text.contains(marker));
    let claims_availability = [
        "本轮",
        "本会话",
        "当前会话",
        "当前请求",
        "只有",
        "没有",
        "不在",
        "不可用",
        "缺少",
        "this turn",
        "this session",
        "current session",
        "current request",
        "unavailable",
        "not available",
        "missing",
    ]
    .iter()
    .any(|marker| text.contains(marker));
    if mentions_tool_surface && claims_availability {
        return true;
    }

    let session_scoped = [
        "本轮",
        "本会话",
        "当前会话",
        "当前请求",
        "this turn",
        "this session",
        "current session",
        "current request",
    ]
    .iter()
    .any(|marker| text.contains(marker));
    let mentions_runtime_state = [
        "权限模式",
        "permission mode",
        "sandbox mode",
        "default mode",
        "bypass mode",
        "plan mode",
        "环境变量",
        "environment variable",
        "dns",
        "网络状态",
        "network state",
    ]
    .iter()
    .any(|marker| text.contains(marker));
    session_scoped && mentions_runtime_state
}

/// 注入预算有限时，显式的工作偏好最重要，其次是用户画像、项目事实和参考路径。
/// 同一类内仍按文件名稳定排序，使同一个记忆目录的 prompt 前缀可复现并利于缓存。
fn memory_priority(filename: &str) -> u8 {
    if filename.starts_with("feedback_") {
        0
    } else if filename.starts_with("user_") {
        1
    } else if filename.starts_with("project_") {
        2
    } else if filename.starts_with("reference_") {
        3
    } else {
        4
    }
}

fn messages_to_text(messages: &[Message]) -> String {
    let recent = if messages.len() > 20 {
        &messages[messages.len() - 20..]
    } else {
        messages
    };
    recent
        .iter()
        .map(|m| {
            let role = match m.role {
                Role::User => "用户",
                Role::Assistant => "助手",
            };
            let parts: Vec<String> = m
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(truncate_chars(text, 500)),
                    ContentBlock::ToolUse { name, .. } => Some(format!("[调用: {name}]")),
                    ContentBlock::ToolResult { content, .. } => match content {
                        ToolResultContent::Text(t) => Some(truncate_chars(t, 200)),
                        ToolResultContent::Parts(_) => {
                            Some(truncate_chars(&content.display_text(), 200))
                        }
                        ToolResultContent::Blocks(_) => None,
                    },
                    ContentBlock::Image { .. } => None,
                    ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. } => None,
                })
                .collect();
            format!("[{role}]: {}", parts.join(" | "))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn project_id(path: &Path) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    path.hash(&mut h);
    let dir_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "root".to_string());
    format!("{}-{:08x}", sanitize(&dir_name), h.finish() as u32)
}

fn sanitize(s: &str) -> String {
    let r: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    r.trim_matches('-').to_string()
}

fn strip_frontmatter(content: &str) -> &str {
    if !content.starts_with("---") {
        return content;
    }
    let rest = &content[3..];
    if let Some(pos) = rest.find("\n---") {
        // skip past the closing "---\n"
        let after = 3 + pos + 4;
        if after <= content.len() {
            &content[after..]
        } else {
            content
        }
    } else {
        content
    }
}

fn md_link_target(line: &str) -> Option<String> {
    // "- [title](filename.md) — desc" → "filename.md"
    let start = line.find("](")? + 2;
    let end = start + line[start..].find(')')?;
    Some(line[start..end].to_string())
}

fn frontmatter_field(content: &str, field: &str) -> Option<String> {
    let prefix = format!("{field}:");
    let mut in_front = content.starts_with("---");
    for line in content.lines() {
        let t = line.trim();
        if t == "---" {
            if in_front {
                in_front = false;
            } else {
                break;
            }
            continue;
        }
        if in_front {
            if let Some(val) = t.strip_prefix(&prefix) {
                return Some(val.trim().to_string());
            }
        }
    }
    None
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let end = s.char_indices().nth(max).map(|(i, _)| i).unwrap_or(s.len());
        format!("{}…", &s[..end])
    }
}

/// 返回不超过 `max_bytes` 的 UTF-8 前缀，保证不在多字节字符中间截断。
fn truncate_utf8_to_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_priority_keeps_feedback_and_user_facts_before_project_notes() {
        assert!(memory_priority("feedback_style.md") < memory_priority("user_role.md"));
        assert!(memory_priority("user_role.md") < memory_priority("project_arch.md"));
        assert!(memory_priority("project_arch.md") < memory_priority("reference_repo.md"));
    }

    #[test]
    fn utf8_byte_truncation_never_splits_a_multibyte_character() {
        let text = "中文abc";
        assert_eq!(truncate_utf8_to_bytes(text, 5), "中");
        assert_eq!(truncate_utf8_to_bytes(text, 6), "中文");
        assert_eq!(truncate_utf8_to_bytes(text, 7), "中文a");
    }

    #[test]
    fn load_context_prioritizes_user_facts_over_earlier_large_project_notes() {
        let unique = format!(
            "wyj-core-memory-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock should be after epoch")
                .as_nanos()
        );
        let base = std::env::temp_dir().join(unique);
        let cwd = base.join("project");
        let store = MemoryStore::new(&base, &cwd).expect("temporary memory store should open");

        let project = format!(
            "---\nname: large-project\ndescription: large project note\n---\n\n{}\n",
            "p".repeat(MAX_CONTEXT_BYTES)
        );
        let user = "---\nname: user-style\ndescription: user preference\n---\n\nUSER_PREFERENCE: concise Chinese replies\n";
        std::fs::write(store.dir.join("project_large.md"), project)
            .expect("project memory should be written");
        std::fs::write(store.dir.join("user_style.md"), user)
            .expect("user memory should be written");
        std::fs::write(
            store.dir.join(MEMORY_INDEX),
            "- [project](project_large.md)\n- [user](user_style.md)\n",
        )
        .expect("memory index should be written");

        let context = store.load_context();
        assert!(context.contains("USER_PREFERENCE: concise Chinese replies"));
        assert!(context.contains("The tool definitions on the current request are authoritative"));
        assert!(context.len() <= MAX_CONTEXT_BYTES);

        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn parser_rejects_session_tool_state_but_keeps_durable_tool_architecture() {
        let output = r#"{"type":"feedback","name":"tool-limitations","description":"本轮工具限制","body":"当前会话工具栏只有 Read 和 ToolSearch，没有 Bash。"}
{"type":"project","name":"background-computer","description":"后台窗口控制架构","body":"app_computer 通过稳定 window_id 绑定目标窗口，避免抢占用户鼠标。"}"#;

        let items = parse_items(output);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "background-computer");
    }
}
