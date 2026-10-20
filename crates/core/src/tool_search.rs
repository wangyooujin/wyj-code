//! ToolSearch 与按会话 sticky 的 lazy tool schema。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use wyj_api::types::ToolDefinition;

use crate::tool::{Tool, ToolContext, ToolResult};

#[derive(Clone)]
pub struct LazyToolState {
    inner: Arc<RwLock<LazyToolInner>>,
}

struct LazyToolInner {
    catalog: HashMap<String, ToolCatalogEntry>,
    core: HashSet<String>,
    /// 会话内**曾经**暴露过 schema 的工具。
    ///
    /// 这一层是**单调**的：只增不减，直到工具被真正卸载（`remove`）。
    /// 它存在的唯一理由是保护 prompt cache —— Anthropic 的缓存前缀物理顺序
    /// 是 `tools → system → messages`，官方文档明写「不要在会话中途增删或
    /// 重排 tools，任何改动都会让 tools 缓存及其后的 system、messages 全部
    /// 失效」。旧实现用 `sticky` 兼管可见性，`sticky_turns`（默认 3）到期后
    /// 工具从请求里消失，代价是**整段 prompt 缓存全部作废**——比它省下的
    /// 那点 schema token 贵几个数量级。
    ///
    /// 「可被模型调用」和「搜索时排前面」是两种不同的诉求：前者一旦给出就
    /// 不该收回（收回等于让模型上一轮还能调、这一轮突然不能调），后者应当
    /// 有新鲜度窗口。因此拆成 `exposed`（单调，管可见性）与 `sticky`
    /// （带过期，管搜索加权）两层。
    exposed: HashSet<String>,
    /// 工具名 → 最后使用轮次。**只用于搜索排序加权**，不决定可见性。
    sticky: HashMap<String, u64>,
    current_turn: u64,
    top_k: usize,
    sticky_turns: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolCatalogEntry {
    #[serde(skip)]
    pub definition: ToolDefinition,
    pub summary: String,
    pub tags: Vec<String>,
    pub source: String,
    pub read_only: bool,
    pub required_capabilities: Vec<String>,
}

impl ToolCatalogEntry {
    pub fn inferred(definition: ToolDefinition) -> Self {
        let source = definition
            .name
            .strip_prefix("mcp__")
            .and_then(|rest| rest.split("__").next())
            .map(|server| format!("mcp:{server}"))
            .unwrap_or_else(|| "builtin".to_string());
        let name = definition.name.to_ascii_lowercase();
        let description = definition.description.to_ascii_lowercase();
        let read_only = matches!(
            name.as_str(),
            "read" | "glob" | "grep" | "codesearch" | "webfetch" | "websearch" | "toolsearch"
        ) || description.contains("read-only");
        let mut tags = vec![if read_only { "read" } else { "side-effect" }.to_string()];
        for (needle, tag) in [
            ("code", "code"),
            ("file", "filesystem"),
            ("shell", "shell"),
            ("web", "network"),
            ("search", "search"),
            ("agent", "agent"),
            ("index", "index"),
        ] {
            if name.contains(needle) || description.contains(needle) {
                tags.push(tag.to_string());
            }
        }
        tags.sort();
        tags.dedup();
        let required_capabilities = definition
            .native
            .as_ref()
            .map(|native| vec![format!("native_tool:{}", native.tool_type)])
            .unwrap_or_default();
        let summary = definition
            .description
            .split(['.', '\n'])
            .next()
            .unwrap_or(&definition.description)
            .trim()
            .chars()
            .take(180)
            .collect();
        Self {
            definition,
            summary,
            tags,
            source,
            read_only,
            required_capabilities,
        }
    }
}

impl LazyToolState {
    pub fn new(core: impl IntoIterator<Item = String>, top_k: usize, sticky_turns: u64) -> Self {
        Self {
            inner: Arc::new(RwLock::new(LazyToolInner {
                catalog: HashMap::new(),
                core: core.into_iter().collect(),
                exposed: HashSet::new(),
                sticky: HashMap::new(),
                current_turn: 0,
                top_k: top_k.clamp(1, 12),
                sticky_turns: sticky_turns.max(1),
            })),
        }
    }

    pub fn upsert(&self, definition: ToolDefinition) {
        self.upsert_entry(ToolCatalogEntry::inferred(definition));
    }

    pub fn upsert_entry(&self, entry: ToolCatalogEntry) {
        self.inner
            .write()
            .unwrap()
            .catalog
            .insert(entry.definition.name.clone(), entry);
    }

