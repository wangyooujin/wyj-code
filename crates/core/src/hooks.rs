//! Hooks 生命周期自动化：`.claude/settings.json` 声明的 shell 命令在
//! PreToolUse/PostToolUse/UserPromptSubmit/Stop 四个时机被执行，对齐真
//! Claude Code 的配置格式与 stdin/stdout 执行协议。
//!
//! 三源合并（用户级 `~/.claude/settings.json` → 项目级
//! `<git-root>/.claude/settings.json` → `<git-root>/.claude/settings.local.json`）
//! 是纯拼接、不覆盖：同一事件下三源的 hook 列表按来源顺序依次追加、依次执行，
//! 与真 CC 语义一致（项目/本地配置是对用户级配置的补充，不是替换）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::claude_md::find_git_root;

const DEFAULT_TIMEOUT_SECS: u64 = 60;

fn default_hook_type() -> String {
    "command".to_string()
}

/// 一份 `.claude/settings.json` 里 `hooks` 键的解析结果，key 为事件名
/// （`"PreToolUse"`/`"PostToolUse"`/`"UserPromptSubmit"`/`"Stop"`）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct HooksSettings {
    pub hooks: HashMap<String, Vec<HookMatcherEntry>>,
}

impl HooksSettings {
    /// Append another source without replacing earlier user/project hooks.
    pub fn append(&mut self, incoming: HooksSettings) {
        for (event, mut entries) in incoming.hooks {
            self.hooks.entry(event).or_default().append(&mut entries);
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct HookMatcherEntry {
    /// 对工具名的正则；`None` 或字段缺失 = 匹配全部。PreToolUse/PostToolUse
    /// 生效，UserPromptSubmit/Stop 无工具维度，matcher 恒被忽略。
    #[serde(default)]
    pub matcher: Option<String>,
    #[serde(default)]
    pub hooks: Vec<HookCommand>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HookCommand {
    #[serde(rename = "type", default = "default_hook_type")]
    pub hook_type: String,
    pub command: String,
    /// 秒，缺省 60。
    #[serde(default)]
    pub timeout: Option<u64>,
}

/// 单个 hook（或一次 `HookRunner::run` 调用聚合后）的判定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookOutcome {
    /// 未命中任何 hook，或命中但无特殊语义：按原有逻辑放行。
    Passthrough,
    /// 跳过既有权限确认，直接执行（仅 PreToolUse 有意义）。
    Approve,
    /// 阻断，原因文本回灌给模型/用户。
    Block(String),
    /// 不阻断，但要追加上下文或让循环继续一轮。
    Continue { context: Option<String> },
}

#[derive(Debug, Serialize)]
struct HookPayload<'a> {
    session_id: Option<&'a str>,
    cwd: String,
    hook_event_name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_input: Option<&'a Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_response: Option<&'a Value>,
}

#[derive(Debug, Default, Deserialize)]
struct HookStdout {
    #[serde(default)]
    decision: Option<String>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default, rename = "additionalContext")]
    additional_context: Option<String>,
    #[serde(default, rename = "continue")]
    continue_: Option<bool>,
}

/// 已加载的 hooks 配置 + 执行器。`enabled == false`（`--no-hooks`）时
/// `run()` 直接短路返回 `Passthrough`，不解析不执行。
pub struct HookRunner {
    settings: HooksSettings,
    enabled: bool,
}

impl HookRunner {
    pub fn load(cwd: &Path, enabled: bool) -> Self {
        Self {
            settings: load_effective_hooks(cwd),
            enabled,
        }
    }

    /// Load Claude-compatible settings and append trusted plugin hook contributions.
    /// Plugin hooks never replace user/project hooks; they run afterwards in catalog order.
    pub fn load_with_additional(
        cwd: &Path,
        enabled: bool,
        additional: impl IntoIterator<Item = HooksSettings>,
    ) -> Self {
        let mut settings = load_effective_hooks(cwd);
        for incoming in additional {
            settings.append(incoming);
        }
        Self { settings, enabled }
    }

