//! System prompt 指纹：记录「这一轮实际发出去的 prompt 由什么构成」的摘要。
//!
//! # 解决什么问题
//!
//! 同一个会话隔天 resume，模型行为可能明显不同，而**没有任何地方能回答
//! 「当时注入的是什么」**。原因是 prompt 每轮在内存里现拼：
//! `Agent::system_prompt`（主提示 + `<env>`）在进程内固定，但 `memory_snapshot`
//! 按 10 轮分桶重算、`ClaudeMdLoader::turn_reminder` 每轮重读盘、Skill 名单与
//! MCP 工具集随配置漂移，版本升级还会整体换掉 `prompts::MAIN`。四者叠加，
//! 同一个 `--resume` 出来的会话，两次运行看到的上下文可以差出几千 token。
//!
//! # 为什么只存指纹不存全文
//!
//! 全文进会话文件会立刻撞上另一个问题：本仓库的 system prompt 动辄 30KB+
//! （完整规约链），而会话文件每轮整份重写。存全文等于每轮多写 30KB，且
//! `SessionFile` 一旦包含 prompt 正文，它就变成了「prompt 的第二份事实来源」，
//! 与 `Agent` 内存里的那份迟早漂移。
//!
//! 指纹（`sha256` 前 16 hex + 字节数）足以回答排障要的那两个问题：
//! **变没变**、**变了哪一段**。真要逐字比对，用 `git diff` 或重新渲染一遍
//! 就够——那是一次性动作，不该由常驻的落盘结构承担。
//!
//! # 哪些 section 参与对比、哪些不参与
//!
//! | section | 对比 | 理由 |
//! |---|---|---|
//! | `main` | ✅ | 主提示 + `<env>`。**版本升级**与 locale/cwd 变化的唯一可见点 |
//! | `tools` | ✅ | 工具定义集合。MCP / plugin / lazy 折叠开关变化都在这里 |
//! | `memory` | ❌ | 按 10 轮分桶重算，**本来就该变**，报差异是噪音 |
//! | `project_rules` | ❌ | 每轮重读盘，用户改 AGENTS.md 后本就该变 |
//! | `volatile` | ❌ | 工具可用性名单 + 语言提醒，随回合变化 |
//!
//! 前两个的语义是「同配置下应当稳定」——它们变了才是值得解释的事。

use sha2::{Digest, Sha256};

/// 主提示 + `<env>` 块（`Agent::system_prompt`）。
pub const SECTION_MAIN: &str = "main";
/// 跨会话记忆快照。
pub const SECTION_MEMORY: &str = "memory";
/// 项目规约链（AGENTS.md / CLAUDE.md 祖先链 + 本轮触达目录的 reminder）。
pub const SECTION_PROJECT_RULES: &str = "project_rules";
/// volatile 尾部三段（兼容 suffix → 工具可用性 → 语言提醒）。
pub const SECTION_VOLATILE: &str = "volatile";
/// 本次发送的工具定义集合。
pub const SECTION_TOOLS: &str = "tools";

/// 参与 drift 对比的 section。**只有这两个**，理由见模块文档的表。
pub const COMPARABLE_SECTIONS: [&str; 2] = [SECTION_MAIN, SECTION_TOOLS];

/// 单个 section 的摘要。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FingerprintSection {
    pub name: String,
    /// 原始字节数。与 `digest` 一起看：字节数相同但 digest 不同 = 改写了内容；
    /// 字节数变化 = 有内容增删。两者的排查方向完全不同。
    pub bytes: usize,
    /// `sha256` 的前 16 个 hex 字符（64 bit）。碰撞概率对本用途可忽略。
    pub digest: String,
    /// 仅 `tools` section 填充：本次发送的工具名，**已排序**。
    ///
    /// 有了它，drift 才能说出「新增了 `mcp__x__y`」而不只是「digest 变了」。
    /// 排序是为了让指纹本身不受工具注册顺序影响。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_names: Vec<String>,
}

impl FingerprintSection {
    fn new(name: &str, bytes: usize, digest: String) -> Self {
        Self {
            name: name.to_string(),
            bytes,
            digest,
            tool_names: Vec::new(),
        }
    }
}

/// 某一轮请求的完整指纹。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PromptFingerprint {
    /// 记录时刻（RFC3339）。展示用：告诉用户「这是哪一轮的指纹」。
    #[serde(default)]
    pub recorded_at: String,
    pub sections: Vec<FingerprintSection>,
}

