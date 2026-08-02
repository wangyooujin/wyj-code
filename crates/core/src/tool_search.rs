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
    }

    pub fn begin_task_turn(&self) {
        let mut inner = self.inner.write().unwrap();
        inner.current_turn = inner.current_turn.saturating_add(1);
        let current = inner.current_turn;
        let sticky_turns = inner.sticky_turns;
        inner
            .sticky
            .retain(|_, last_used| current.saturating_sub(*last_used) <= sticky_turns);
    }

    pub fn mark_used(&self, name: &str) {
        let mut inner = self.inner.write().unwrap();
        if inner.catalog.contains_key(name) && !inner.core.contains(name) && name != "ToolSearch" {
            let current = inner.current_turn;
            inner.sticky.insert(name.to_string(), current);
        }
    }

    pub fn visible(&self, name: &str) -> bool {
        let inner = self.inner.read().unwrap();
        inner.core.contains(name) || inner.sticky.contains_key(name) || name == "ToolSearch"
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
    fn sticky_tools_expire_and_removed_tools_disappear_immediately() {
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
        state.begin_task_turn();
        assert!(!state.visible("Bash"));

        state.search("WebFetch", 8, None, &[], None);
        assert!(state.visible("WebFetch"));
        state.remove("WebFetch");
        assert!(!state.visible("WebFetch"));
    }
}
