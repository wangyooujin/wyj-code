//! 友好错误类型。

use thiserror::Error;

#[derive(Debug, Error)]
pub enum WyjError {
    #[error("未找到 profile `{0}`。可用 profile:{1}")]
    ProfileNotFound(String, String),

    #[error("未设置默认 profile。用 `wyj-code default <name>` 设置。可用 profile:{0}")]
    NoDefaultProfile(String),

    #[error("已存在同名 profile `{0}`,用 -f 覆盖")]
    ProfileExists(String),

    #[error("未配置任何 profile。运行 `wyj-code import` 从 zshrc 导入,或 `wyj-code add` 新增")]
    NoProfiles,

    #[error("未找到 claude 可执行文件。安装 Claude Code CLI,或在 profiles.toml 设置 `claude_path`")]
    ClaudeNotFound,

    #[error("配置文件 {path} 解析失败: {err}")]
    TomlParse { path: String, err: String },
}