/// 两个指纹之间某个可比 section 的差异。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionDrift {
    pub name: String,
    pub previous_digest: String,
    pub current_digest: String,
    pub previous_bytes: usize,
    pub current_bytes: usize,
    /// 仅 `tools`：当前有而旧指纹没有的工具名。
    pub added_tools: Vec<String>,
    /// 仅 `tools`：旧指纹有而当前没有的工具名。
    pub removed_tools: Vec<String>,
}

/// 一次请求相对落盘指纹的漂移。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptDrift {
    /// 旧指纹的记录时刻（多半来自上次运行）。
    pub previous_recorded_at: String,
    pub sections: Vec<SectionDrift>,
}

impl PromptDrift {
    pub fn is_empty(&self) -> bool {
        self.sections.is_empty()
    }
}

fn digest_of(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let hex = format!("{:x}", hasher.finalize());
    hex[..16].to_string()
}

fn digest_of_str(text: &str) -> String {
    digest_of(text.as_bytes())
}

impl PromptFingerprint {
    /// 采集一份指纹。
    ///
    /// `tools` 传本次**真正发送**（已过能力裁剪 + lazy 折叠）的定义，
    /// 不是全量注册表——折叠掉的工具不进上下文，它们的变化也不该被报成
    /// 「prompt 变了」。
    pub fn capture(
        main: &str,
        memory: &str,
        project_rules: &str,
        volatile: &str,
        tools: &[wyj_api::ToolDefinition],
    ) -> Self {
        let mut sections = vec![
            FingerprintSection::new(SECTION_MAIN, main.len(), digest_of_str(main)),
            FingerprintSection::new(SECTION_MEMORY, memory.len(), digest_of_str(memory)),
            FingerprintSection::new(
                SECTION_PROJECT_RULES,
                project_rules.len(),
                digest_of_str(project_rules),
            ),
            FingerprintSection::new(SECTION_VOLATILE, volatile.len(), digest_of_str(volatile)),
        ];

        // 工具指纹 = 排序后的 (name, description, schema) 三元组序列化后再 hash。
        // 不直接 hash `serde_json::to_string(tools)`：`ToolDefinition` 的字段顺序
        // 由结构体定义决定（稳定），但注册顺序会变（插件加载顺序、MCP server
        // 连接顺序），不排序就会把「同一个工具集换个顺序」报成变化。
        let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        names.sort_unstable();
        let mut tool_hasher = Sha256::new();
        let mut bytes = 0usize;
        for name in &names {
            if let Some(tool) = tools.iter().find(|t| t.name == *name) {
                let serialized = serde_json::json!({
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": tool.input_schema,
                })
                .to_string();
                bytes += serialized.len();
                tool_hasher.update(serialized.as_bytes());
                tool_hasher.update(b"\n");
            }
        }
        let mut tools_section = FingerprintSection::new(
            SECTION_TOOLS,
            bytes,
            format!("{:x}", tool_hasher.finalize())[..16].to_string(),
        );
        tools_section.tool_names = names.into_iter().map(str::to_string).collect();
        sections.push(tools_section);

        Self {
            recorded_at: chrono::Utc::now().to_rfc3339(),
            sections,
        }
    }

    pub fn section(&self, name: &str) -> Option<&FingerprintSection> {
        self.sections.iter().find(|s| s.name == name)
    }

