//! `wyj-code run <command>`：一次性执行一条斜杠命令 / Skill。
//!
//! 与 headless REPL 的分工：REPL 负责多轮交互（读 stdin），本命令负责
//! **脚本化的一次性调用**——不读 stdin、跑完即退出，退出码反映命令是否成功。
//!
//! 命令本体走的是和会话里敲 `/xxx` 完全相同的注册表与分发链路
//! （`CommandRegistry::dispatch`），因此 skill 的 `$ARGUMENTS` 展开、
//! `allowed-tools` / `model` frontmatter 都照常生效。

use anyhow::Result;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use wyj_commands::{standard_registry_with_skills, CommandContext, CommandResult};
use wyj_config::{AgentMode, Config};
use wyj_core::{Agent, Session};
use wyj_tools::{PermissionMode, ToolCtx};

use super::{run_one_turn, SingleTurnEnv};

/// 把 `wyj-code run` 的位置参数拼成一条命令文本。
///
/// - 命令名可省略前导 `/`（`run explain` ≡ `run /explain`）。
/// - 其余参数按原顺序用单空格拼接后传给命令（skill 的 `$ARGUMENTS`）。
pub fn normalize_command(parts: &[String]) -> String {
    let joined = parts.join(" ").trim().to_string();
    if joined.is_empty() {
        return joined;
    }
    if joined.starts_with('/') {
        joined
    } else {
        format!("/{joined}")
    }
}

/// `run` 一次执行所需的全部输入。
pub struct RunRequest<'a> {
    /// 已归一化的命令文本（`/name args`）。
    pub text: String,
    /// 主 Agent：没有 `model:` frontmatter 时用它跑这一轮。
    pub agent: Agent,
    /// `model:` frontmatter 指定临时 Profile 时用它重建 Agent。
    pub rebuild_fn: wyj_tui::RebuildFn,
    pub tool_ctx: ToolCtx,
    pub session: &'a mut Session,
    pub cfg: &'a Config,
    pub mode: &'a AgentMode,
    pub cwd: &'a Path,
    /// 已启用插件贡献的 skill 路径（全局 + 本地插件）。
    pub plugin_skill_sources: Vec<PathBuf>,
    /// 已启用插件贡献的 agent 定义路径。
    pub plugin_agent_paths: Vec<PathBuf>,
    /// 实际会连接的 MCP server 数量（面板类命令用）。
    pub effective_mcp_count: usize,
    pub hooks_enabled: bool,
    pub context_window: u32,
    pub env: &'a SingleTurnEnv,
}

