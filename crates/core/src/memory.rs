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
/// 注入 system prompt 的记忆正文总字符上限，超限截断（避免 system prompt 臃肿）。
const MAX_CONTEXT_CHARS: usize = 8_000;

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
        })
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

        let mut file_bodies: Vec<String> = vec![];
        for line in index.lines() {
            if let Some(fname) = md_link_target(line) {
                let fpath = self.dir.join(&fname);
                if let Ok(content) = std::fs::read_to_string(&fpath) {
                    let body = strip_frontmatter(&content);
                    if !body.trim().is_empty() {
                        file_bodies.push(body.trim().to_string());
                    }
                }
            }
        }

        if file_bodies.is_empty() {
            return String::new();
        }

        // 拼接时限制总字符数，超限截断（避免 system prompt 臃肿影响缓存命中率）
        let mut joined = String::new();
        let header = "## 项目记忆（来自历史会话）\n\n";
        joined.push_str(header);
        for (i, body) in file_bodies.iter().enumerate() {
            let sep = if i > 0 { "\n\n---\n\n" } else { "" };
            if joined.len() + sep.len() + body.len() > MAX_CONTEXT_CHARS {
                let remaining = MAX_CONTEXT_CHARS.saturating_sub(joined.len() + sep.len());
                if remaining > 20 {
                    joined.push_str(sep);
                    let body_chars: Vec<char> = body.chars().take(remaining).collect();
                    joined.extend(body_chars);
                    joined.push_str("…");
                }
                joined.push_str("\n\n（记忆已截断，仅显示部分）");
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

        let conv = messages_to_text(&messages);
        let prompt = format!(
            "从以下 AI 编程助手的对话中提取值得跨会话保存的关键信息。\n\n\
            每条记忆单独一行 JSON（不要使用代码块标记），格式：\n\
            {{\"type\":\"...\",\"name\":\"...\",\"description\":\"...\",\"body\":\"...\"}}\n\n\
            type 取值：\n\
            - user: 用户角色、技能水平、工作习惯\n\
            - feedback: 用户对工作方式的明确偏好或纠正\n\
            - project: 项目背景、技术决策、架构信息\n\
            - reference: 重要资源位置（文件路径、仓库、文档）\n\n\
            规则：只提取跨会话真正有价值的信息；忽略临时任务状态和一次性代码；\
            没有可提取的内容时不输出任何内容。\n\n\
            对话记录：\n{conv}"
        );

        let req = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: prompt }],
        }];

        let result = provider
            .complete(
                "你是记忆提取助手，从对话中识别值得长期保存的关键信息。",
                &req,
                &[],
                4096,
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
        tracing::debug!("记忆已写入 {} 条 → {}", items.len(), self.dir.display());
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
        entries.sort_by_key(|e| e.file_name());

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
        .collect()
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
                        ToolResultContent::Blocks(_) => None,
                    },
                    ContentBlock::Image { .. } => None,
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
