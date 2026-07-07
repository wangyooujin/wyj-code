//! Tool trait — 所有工具的统一抽象

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use wyj_api::types::ToolDefinition;

/// 工具执行结果。`content` 恒为展示/降级用文本（TUI 与不支持多模态的
/// Provider 使用）；`parts` 存在时为发给模型的结构化内容（如图片块）。
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
    pub parts: Option<Vec<wyj_api::types::ToolResultPart>>,
}

impl ToolResult {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            parts: None,
        }
    }
    pub fn err(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            parts: None,
        }
    }
    /// 结构化多块结果：content 为展示文本，parts 为发给模型的真实内容
    pub fn with_parts(
        content: impl Into<String>,
        parts: Vec<wyj_api::types::ToolResultPart>,
    ) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            parts: Some(parts),
        }
    }
}

/// AskQuestion 工具的单个选项
#[derive(Debug, Clone)]
pub struct AskQuestionOption {
    pub label: String,
    pub description: Option<String>,
}

/// AskQuestion 工具的单个问题
#[derive(Debug, Clone)]
pub struct AskQuestionSpec {
    pub question: String,
    pub header: Option<String>,
    pub multi_select: bool,
    pub options: Vec<AskQuestionOption>,
}

/// 单题的用户作答结果
#[derive(Debug, Clone)]
pub enum QuestionAnswer {
    /// 选中了若干个预设选项下标（单选时长度恒为 1）
    Selected(Vec<usize>),
    /// 选择了"其他"并输入了自由文本
    FreeText(String),
    /// 多选场景下：勾选了部分预设选项，同时也追加了"其他"自由文本
    SelectedWithFreeText(Vec<usize>, String),
}

/// 工具上下文（由运行时注入）
#[async_trait]
pub trait ToolContext: Send + Sync {
    fn cwd(&self) -> &std::path::Path;
    fn is_allowed(&self, name: &str, input: &Value) -> bool;
    /// 若当前处于工具白名单模式（如 Plan 模式），返回该白名单，供 SubAgent 等
    /// 派生的子 Agent 继承同样的限制；否则返回 None（不受限）。
    fn allowed_tools(&self) -> Option<std::collections::HashSet<String>> {
        None
    }
    /// 发起一次多题访谈并等待用户完成（TUI 模式下弹面板，headless 返回 None）。
    /// 返回 None 表示用户整体取消；Some(answers) 长度与 questions 长度一致，按序对应。
    async fn ask_questions(&self, _questions: &[AskQuestionSpec]) -> Option<Vec<QuestionAnswer>> {
        None
    }
    /// 请求退出 plan 模式并等待用户批准，plan 为完整计划文本（Markdown）
    /// （headless 自动批准返回 true）
    async fn exit_plan_mode(&self, _plan: &str) -> bool {
        true
    }
    /// 逐调用工具权限确认：在执行一个 `needs_permission` 的工具前调用，
    /// 返回 true 表示放行、false 表示用户拒绝。`summary` 为展示给用户的操作摘要
    /// （如 bash 命令、目标文件）。默认放行（headless / 子 Agent 无 UI 时不阻塞）。
    async fn confirm_tool(&self, _name: &str, _summary: &str) -> bool {
        true
    }
}

/// 工具抽象
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn definition(&self) -> ToolDefinition;
    fn needs_permission(&self, _input: &Value) -> bool {
        false
    }
    /// 权限确认对话框正文里展示的操作摘要（如 bash 命令、目标文件路径）。
    /// 默认回工具名；`needs_permission` 为 true 的工具应覆盖以给出更具体的内容。
    fn action_summary(&self, _input: &Value) -> String {
        self.name().to_string()
    }
    /// 同一轮 assistant 消息里的多个此工具调用是否可以并发执行。
    /// 默认 false（顺序执行）；SubAgent 等自身即独立任务的工具返回 true。
    fn parallel_safe(&self) -> bool {
        false
    }
    async fn run(&self, input: Value, ctx: &dyn ToolContext) -> Result<ToolResult>;
}
