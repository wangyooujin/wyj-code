//! 按能力组合稳定、短小的模型适配契约，避免为每个厂商复制整套 system prompt。

use crate::{ModelCapabilities, ModelQuirk, PromptDialect};

pub struct PromptPolicy;

impl PromptPolicy {
    pub fn compatibility_suffix(capabilities: &ModelCapabilities) -> &'static str {
        let single_tool = capabilities.max_tools_per_turn == 1
            || capabilities
                .quirks
                .iter()
                .any(|quirk| matches!(quirk, ModelQuirk::RequiresSingleTool));
        match (capabilities.preferred_prompt_dialect, single_tool) {
            (PromptDialect::Bilingual, true) => {
                "<model-compatibility>\nUse at most one tool call per response. Tool arguments must be one strict JSON object using only schema fields; never invent a path, command, enum value, or edit text. Wait for the tool result before continuing.\n每次回复最多调用一个工具。参数必须是仅含 schema 字段的严格 JSON 对象；不得猜测路径、命令、枚举值或编辑内容。收到工具结果后再继续。\n</model-compatibility>"
            }
            (PromptDialect::Bilingual, false) => {
                "<model-compatibility>\nTool arguments must be strict JSON objects using only schema fields. Never invent missing paths, commands, enum values, or edit text.\n工具参数必须是仅含 schema 字段的严格 JSON 对象，不得猜测缺失的路径、命令、枚举值或编辑内容。\n</model-compatibility>"
            }
            (_, true) => {
                "<model-compatibility>Use at most one tool call per response. Emit one strict JSON object containing only schema fields, and wait for its result.</model-compatibility>"
            }
            _ => "",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 放开并发后，system prompt 里的「每次回复最多调用一个工具」必须一并消失。
    /// 只翻 `parallel_tool_calls` 而留着这句，模型照样每轮只发一个工具——
    /// 这正是本次要修的「同一事实被两处各算一半」。
    #[test]
    fn parallel_capable_model_gets_no_single_tool_instruction() {
        let mut caps = ModelCapabilities::conservative(64_000, 8_192);
        caps.preferred_prompt_dialect = PromptDialect::Bilingual;
        crate::model_catalog::apply_parallel_policy(
            &mut caps,
            true,
            4,
            crate::capabilities::CapabilitySource::UserOverride,
            crate::capabilities::Confidence::High,
        );
        let suffix = PromptPolicy::compatibility_suffix(&caps);
        assert!(
            !suffix.contains("每次回复最多调用一个工具"),
            "放开并发后仍注入了单工具禁令: {suffix}"
        );
        assert!(
            !suffix.contains("at most one tool call"),
            "放开并发后仍注入了单工具禁令: {suffix}"
        );
        // 严格 JSON 的要求与并发无关，必须保留
        assert!(suffix.contains("strict JSON"));
    }

    #[test]
    fn domestic_single_tool_suffix_is_bilingual_and_compact() {
        let mut caps = ModelCapabilities::conservative(64_000, 8_192);
        caps.preferred_prompt_dialect = PromptDialect::Bilingual;
        let suffix = PromptPolicy::compatibility_suffix(&caps);
        assert!(suffix.contains("strict JSON"));
        assert!(suffix.contains("每次回复最多调用一个工具"));
        assert!(suffix.len() < 800);
    }
}