/// 执行一条斜杠命令。
///
/// 只有 `RunPrompt` / `RunPromptScoped` 会真的调用模型；其余命令要么是面板类
/// （无 UI 可开），要么是会话状态操作（`/clear`、`/new`、`/compact`…）——在
/// 一次性执行里没有意义，打印一行说明后正常退出。
pub async fn execute(req: RunRequest<'_>) -> Result<()> {
    let RunRequest {
        text,
        agent,
        rebuild_fn,
        tool_ctx,
        session,
        cfg,
        mode,
        cwd,
        plugin_skill_sources,
        plugin_agent_paths,
        effective_mcp_count,
        hooks_enabled,
        context_window,
        env,
    } = req;

    // 与 headless REPL 同一份注册表：内置命令 + skill（内置 / 全局 / 插件 / 项目级），
    // 内置命令后注册、覆盖同名 skill —— 两处必须一致，否则同一条命令在
    // 「会话里敲」和「命令行跑」会命中不同的实现。
    let home_dir = std::env::var("HOME").map(PathBuf::from).unwrap_or_default();
    let disabled_skills = wyj_store::disabled_skill_names(cwd);
    let registry = standard_registry_with_skills(
        &home_dir,
        cwd,
        &disabled_skills,
        &plugin_skill_sources,
        wyj_commands::skill::ProjectSkillsGate::resolve(cwd),
    );
    let dynamic_commands: Vec<(String, String, String)> = registry
        .list()
        .iter()
        .filter(|c| c.is_dynamic())
        .map(|c| (c.name().to_string(), c.description(), c.usage()))
        .collect();

    let cmd_ctx = CommandContext {
        cwd: cwd.to_path_buf(),
        model: cfg.active_profile().model.clone(),
        terminal_width: wyj_tui::detect_terminal_width(),
        input_tokens: session.total_input_tokens,
        output_tokens: session.total_output_tokens,
        cache_read_tokens: session.total_cache_read_tokens,
        cache_write_tokens: session.total_cache_write_tokens,
        api_calls: session.api_calls,
        prompt_cache_state: session.prompt_cache_state,
        context_window: cfg.active_profile().context_window,
        estimated_tokens: session
            .context_audit
            .total()
            .max(wyj_core::estimate_tokens(&session.messages)),
        context_audit: (session.context_audit.total() > 0).then_some(session.context_audit),
        last_latency: session.latency,
        prompt_drift: session.prompt_drift_since_resume.clone(),
        compact_count: session.compact_count,
        compact_generation: session.compact_generation,
        context_edit: (
            wyj_core::context_edit::elided_count(&session.messages) as u32,
            session.context_edit_freed_tokens,
        ),
        home_dir,
        sub_input_tokens: session.sub_input_tokens,
        sub_output_tokens: session.sub_output_tokens,
        sub_cache_read_tokens: session.sub_cache_read_tokens,
        sub_cache_write_tokens: session.sub_cache_write_tokens,
        aux_input_tokens: session.aux_input_tokens,
        aux_output_tokens: session.aux_output_tokens,
        aux_cache_read_tokens: session.aux_cache_read_tokens,
        aux_calls: session.aux_calls,
        // 缓存归因：headless 路径没有「掉量」这一说（单次执行），
        // 但字段必须给值。
        cache_attribution: wyj_core::cache_attribution::attribute(
            &session.cache_samples,
            &session.cache_fingerprints,
        ),
        reasoning_tokens: session.total_reasoning_tokens,
        api_attempts: session.api_attempts,
        effective_mcp_count,
        plugin_agent_paths,
        hooks_enabled,
        dynamic_commands,
    };

    let Some(result) = registry.dispatch(&text, &cmd_ctx).await else {
        anyhow::bail!("`run` 只接受斜杠命令（以 `/` 开头），收到：{text}");
    };

    match result {
        Ok(CommandResult::Output(out)) | Ok(CommandResult::OutputPlain(out)) => {
            println!("{out}");
            Ok(())
        }
        Ok(CommandResult::RunPrompt(prompt)) => {
            session.push_user(prompt);
            run_one_turn(&agent, session, &tool_ctx, context_window, env).await
        }
        Ok(CommandResult::RunPromptScoped {
            text: body,
            allowed_tools,
            profile,
        }) => {
            // `model:` frontmatter：只影响这一轮的 Profile，不改会话的 active profile。
            let scoped_agent = match profile {
                Some(profile_name) => {
                    let mut scoped_cfg = Config::load().unwrap_or_else(|_| cfg.clone());
                    if scoped_cfg.profile_by_name(&profile_name).is_none() {
                        anyhow::bail!("未找到 Profile: {profile_name}");
                    }
                    scoped_cfg.active_profile = profile_name;
                    let scoped_model = scoped_cfg.model_for_mode(mode).to_string();
                    Arc::new(rebuild_fn(&scoped_cfg, &scoped_model)?)
                }
                None => Arc::new(agent),
            };
            // `allowed-tools`：临时把权限收紧为白名单，跑完（无论成败）都还原。
            let prev_mode = tool_ctx.permission_mode.read().unwrap().clone();
            if let Some(tools) = allowed_tools {
                let scoped: HashSet<String> = tools.into_iter().collect();
                tool_ctx.set_permission_mode(if matches!(mode, AgentMode::Plan) {
                    PermissionMode::Plan(scoped)
                } else {
                    PermissionMode::Allowlist(scoped)
                });
            }
            session.push_user(body);
            let outcome =
                run_one_turn(&scoped_agent, session, &tool_ctx, context_window, env).await;
            tool_ctx.set_permission_mode(prev_mode);
            outcome
        }
        Ok(other) => {
            println!("[run] 该命令在一次性执行模式下无实际作用：{other:?}");
            Ok(())
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_leading_slash_is_optional() {
        assert_eq!(normalize_command(&parts(&["/explain"])), "/explain");
        assert_eq!(normalize_command(&parts(&["explain"])), "/explain");
    }

    #[test]
    fn arguments_are_joined_with_single_spaces() {
        assert_eq!(
            normalize_command(&parts(&["/explain", "crates/core/src/agent.rs"])),
            "/explain crates/core/src/agent.rs"
        );
        assert_eq!(
            normalize_command(&parts(&["explain", "a", "b"])),
            "/explain a b"
        );
    }

    #[test]
    fn surrounding_whitespace_is_trimmed() {
        assert_eq!(normalize_command(&parts(&[" /explain "])), "/explain");
    }

    #[test]
    fn empty_input_stays_empty() {
        assert_eq!(normalize_command(&[]), "");
        assert_eq!(normalize_command(&parts(&["   "])), "");
    }
}