    pub fn remove(&self, name: &str) {
        let mut inner = self.inner.write().unwrap();
        inner.catalog.remove(name);
        inner.sticky.remove(name);
        // 工具被真正卸载（插件移除 / 重新注册）时，可见性必须跟着收回，
        // 否则 `visible` 会为已不存在的工具放行，而 `tools_for_request` 又是
        // 按 `self.tools` 过滤的，两边对不上。
        inner.exposed.remove(name);
    }

    /// 新回合开始：轮次 +1，并让**搜索加权**的 sticky 记录过期。
    ///
    /// 刻意**不**动 `exposed` —— 已暴露过的工具在本会话内保持可见，请求里的
    /// tools 数组因此单调不减，prompt cache 不会被中途作废。
    pub fn begin_task_turn(&self) {
        let mut inner = self.inner.write().unwrap();
        inner.current_turn = inner.current_turn.saturating_add(1);
        let current = inner.current_turn;
        let sticky_turns = inner.sticky_turns;
        inner
            .sticky
            .retain(|_, last_used| current.saturating_sub(*last_used) <= sticky_turns);
    }

    /// 显式暴露某个工具的 schema：从这一刻起它在本会话内**永久可见**。
    ///
    /// 可见性是单调的（见 `exposed` 字段），因为 schema 一旦发给模型就
    /// 不得收回——收回会让 tools 数组中途缩水，进而让整段 prompt cache
    /// 失效。工具被真正卸载时由 [`Self::remove`] 收回。
    pub fn expose(&self, name: &str) {
        let mut inner = self.inner.write().unwrap();
        Self::expose_locked(&mut inner, name);
    }

    fn expose_locked(inner: &mut LazyToolInner, name: &str) {
        if inner.catalog.contains_key(name) {
            inner.exposed.insert(name.to_string());
        }
    }

    pub fn mark_used(&self, name: &str) {
        let mut inner = self.inner.write().unwrap();
        if inner.catalog.contains_key(name) && !inner.core.contains(name) && name != "ToolSearch" {
            let current = inner.current_turn;
            inner.sticky.insert(name.to_string(), current);
            Self::expose_locked(&mut inner, name);
        }
    }

    /// 本次请求里这个工具的 schema 会不会被发出去。
    ///
    /// 判定**单调**：`core` 常驻、`exposed` 只增不减，因此同一个会话内
    /// 可见集合只增不减 —— 这是保护 Anthropic prompt cache 的硬约束
    /// （见 `LazyToolInner::exposed`）。
    pub fn visible(&self, name: &str) -> bool {
        let inner = self.inner.read().unwrap();
        Self::is_visible(&inner, name)
    }

    fn is_visible(inner: &LazyToolInner, name: &str) -> bool {
        inner.core.contains(name) || inner.exposed.contains(name) || name == "ToolSearch"
    }

    /// 本次请求被 lazy 折叠掉的工具，按 source 分组计数（source 升序）。
    ///
    /// 唯一用途是喂给 system prompt 的 `<current-tool-availability>`：只报
    /// 「本次直接可调用」的名单而不提被折叠的部分，模型会把「不在列表里」
    /// 读成「本会话没有这个工具」。实测（2026-10-18）MCP codegraph 的 8 个
    /// 工具全部被折叠后，模型面对「列出 mcp__ 开头的工具」直接回答"没有，
    /// 当前会话未挂载任何 MCP 工具"，随后改用 Read/Grep 手工翻代码——
    /// 而 catalog 里那 8 个工具一直躺着，只是没人告诉它可以去检索。
    pub fn hidden_sources(&self) -> Vec<(String, usize)> {
        let inner = self.inner.read().unwrap();
        let mut counts: HashMap<String, usize> = HashMap::new();
        for entry in inner.catalog.values() {
            let name = &entry.definition.name;
            if !Self::is_visible(&inner, name) {
                *counts.entry(entry.source.clone()).or_insert(0) += 1;
            }
        }
        let mut grouped: Vec<(String, usize)> = counts.into_iter().collect();
        grouped.sort_by(|a, b| a.0.cmp(&b.0));
        grouped
    }

