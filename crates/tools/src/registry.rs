//! 工具注册表 — 按名字查找和批量列出工具

use std::collections::HashMap;
use std::sync::Arc;
use wyj_api::types::ToolDefinition;
use wyj_core::tool::Tool;

pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self { tools: HashMap::new() }
    }

    pub fn register(&mut self, tool: impl Tool + 'static) {
        self.tools.insert(tool.name().to_string(), Arc::new(tool));
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        let mut defs: Vec<_> = self.tools.values().map(|t| t.definition()).collect();
        defs.sort_by(|a, b| a.name.cmp(&b.name));
        defs
    }

    /// 构建包含所有标准工具的注册表
    pub fn standard() -> Self {
        use crate::{bash::BashTool, read::ReadTool, write::WriteTool, edit::EditTool,
                    glob::GlobTool, grep::GrepTool, webfetch::WebFetchTool};
        let mut r = Self::new();
        r.register(BashTool);
        r.register(ReadTool);
        r.register(WriteTool::default());
        r.register(EditTool);
        r.register(GlobTool);
        r.register(GrepTool);
        r.register(WebFetchTool::new());
        r
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}
