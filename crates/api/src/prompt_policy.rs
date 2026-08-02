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