    fn search(
        &self,
        query: &str,
        limit: usize,
        source: Option<&str>,
        tags: &[String],
        read_only: Option<bool>,
    ) -> Vec<ToolCatalogEntry> {
        let query = query.trim().to_ascii_lowercase();
        let terms: Vec<&str> = query.split_whitespace().collect();
        let mut inner = self.inner.write().unwrap();
        let mut ranked: Vec<(i32, ToolCatalogEntry)> = inner
            .catalog
            .values()
            .filter(|entry| entry.definition.name != "ToolSearch")
            .filter(|entry| source.map_or(true, |source| entry.source.eq_ignore_ascii_case(source)))
            .filter(|entry| read_only.map_or(true, |read_only| entry.read_only == read_only))
            .filter(|entry| {
                tags.iter().all(|tag| {
                    entry
                        .tags
                        .iter()
                        .any(|candidate| candidate.eq_ignore_ascii_case(tag))
                })
            })
            .filter_map(|entry| {
                let name = entry.definition.name.to_ascii_lowercase();
                let description = entry.definition.description.to_ascii_lowercase();
                let tag_text = entry.tags.join(" ").to_ascii_lowercase();
                let source_text = entry.source.to_ascii_lowercase();
                let mut score = 0;
                if inner.sticky.contains_key(&entry.definition.name) {
                    score += 15;
                }
                if name == query {
                    score += 100;
                } else if name.starts_with(&query) {
                    score += 60;
                } else if name.contains(&query) {
                    score += 40;
                }
                for term in &terms {
                    if name.contains(term) {
                        score += 20;
                    }
                    if description.contains(term) {
                        score += 5;
                    }
                    if tag_text.contains(term) {
                        score += 12;
                    }
                    if source_text.contains(term) {
                        score += 8;
                    }
                }
                (score > 0 || (!tags.is_empty() || source.is_some() || read_only.is_some()))
                    .then(|| (score, entry.clone()))
            })
            .collect();
        ranked.sort_by(|(score_a, entry_a), (score_b, entry_b)| {
            score_b
                .cmp(score_a)
                .then_with(|| entry_a.definition.name.cmp(&entry_b.definition.name))
        });
        let entries: Vec<ToolCatalogEntry> = ranked
            .into_iter()
            .take(limit.clamp(1, inner.top_k))
            .map(|(_, entry)| entry)
            .collect();
        let current = inner.current_turn;
        inner.sticky.extend(
            entries
                .iter()
                .map(|entry| (entry.definition.name.clone(), current)),
        );
        // 命中的工具从这一刻起**永久可见**（直到被 `remove`）：schema 一旦
        // 发出去就不得收回，否则 tools 数组中途缩水会让整段 prompt cache
        // 失效。`sticky` 的过期只影响搜索排序加权。
        for entry in &entries {
            Self::expose_locked(&mut inner, &entry.definition.name);
        }
        entries
    }
}

pub struct ToolSearchTool {
    state: LazyToolState,
}

impl ToolSearchTool {
    pub fn new(state: LazyToolState) -> Self {
        Self { state }
    }
}

#[derive(Deserialize)]
struct SearchInput {
    query: String,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    read_only: Option<bool>,
}

#[async_trait]
impl Tool for ToolSearchTool {
    fn name(&self) -> &str {
        "ToolSearch"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Search the available tool catalog by capability before calling a tool whose schema is not currently visible. Matching schemas become available on the next model turn and remain sticky for this session.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["query"],
                "properties": {
                    "query": {"type": "string", "minLength": 1},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 12},
                    "source": {"type": "string", "description": "Optional exact source such as builtin or mcp:server"},
                    "tags": {"type": "array", "items": {"type": "string"}, "maxItems": 8},
                    "read_only": {"type": "boolean"}
                },
                "additionalProperties": false
            }),
            native: None,
        }
    }

    /// 目录查询本身无副作用，可安全并发：`LazyToolState` 内部是
    /// `Arc<RwLock<..>>`，search 把命中的工具置 sticky 的写入也是加锁的。
    ///
    /// 漏掉这个覆写会让「同时搜两个能力」退化成两趟串行往返，而 ToolSearch
    /// 恰恰是 lazy-tools 折叠下**每次换能力都要付一趟**的高频工具。
    fn parallel_safe(&self) -> bool {
        true
    }

    async fn run(&self, input: Value, _ctx: &dyn ToolContext) -> Result<ToolResult> {
        let input: SearchInput = serde_json::from_value(input)?;
        let matches = self.state.search(
            &input.query,
            input.limit.unwrap_or(usize::MAX),
            input.source.as_deref(),
            &input.tags,
            input.read_only,
        );
        if matches.is_empty() {
            return Ok(ToolResult::ok(
                "No matching tools. Refine the capability query; do not guess a hidden tool name."
                    .to_string(),
            ));
        }
        let compact: Vec<Value> = matches
            .into_iter()
            .map(|entry| {
                serde_json::json!({
                    "name": entry.definition.name,
                    "summary": entry.summary,
                    "tags": entry.tags,
                    "source": entry.source,
                    "read_only": entry.read_only,
                    "required_capabilities": entry.required_capabilities,
                    "available_next_turn": true
                })
            })
            .collect();
        Ok(ToolResult::ok(serde_json::to_string(&compact)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_makes_matching_tool_sticky_without_exposing_everything() {
        let state = LazyToolState::new(["Read".to_string()], 8, 3);
        for (name, description) in [
            ("Read", "read a file"),
            ("Bash", "run a shell command"),
            ("WebFetch", "fetch a URL"),
        ] {
            state.upsert(ToolDefinition {
                name: name.to_string(),
                description: description.to_string(),
                input_schema: serde_json::json!({"type": "object"}),
                native: None,
            });
        }
        assert!(state.visible("Read"));
        assert!(!state.visible("Bash"));
        let found = state.search("shell command", 3, None, &[], None);
        assert_eq!(found[0].definition.name, "Bash");
        assert!(state.visible("Bash"));
        assert!(!state.visible("WebFetch"));
    }

    #[test]
    fn hidden_sources_groups_folded_tools_by_source() {
        let state = LazyToolState::new(["Read".to_string()], 8, 3);
        for (name, description) in [
            ("Read", "read a file"),
            ("Bash", "run a shell command"),
            ("WebFetch", "fetch a URL"),
        ] {
            state.upsert(ToolDefinition {
                name: name.to_string(),
                description: description.to_string(),
                input_schema: serde_json::json!({"type": "object"}),
                native: None,
            });
        }
        let mut mcp = ToolDefinition {
            name: "mcp__codegraph__codegraph_search".to_string(),
            description: "search symbols".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
            native: None,
        };
        state.upsert(mcp.clone());
        mcp.name = "mcp__codegraph__codegraph_explore".to_string();
        state.upsert(mcp);

        // Read 是 core、ToolSearch 自动可见，其余都算折叠。
        assert_eq!(
            state.hidden_sources(),
            vec![("builtin".to_string(), 2), ("mcp:codegraph".to_string(), 2)]
        );

        state.search("shell", 8, None, &[], None);
        // Bash 被命中后已 sticky，builtin 只剩 WebFetch 一个折叠。
        assert_eq!(
            state.hidden_sources(),
            vec![("builtin".to_string(), 1), ("mcp:codegraph".to_string(), 2)]
        );
    }

    #[test]
    fn exposed_tools_stay_visible_after_the_sticky_window_expires() {
        let state = LazyToolState::new(["Read".to_string()], 2, 2);
        for name in ["Read", "Bash", "WebFetch"] {
            state.upsert(ToolDefinition {
                name: name.to_string(),
                description: name.to_string(),
                input_schema: serde_json::json!({"type": "object"}),
                native: None,
            });
        }
        state.begin_task_turn();
        state.search("Bash", 8, None, &[], None);
        assert!(state.visible("Bash"));
        state.begin_task_turn();
        state.mark_used("Bash");
        state.begin_task_turn();
        state.begin_task_turn();
        assert!(state.visible("Bash"));
        // sticky 的新鲜度窗口已过，但 schema **已经发出去过**，不得收回：
        // 收回会让 tools 数组中途缩水，进而让整段 prompt cache 失效。
        state.begin_task_turn();
        assert!(state.visible("Bash"));

        state.search("WebFetch", 8, None, &[], None);
        assert!(state.visible("WebFetch"));
        // 工具被真正卸载时，可见性才跟着收回。
        state.remove("WebFetch");
        assert!(!state.visible("WebFetch"));
    }

    /// 可见集合必须**单调不减**——这是保护 Anthropic prompt cache 的硬约束。
    ///
    /// Anthropic 的缓存前缀物理顺序是 `tools → system → messages`，官方文档
    /// 明写「不要在会话中途增删或重排 tools，任何改动都会让 tools 缓存及其后
    /// 的 system、messages 全部失效」。所以本会话内一旦暴露过某个工具的
    /// schema，它就必须一直留在请求里。
    #[test]
    fn the_visible_tool_set_never_shrinks_within_a_session() {
        let state = LazyToolState::new(["Read".to_string()], 8, 1);
        for name in ["Read", "Bash", "WebFetch", "Grep"] {
            state.upsert(ToolDefinition {
                name: name.to_string(),
                description: name.to_string(),
                input_schema: serde_json::json!({"type": "object"}),
                native: None,
            });
        }

        let snapshot = |state: &LazyToolState| -> Vec<String> {
            let mut names: Vec<String> = ["Read", "Bash", "WebFetch", "Grep", "ToolSearch"]
                .into_iter()
                .filter(|name| state.visible(name))
                .map(|name| name.to_string())
                .collect();
            names.sort();
            names
        };

        let mut previous = snapshot(&state);
        assert_eq!(previous, vec!["Read", "ToolSearch"]);

        for turn in 0..12 {
            state.begin_task_turn();
            if turn % 2 == 0 {
                state.search("Grep", 8, None, &[], None);
            } else {
                state.search("WebFetch", 8, None, &[], None);
            }
            let current = snapshot(&state);
            assert!(
                current.len() >= previous.len(),
                "可见工具集合在第 {turn} 轮缩水了：{previous:?} → {current:?}"
            );
            previous = current;
        }
    }
}