    /// 直接从已有 `HooksSettings` 构造，跳过磁盘读取（供 `agent.rs` 等上层
    /// 模块的集成测试直接装配一个已知 hook 配置）。
    #[cfg(test)]
    pub(crate) fn from_settings(settings: HooksSettings, enabled: bool) -> Self {
        Self { settings, enabled }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn settings(&self) -> &HooksSettings {
        &self.settings
    }

    /// 合并后是否存在任何 hook 配置（用于启动期一次性安全提示）。
    pub fn has_any(&self) -> bool {
        self.settings
            .hooks
            .values()
            .any(|v| v.iter().any(|e| !e.hooks.is_empty()))
    }

    pub fn total_hook_count(&self) -> usize {
        self.settings
            .hooks
            .values()
            .flat_map(|v| v.iter())
            .map(|e| e.hooks.len())
            .sum()
    }

    /// 跑一次事件对应的 hooks。`tool_name` 为 `None` 表示无工具维度的事件
    /// （UserPromptSubmit/Stop），matcher 恒视为命中。命中的多个 hook 按
    /// 来源/entry 顺序串行执行；任一 `Block` 立即短路返回；非 Block 的结果
    /// （`Approve`/`Continue`）不短路，允许后续 hook 仍有机会 `Block`，
    /// 多个非 Block 结果里以最后一个为准。
    #[allow(clippy::too_many_arguments)]
    pub async fn run(
        &self,
        event: &str,
        tool_name: Option<&str>,
        session_id: Option<&str>,
        cwd: &Path,
        tool_input: Option<&Value>,
        tool_response: Option<&Value>,
    ) -> HookOutcome {
        if !self.enabled {
            return HookOutcome::Passthrough;
        }
        let entries = match self.settings.hooks.get(event) {
            Some(e) if !e.is_empty() => e,
            _ => return HookOutcome::Passthrough,
        };

        let payload = HookPayload {
            session_id,
            cwd: cwd.to_string_lossy().to_string(),
            hook_event_name: event,
            tool_name,
            tool_input,
            tool_response,
        };
        let payload_json = match serde_json::to_string(&payload) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("hooks: payload 序列化失败: {e}");
                return HookOutcome::Passthrough;
            }
        };

        let mut pending = HookOutcome::Passthrough;
        for entry in entries {
            if !matcher_hits(entry.matcher.as_deref(), tool_name) {
                continue;
            }
            for cmd in &entry.hooks {
                if cmd.hook_type != "command" {
                    tracing::warn!("hooks: 不支持的 hook type `{}`，已跳过", cmd.hook_type);
                    continue;
                }
                match run_one(cmd, &payload_json).await {
                    HookOutcome::Block(reason) => return HookOutcome::Block(reason),
                    outcome @ (HookOutcome::Approve | HookOutcome::Continue { .. }) => {
                        pending = outcome;
                    }
                    HookOutcome::Passthrough => {}
                }
            }
        }
        pending
    }
}

fn matcher_hits(matcher: Option<&str>, tool_name: Option<&str>) -> bool {
    match (matcher, tool_name) {
        (None, _) => true,
        (Some(_), None) => true,
        (Some(pattern), Some(name)) => regex::Regex::new(pattern)
            .map(|re| re.is_match(name))
            .unwrap_or_else(|e| {
                tracing::warn!("hooks: matcher 正则编译失败 `{pattern}`: {e}");
                false
            }),
    }
}

