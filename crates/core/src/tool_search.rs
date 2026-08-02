//! ToolSearch 与按会话 sticky 的 lazy tool schema。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use wyj_api::types::ToolDefinition;

use crate::tool::{Tool, ToolContext, ToolResult};

#[derive(Clone)]
pub struct LazyToolState {
    inner: Arc<RwLock<LazyToolInner>>,
}

struct LazyToolInner {
    catalog: HashMap<String, ToolDefinition>,
    core: HashSet<String>,
    sticky: HashMap<String, u64>,
    current_turn: u64,
    top_k: usize,
    sticky_turns: u64,
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
        self.inner
            .write()
            .unwrap()
            .catalog
            .insert(definition.name.clone(), definition);
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

    fn search(&self, query: &str, limit: usize) -> Vec<ToolDefinition> {
        let query = query.trim().to_ascii_lowercase();
        let terms: Vec<&str> = query.split_whitespace().collect();
        let mut inner = self.inner.write().unwrap();
        let mut ranked: Vec<(i32, ToolDefinition)> = inner
            .catalog
            .values()
            .filter(|definition| definition.name != "ToolSearch")
            .filter_map(|definition| {
                let name = definition.name.to_ascii_lowercase();
                let description = definition.description.to_ascii_lowercase();
                let mut score = 0;
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
                }
                (score > 0).then(|| (score, definition.clone()))
            })
            .collect();
        ranked.sort_by(|(score_a, def_a), (score_b, def_b)| {
            score_b
                .cmp(score_a)
                .then_with(|| def_a.name.cmp(&def_b.name))
        });
        let definitions: Vec<ToolDefinition> = ranked
            .into_iter()
            .take(limit.clamp(1, inner.top_k))
            .map(|(_, definition)| definition)
            .collect();
        let current = inner.current_turn;
        inner.sticky.extend(
            definitions
                .iter()
                .map(|definition| (definition.name.clone(), current)),
        );
        definitions
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
                    "limit": {"type": "integer", "minimum": 1, "maximum": 12}
                },
                "additionalProperties": false
            }),
            native: None,
        }
    }

    async fn run(&self, input: Value, _ctx: &dyn ToolContext) -> Result<ToolResult> {
        let input: SearchInput = serde_json::from_value(input)?;
        let matches = self
            .state
            .search(&input.query, input.limit.unwrap_or(usize::MAX));
        if matches.is_empty() {
            return Ok(ToolResult::ok(
                "No matching tools. Refine the capability query; do not guess a hidden tool name."
                    .to_string(),
            ));
        }
        let compact: Vec<Value> = matches
            .into_iter()
            .map(|definition| {
                serde_json::json!({
                    "name": definition.name,
                    "description": definition.description,
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
        let found = state.search("shell command", 3);
        assert_eq!(found[0].name, "Bash");
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
        state.search("Bash", 8);
        assert!(state.visible("Bash"));
        state.begin_task_turn();
        state.mark_used("Bash");
        state.begin_task_turn();
        state.begin_task_turn();
        assert!(state.visible("Bash"));
        state.begin_task_turn();
        assert!(!state.visible("Bash"));

        state.search("WebFetch", 8);
        assert!(state.visible("WebFetch"));
        state.remove("WebFetch");
        assert!(!state.visible("WebFetch"));
    }
}