    /// 比对**可比 section**（`COMPARABLE_SECTIONS`），返回差异列表。
    ///
    /// 空列表 = 两次运行的 prompt 在这两个维度上完全一致。`memory` /
    /// `project_rules` / `volatile` 刻意不参与：它们本来就该变。
    pub fn drift(&self, current: &Self) -> PromptDrift {
        let mut sections = Vec::new();
        for name in COMPARABLE_SECTIONS {
            let (Some(previous), Some(now)) = (self.section(name), current.section(name)) else {
                // 缺 section 说明是旧版本落盘的指纹（字段引入前）。这里不报差异
                // ——「旧数据没有这一项」和「这一项变了」是两回事，混为一谈会
                // 让升级后的首次 resume 刷一堆假漂移。
                continue;
            };
            if previous.digest == now.digest {
                continue;
            }
            let (added_tools, removed_tools) = if name == SECTION_TOOLS {
                (
                    now.tool_names
                        .iter()
                        .filter(|n| !previous.tool_names.contains(n))
                        .cloned()
                        .collect(),
                    previous
                        .tool_names
                        .iter()
                        .filter(|n| !now.tool_names.contains(n))
                        .cloned()
                        .collect(),
                )
            } else {
                (Vec::new(), Vec::new())
            };
            sections.push(SectionDrift {
                name: name.to_string(),
                previous_digest: previous.digest.clone(),
                current_digest: now.digest.clone(),
                previous_bytes: previous.bytes,
                current_bytes: now.bytes,
                added_tools,
                removed_tools,
            });
        }
        PromptDrift {
            previous_recorded_at: self.recorded_at.clone(),
            sections,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wyj_api::ToolDefinition;

    fn tool(name: &str, description: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            description: description.to_string(),
            input_schema: serde_json::json!({"type": "object"}),
            native: None,
        }
    }

    #[test]
    fn identical_prompts_produce_no_drift() {
        let a = PromptFingerprint::capture("main", "mem", "rules", "vol", &[tool("Read", "r")]);
        let b = PromptFingerprint::capture("main", "mem", "rules", "vol", &[tool("Read", "r")]);
        assert!(a.drift(&b).is_empty());
    }

    #[test]
    fn main_prompt_change_is_reported() {
        let old = PromptFingerprint::capture("v1", "", "", "", &[]);
        let new = PromptFingerprint::capture("v2", "", "", "", &[]);
        let drift = old.drift(&new);
        assert_eq!(drift.sections.len(), 1);
        assert_eq!(drift.sections[0].name, SECTION_MAIN);
        assert_ne!(
            drift.sections[0].previous_digest,
            drift.sections[0].current_digest
        );
    }

    #[test]
    fn volatile_sections_are_never_compared() {
        // memory / project_rules / volatile 变了都不该报：它们本来就每轮变。
        let old = PromptFingerprint::capture("main", "old memory", "old rules", "v1", &[]);
        let new = PromptFingerprint::capture("main", "new memory", "new rules", "v2", &[]);
        assert!(old.drift(&new).is_empty());
    }

    #[test]
    fn tool_set_changes_name_the_added_and_removed_tools() {
        let old =
            PromptFingerprint::capture("m", "", "", "", &[tool("Read", "r"), tool("Grep", "g")]);
        let new =
            PromptFingerprint::capture("m", "", "", "", &[tool("Read", "r"), tool("Glob", "gl")]);
        let drift = old.drift(&new);
        assert_eq!(drift.sections.len(), 1);
        let section = &drift.sections[0];
        assert_eq!(section.name, SECTION_TOOLS);
        assert_eq!(section.added_tools, vec!["Glob".to_string()]);
        assert_eq!(section.removed_tools, vec!["Grep".to_string()]);
    }

    #[test]
    fn tool_registration_order_does_not_change_the_digest() {
        // 注册顺序来自插件加载 / MCP 连接顺序，不该被当成 prompt 变化。
        let a =
            PromptFingerprint::capture("m", "", "", "", &[tool("Read", "r"), tool("Grep", "g")]);
        let b =
            PromptFingerprint::capture("m", "", "", "", &[tool("Grep", "g"), tool("Read", "r")]);
        assert!(a.drift(&b).is_empty());
        assert_eq!(
            a.section(SECTION_TOOLS).unwrap().digest,
            b.section(SECTION_TOOLS).unwrap().digest
        );
    }

    #[test]
    fn same_tool_name_with_different_description_is_a_change() {
        // 版本升级改了工具描述，行为会变，必须报。
        let old = PromptFingerprint::capture("m", "", "", "", &[tool("Read", "old desc")]);
        let new = PromptFingerprint::capture("m", "", "", "", &[tool("Read", "new desc")]);
        let drift = old.drift(&new);
        assert_eq!(drift.sections.len(), 1);
        // 工具名集合没变，所以 added/removed 都是空，只有 digest 不同。
        assert!(drift.sections[0].added_tools.is_empty());
        assert!(drift.sections[0].removed_tools.is_empty());
    }

    #[test]
    fn missing_comparable_section_is_not_reported_as_drift() {
        // 旧版本落盘的指纹没有 tools section：这是「没数据」，不是「变了」。
        let old = PromptFingerprint {
            recorded_at: "2026-01-01T00:00:00Z".to_string(),
            sections: vec![FingerprintSection::new(
                SECTION_MAIN,
                4,
                digest_of_str("main"),
            )],
        };
        let new = PromptFingerprint::capture("main", "", "", "", &[]);
        assert!(old.drift(&new).is_empty());
    }

    #[test]
    fn fingerprint_round_trips_through_json() {
        let fp = PromptFingerprint::capture("m", "mem", "rules", "vol", &[tool("Read", "r")]);
        let json = serde_json::to_string(&fp).unwrap();
        let back: PromptFingerprint = serde_json::from_str(&json).unwrap();
        assert_eq!(fp, back);
    }
}