async fn run_one(cmd: &HookCommand, payload_json: &str) -> HookOutcome {
    let timeout_secs = cmd.timeout.unwrap_or(DEFAULT_TIMEOUT_SECS);
    let mut process = build_command(&cmd.command);
    process
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match process.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("hooks: 启动命令失败 `{}`: {e}", cmd.command);
            return HookOutcome::Passthrough;
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(payload_json.as_bytes()).await;
    }

    let output =
        match tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait_with_output())
            .await
        {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => {
                tracing::warn!("hooks: 命令执行失败 `{}`: {e}", cmd.command);
                return HookOutcome::Passthrough;
            }
            Err(_) => {
                tracing::warn!("hooks: 命令超时（{timeout_secs}s）`{}`", cmd.command);
                return HookOutcome::Passthrough;
            }
        };

    let stdout_text = String::from_utf8_lossy(&output.stdout);
    if let Ok(parsed) = serde_json::from_str::<HookStdout>(stdout_text.trim()) {
        let blocked =
            parsed.decision.as_deref() == Some("block") || parsed.continue_ == Some(false);
        if blocked {
            return HookOutcome::Block(parsed.reason.unwrap_or_default());
        }
        if parsed.decision.as_deref() == Some("approve") {
            return HookOutcome::Approve;
        }
        if let Some(ctx) = parsed.additional_context.filter(|c| !c.is_empty()) {
            return HookOutcome::Continue { context: Some(ctx) };
        }
        return HookOutcome::Passthrough;
    }

    match output.status.code() {
        Some(0) => HookOutcome::Passthrough,
        Some(2) => {
            let stderr_text = String::from_utf8_lossy(&output.stderr).trim().to_string();
            HookOutcome::Block(stderr_text)
        }
        other => {
            tracing::warn!("hooks: 命令 `{}` 以非零状态退出: {:?}", cmd.command, other);
            HookOutcome::Passthrough
        }
    }
}

#[cfg(unix)]
fn build_command(command: &str) -> Command {
    let mut c = Command::new("sh");
    c.arg("-c").arg(command);
    c
}

#[cfg(windows)]
fn build_command(command: &str) -> Command {
    let mut c = Command::new("cmd");
    c.arg("/C").arg(command);
    c
}

/// 三源合并后的生效 hooks 配置。每次调用都重新读盘（与 `ClaudeMdLoader`
/// 同一哲学：内容不缓存，保证运行期间编辑立即生效）。
pub fn load_effective_hooks(cwd: &Path) -> HooksSettings {
    let mut paths = Vec::new();
    if let Ok(home) = wyj_config::claude_home_dir() {
        paths.push(home.join("settings.json"));
    }
    let root = find_git_root(cwd).unwrap_or_else(|| cwd.to_path_buf());
    paths.push(root.join(".claude").join("settings.json"));
    paths.push(root.join(".claude").join("settings.local.json"));
    merge_sources(&paths)
}

fn merge_sources(paths: &[PathBuf]) -> HooksSettings {
    let mut merged: HashMap<String, Vec<HookMatcherEntry>> = HashMap::new();
    for path in paths {
        if let Some(parsed) = load_one_settings(path) {
            for (event, mut entries) in parsed {
                merged.entry(event).or_default().append(&mut entries);
            }
        }
    }
    HooksSettings { hooks: merged }
}

fn load_one_settings(path: &Path) -> Option<HashMap<String, Vec<HookMatcherEntry>>> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let hooks_value = value.get("hooks")?.clone();
    match serde_json::from_value(hooks_value) {
        Ok(map) => Some(map),
        Err(e) => {
            tracing::warn!("hooks: 解析 {} 中的 hooks 字段失败: {e}", path.display());
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(command: &str) -> HookCommand {
        HookCommand {
            hook_type: "command".into(),
            command: command.into(),
            timeout: Some(5),
        }
    }

    fn entry(matcher: Option<&str>, hooks: Vec<HookCommand>) -> HookMatcherEntry {
        HookMatcherEntry {
            matcher: matcher.map(|s| s.to_string()),
            hooks,
        }
    }

    fn runner_with(event: &str, entries: Vec<HookMatcherEntry>) -> HookRunner {
        let mut hooks = HashMap::new();
        hooks.insert(event.to_string(), entries);
        HookRunner {
            settings: HooksSettings { hooks },
            enabled: true,
        }
    }

    #[test]
    fn append_preserves_user_project_then_plugin_order() {
        let mut settings = HooksSettings {
            hooks: HashMap::from([(
                "PreToolUse".to_string(),
                vec![
                    entry(None, vec![cmd("user")]),
                    entry(None, vec![cmd("project")]),
                ],
            )]),
        };
        settings.append(HooksSettings {
            hooks: HashMap::from([(
                "PreToolUse".to_string(),
                vec![entry(None, vec![cmd("plugin")])],
            )]),
        });
        let commands: Vec<&str> = settings.hooks["PreToolUse"]
            .iter()
            .flat_map(|entry| entry.hooks.iter())
            .map(|hook| hook.command.as_str())
            .collect();
        assert_eq!(commands, vec!["user", "project", "plugin"]);
    }

    #[tokio::test]
    async fn exit_zero_is_passthrough() {
        let r = runner_with("PreToolUse", vec![entry(None, vec![cmd("exit 0")])]);
        let outcome = r
            .run(
                "PreToolUse",
                Some("Bash"),
                None,
                Path::new("/tmp"),
                None,
                None,
            )
            .await;
        assert_eq!(outcome, HookOutcome::Passthrough);
    }

    #[tokio::test]
    async fn exit_two_blocks_with_stderr() {
        let r = runner_with(
            "PreToolUse",
            vec![entry(None, vec![cmd("echo denied 1>&2; exit 2")])],
        );
        let outcome = r
            .run(
                "PreToolUse",
                Some("Bash"),
                None,
                Path::new("/tmp"),
                None,
                None,
            )
            .await;
        assert_eq!(outcome, HookOutcome::Block("denied".to_string()));
    }

    #[tokio::test]
    async fn nonzero_non_two_exit_is_passthrough() {
        let r = runner_with("PreToolUse", vec![entry(None, vec![cmd("exit 7")])]);
        let outcome = r
            .run(
                "PreToolUse",
                Some("Bash"),
                None,
                Path::new("/tmp"),
                None,
                None,
            )
            .await;
        assert_eq!(outcome, HookOutcome::Passthrough);
    }

    #[tokio::test]
    async fn stdout_json_decision_block() {
        let r = runner_with(
            "PreToolUse",
            vec![entry(
                None,
                vec![cmd(r#"echo '{"decision":"block","reason":"nope"}'"#)],
            )],
        );
        let outcome = r
            .run(
                "PreToolUse",
                Some("Bash"),
                None,
                Path::new("/tmp"),
                None,
                None,
            )
            .await;
        assert_eq!(outcome, HookOutcome::Block("nope".to_string()));
    }

    #[tokio::test]
    async fn stdout_json_decision_approve() {
        let r = runner_with(
            "PreToolUse",
            vec![entry(None, vec![cmd(r#"echo '{"decision":"approve"}'"#)])],
        );
        let outcome = r
            .run(
                "PreToolUse",
                Some("Bash"),
                None,
                Path::new("/tmp"),
                None,
                None,
            )
            .await;
        assert_eq!(outcome, HookOutcome::Approve);
    }

    #[tokio::test]
    async fn stdout_json_additional_context() {
        let r = runner_with(
            "UserPromptSubmit",
            vec![entry(
                None,
                vec![cmd(r#"echo '{"additionalContext":"extra info"}'"#)],
            )],
        );
        let outcome = r
            .run(
                "UserPromptSubmit",
                None,
                None,
                Path::new("/tmp"),
                None,
                None,
            )
            .await;
        assert_eq!(
            outcome,
            HookOutcome::Continue {
                context: Some("extra info".to_string())
            }
        );
    }

    #[tokio::test]
    async fn stdout_json_continue_false_blocks() {
        let r = runner_with(
            "Stop",
            vec![entry(
                None,
                vec![cmd(r#"echo '{"continue":false,"reason":"keep going"}'"#)],
            )],
        );
        let outcome = r
            .run("Stop", None, None, Path::new("/tmp"), None, None)
            .await;
        assert_eq!(outcome, HookOutcome::Block("keep going".to_string()));
    }

    #[tokio::test]
    async fn matcher_regex_filters_by_tool_name() {
        let r = runner_with(
            "PreToolUse",
            vec![entry(Some("^Edit$"), vec![cmd("exit 2")])],
        );
        let miss = r
            .run(
                "PreToolUse",
                Some("Bash"),
                None,
                Path::new("/tmp"),
                None,
                None,
            )
            .await;
        assert_eq!(miss, HookOutcome::Passthrough);
        let hit = r
            .run(
                "PreToolUse",
                Some("Edit"),
                None,
                Path::new("/tmp"),
                None,
                None,
            )
            .await;
        assert!(matches!(hit, HookOutcome::Block(_)));
    }

    #[tokio::test]
    async fn multiple_hooks_short_circuit_on_first_block() {
        let r = runner_with(
            "PreToolUse",
            vec![entry(
                None,
                vec![
                    cmd(r#"echo '{"decision":"block","reason":"first"}'"#),
                    cmd(r#"echo '{"decision":"block","reason":"second"}'"#),
                ],
            )],
        );
        let outcome = r
            .run(
                "PreToolUse",
                Some("Bash"),
                None,
                Path::new("/tmp"),
                None,
                None,
            )
            .await;
        assert_eq!(outcome, HookOutcome::Block("first".to_string()));
    }

    #[tokio::test]
    async fn timeout_treated_as_passthrough() {
        let mut c = cmd("sleep 5");
        c.timeout = Some(1);
        let r = runner_with("PreToolUse", vec![entry(None, vec![c])]);
        let outcome = r
            .run(
                "PreToolUse",
                Some("Bash"),
                None,
                Path::new("/tmp"),
                None,
                None,
            )
            .await;
        assert_eq!(outcome, HookOutcome::Passthrough);
    }

    #[tokio::test]
    async fn disabled_runner_never_executes() {
        let mut r = runner_with("PreToolUse", vec![entry(None, vec![cmd("exit 2")])]);
        r.enabled = false;
        let outcome = r
            .run(
                "PreToolUse",
                Some("Bash"),
                None,
                Path::new("/tmp"),
                None,
                None,
            )
            .await;
        assert_eq!(outcome, HookOutcome::Passthrough);
    }

    #[test]
    fn merge_concatenates_across_sources_in_order() {
        let dir = std::env::temp_dir().join(format!(
            "wyj-hooks-merge-test-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let p1 = dir.join("user.json");
        let p2 = dir.join("project.json");
        std::fs::write(
            &p1,
            r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"echo user"}]}]}}"#,
        )
        .unwrap();
        std::fs::write(
            &p2,
            r#"{"hooks":{"PreToolUse":[{"matcher":"Edit","hooks":[{"type":"command","command":"echo project"}]}]}}"#,
        )
        .unwrap();

        let settings = merge_sources(&[p1, p2]);
        let entries = settings.hooks.get("PreToolUse").unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].hooks[0].command, "echo user");
        assert_eq!(entries[1].hooks[0].command, "echo project");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_files_produce_empty_settings() {
        let settings = merge_sources(&[PathBuf::from(
            "/nonexistent/wyj-hooks-test-path/settings.json",
        )]);
        assert!(settings.hooks.is_empty());
    }

    #[test]
    fn unknown_top_level_keys_are_tolerated() {
        let dir = std::env::temp_dir().join(format!(
            "wyj-hooks-tolerant-test-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("settings.json");
        std::fs::write(
            &p,
            r#"{"permissions":{"allow":["Bash"]},"hooks":{"Stop":[{"hooks":[{"type":"command","command":"echo done"}]}]}}"#,
        )
        .unwrap();

        let settings = merge_sources(&[p]);
        assert_eq!(
            settings.hooks.get("Stop").unwrap()[0].hooks[0].command,
            "echo done"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
