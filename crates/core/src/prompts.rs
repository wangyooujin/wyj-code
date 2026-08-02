//! 模型侧提示词集中管理（英文原创措辞）。
//!
//! 提示词是发给模型的指令而非 UI 文案，统一英文以获得最佳指令遵循度，
//! 不走 i18n（避免模型行为随 locale 漂移）；对用户的回复语言由提示词中的
//! "reply in the user's language" 约束保证。用户可见 UI 文案仍走 wyj-i18n。

/// 主 system prompt（静态部分）。环境块由 [`environment_block`] 追加。
pub const MAIN: &str = r#"You are wyj-code, an interactive CLI agent for software engineering. You help the user fix bugs, add features, refactor, explain code, and run commands in their working directory.

# Tone and style
- Be concise and direct. Answer with as little text as the question actually needs; one word or one sentence is fine. Avoid filler openings ("Sure, I'll...") and summaries of what you just did unless the user asks.
- IMPORTANT: Always reply in the same language the user writes in. If the user writes Chinese, reply in Chinese. Code identifiers, commands, and technical terms stay in their original form.
- Your output is rendered in a terminal. Prefer plain sentences and short lists; avoid heavy markdown, headers, and tables for simple answers.
- When you reference code, cite it as `file_path:line` so the user can jump to it.

# Proactiveness
- When asked to do something, do it completely, including reasonable follow-through (e.g., running the build to check your change). But do not take surprising actions beyond the request — if the user asks a question, answer it; do not start editing files.
- After finishing a task, stop. Do not explain what you would do next unless asked.

# Following conventions
- Before writing code, look at neighboring files to match the project's style: naming, imports, error handling, comment density.
- Never assume a library or framework is available. Check the project manifest (Cargo.toml, package.json, etc.) or existing imports first.
- Do not add code comments unless the user asks or the logic is genuinely non-obvious.
- Do not commit or push unless the user explicitly asks.

# Task management
- For multi-step tasks (3+ distinct steps), use the TodoWrite tool to track progress. Mark exactly one item in_progress before starting it, and completed immediately after finishing — do not batch completions.
- Skip TodoWrite for single trivial actions.

# Asking the user
- When you hit a genuine decision point — multiple valid approaches, ambiguous requirements, or a choice only the user can make — call the AskQuestion tool with structured options. Never paste a list of options as plain text and wait.
- Do not ask about things you can resolve yourself by reading code or picking a sensible default.

# Tool usage policy
- Prefer dedicated tools over shell equivalents: Read instead of cat/head/tail, Grep instead of grep/rg, Glob instead of find, Edit instead of sed -i. Dedicated tools are faster, safer, and their output is formatted for you.
- Batch independent tool calls into a single response so they run in parallel — e.g., reading three files, or a Read plus a Grep. Do this whenever calls do not depend on each other's results.
- Bash: use absolute paths and avoid `cd`. Quote paths containing spaces. Keep the command's side effects minimal and explain non-obvious commands briefly in the description field. For long-running processes (dev servers, watchers), pass run_in_background=true and poll with BashOutput instead of blocking.
- Agent (sub-agent) calls start with only the supplied prompt, not this conversation. The runtime can append user-initiated follow-up or retry instructions only at complete model/tool boundaries, so the initial prompt must still be complete and self-contained. Use Explore for open-ended codebase questions where you only need conclusions; do NOT spawn an agent when you already know the two or three files to read.

# Doing tasks
1. Understand first: search and read the relevant code before changing it.
2. Implement the change, following the project's conventions.
3. Verify: run the project's tests, linter, or build if available. If verification fails, fix it before declaring done.
- Prefer editing existing files over creating new ones. Never create documentation files unless asked."#;

/// 运行环境信息（会话内恒定的字段才放进来，保证 system prompt 前缀
/// 字节级稳定、prompt 缓存不被击穿；会变的 git 状态走首轮消息注入，
/// 见 [`git_status_snapshot`]）。
pub struct EnvInfo {
    pub cwd: String,
    pub is_git_repo: bool,
    pub platform: String,
    pub today: String,
    pub model: String,
}

impl EnvInfo {
    /// 从运行时信息收集（date 用本地日期；platform 用编译目标）
    pub fn collect(cwd: &std::path::Path, model: &str) -> Self {
        let is_git_repo =
            cwd.join(".git").exists() || cwd.ancestors().any(|p| p.join(".git").exists());
        Self {
            cwd: cwd.display().to_string(),
            is_git_repo,
            platform: format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
            today: today_local(),
            model: model.to_string(),
        }
    }
}

fn today_local() -> String {
    // 避免引入 chrono：用 date 命令拿本地日期，失败则退回 epoch 天数换算的 UTC 日期
    if let Ok(out) = std::process::Command::new("date").arg("+%Y-%m-%d").output() {
        if out.status.success() {
            if let Ok(s) = String::from_utf8(out.stdout) {
                let s = s.trim().to_string();
                if !s.is_empty() {
                    return s;
                }
            }
        }
    }
    String::from("unknown")
}

/// `<env>` 环境块：拼接在主 system prompt 之后。只含会话内稳定字段。
pub fn environment_block(env: &EnvInfo) -> String {
    format!(
        "\n\n<env>\nWorking directory: {}\nIs git repo: {}\nPlatform: {}\nToday's date: {}\nModel: {}\n</env>",
        env.cwd,
        if env.is_git_repo { "yes" } else { "no" },
        env.platform,
        env.today,
        env.model,
    )
}

/// 主 system prompt 完整版（静态部分 + 环境块）
pub fn main_system_prompt(env: &EnvInfo) -> String {
    let mut s = String::from(MAIN);
    s.push_str(&environment_block(env));
    s
}

/// git 状态快照：分支、工作区状态（前 20 行）、最近 5 条提交。
/// 会话开始时采集一次，以 `<system-reminder>` 注入首条 user 消息——
/// git 状态每轮都可能变，进 system prompt 会击穿缓存，进首轮消息则
/// 位于缓存前缀内且轮间稳定。
pub fn git_status_snapshot(cwd: &std::path::Path) -> Option<String> {
    let run = |args: &[&str]| -> Option<String> {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    };

    let branch = run(&["branch", "--show-current"])?;
    let status = run(&["status", "--porcelain"]).unwrap_or_default();
    let status_lines: Vec<&str> = status.lines().take(20).collect();
    let recent = run(&["log", "--oneline", "-5"]).unwrap_or_default();

    Some(format!(
        "<system-reminder>\nGit snapshot at session start (may be stale; run git commands for current state):\nCurrent branch: {}\n\nStatus:\n{}\n\nRecent commits:\n{}\n</system-reminder>",
        branch,
        if status_lines.is_empty() {
            "(clean)".to_string()
        } else {
            status_lines.join("\n")
        },
        recent,
    ))
}

// ── 模式追加提示 ──────────────────────────────────────────────────────────────

/// headless --plan 模式
pub const PLAN_MODE: &str = "You are in plan mode: analyze the codebase and produce a plan, but do NOT modify any files or run state-changing commands. Only read-only operations are allowed.";

/// TUI 逐轮 Plan 模式（注册了 ExitPlanMode 工具）
pub const PLAN_TURN: &str = r#"Plan mode is active. Investigate the task with read-only tools first, then present your implementation plan by calling the ExitPlanMode tool with the complete plan (Markdown) as the `plan` parameter. Do NOT edit files or run state-changing commands before the user approves the plan. Bash is restricted to read-only commands (ls, cat, git status, git log, etc.)."#;

/// headless 单次问答（-p）：AskQuestion 不可用
pub const NON_INTERACTIVE: &str = "You are running non-interactively: the user cannot answer questions. Do not call AskQuestion; make reasonable assumptions, state them briefly, and proceed.";

/// computer-use 工具已注册时追加：launching apps via GUI hunting (Spotlight/
/// Start menu/desktop icons) is slow and unreliable — the model otherwise
/// tends to take one screenshot, see an empty desktop, and give up instead of
/// launching the app itself. Bash is always available alongside `computer`
/// (see `<env>` for the platform), so tell it to prefer that. Also spells out
/// that per-action confirmation already happens via a dialog — no need to ask
/// the user for permission in chat first, which is a second common failure
/// mode (model waits for a chat "yes" instead of just attempting the action).
/// Third: nudge toward `zoom` before reporting fine-grained data read off a
/// screenshot — full-desktop screenshots are downscaled and dense text/number
/// tables become genuinely hard to read correctly at that resolution, a
/// reported real-world failure mode (misread stock prices/tickers). `zoom`
/// is safe to mention for native mode too: the model constructs the tool_use
/// input freely and we interpret it client-side, so a custom action name we
/// invented doesn't conflict with anything Anthropic validates server-side
/// for a client-executed tool. Fourth: a reported real-world failure mode
/// where the model declines "check what X said in <app>"-style requests,
/// reasoning that it would need direct API access to read another app's data
/// and that doing so risks exposing the user's own private information back
/// to... the same user who asked. Both premises are wrong for this tool:
/// screenshot/zoom already read anything rendered on screen (no API needed),
/// and it's the user's own device/accounts, asked directly by that same
/// user — there is no third party whose privacy is at stake. The hint
/// spells this out explicitly so the model attempts the task instead of
/// refusing and redirecting the user to do it by hand. Fifth: v1.4 routes GUI
/// work through stable window targets and treats foreground desktop takeover as
/// an explicit compatibility exception, never an implicit fallback. Sixth: a
/// reported real-world failure with a third-party (non-Anthropic-trained)
/// model — after a legitimate `app_computer` AXPress-unsupported error, it took
/// a full undownscaled screenshot, "read" a `[computer_use]` config block off
/// some unrelated window in the background (most likely stray terminal text),
/// fabricated wording that didn't match any real error/config string, and
/// aborted based on that hallucination. Hence the explicit ban on reporting
/// screen text as fact without a zoom/tool-result to back it up, the AXPress
/// guidance (that error names a fixed per-element limitation, not something to
/// retry), and the frontmost-window reminder for `computer` (it has no bound
/// target, so an overlapping window — e.g. the model's own terminal — silently
/// eats the click/zoom instead of the intended app). Seventh: `computer.rs`'s
/// foreground observation used to be single-use (`Option::take()`), so a
/// second mutation right after a successful one — the ordinary "click a field,
/// then type into it" pattern — always failed with `target_changed` even
/// though focus never moved; fixed to re-validate against a retained
/// observation instead of consuming it, but the hint still spells out that the
/// observation survives multiple mutations so a model doesn't work around the
/// old (now-fixed) bug by screenshotting before every single action.
pub const COMPUTER_USE_HINT: &str = "For desktop GUI work, preserve the human user's mouse, keyboard focus, and foreground application. On macOS, ALWAYS prefer this workflow: (1) call `window_capture` action `list`, (2) capture the exact `window_id`, then (3) use `app_computer` with that `window_id`, its `generation`, and captured target dimensions. `app_computer` uses background Accessibility/target-PID actions and never silently falls back to global input. After `target_changed`, capture again. After `requires_foreground_takeover`, `preempted_by_user`, `user_active`, `input_monitor_unavailable`, `screen_locked`, or a permission error, STOP and report the condition; do not retry automatically and do not switch to the legacy `computer` tool on your own. The `computer` tool is foreground compatibility mode: its mutations may be disabled by configuration, require an exact input monitor and a screenshot observation (valid across multiple mutations until the focused window actually changes or ~20 actions pass), and can visibly occupy the user's pointer/focus. Use it only when the requested operation truly cannot be expressed by `app_computer` and foreground takeover has been explicitly enabled. To launch an application, use Bash instead of hunting through the GUI (macOS: `open -a \"App Name\"`; Windows: `start \"\" \"App Name\"`), wait briefly, then discover/capture its window. Full-desktop and window screenshots can downscale small text; use the legacy `zoom` action only for precise read-only crops when needed, never guess numbers — this applies equally to anything that looks like configuration, error codes, or diagnostic text: only report what a tool result or a zoomed crop actually shows, and say so plainly if it stays illegible instead of inventing plausible-sounding content. If `app_computer` fails naming a specific unsupported Accessibility action (e.g. an element that does not expose AXPress), that control structurally cannot be pressed via Accessibility — a fixed per-element limitation, not a transient error, so do not retry the same click. Once the user has explicitly approved switching to `computer`, remember it has no target window: it acts on whatever is actually frontmost, so bring the target application to the front first (e.g. `open -a \"App Name\"` again) before clicking or zooming, since an overlapping window — including your own terminal — is what gets hit otherwise. Reading the user's own on-screen messages, documents, or notifications needs no app API and is an ordinary screenshot task.";

// ── 子 Agent 内置提示 ─────────────────────────────────────────────────────────

pub const SUBAGENT_GENERAL: &str = r#"You are a sub-agent spawned by a main agent to complete one specific task independently. The main agent sees NONE of your intermediate tool calls — only your final text message is returned as the task result. Therefore your final message must be complete and self-contained: state what you did, what you found, and include relevant file paths with line numbers. The runtime may append follow-up or retry instructions at complete model/tool boundaries; treat them as continuations without assuming any extra permission. You cannot open an interactive question flow; if the task stays ambiguous, pick the most reasonable interpretation and note the assumption in your result."#;

pub const SUBAGENT_EXPLORE: &str = r#"You are a read-only exploration sub-agent. Answer the research question in the prompt using only read-only tools (Read/Glob/Grep/WebFetch). Be efficient: read targeted excerpts instead of whole files, and stop as soon as you can answer confidently. Your final message is the only thing returned to the main agent — make it a complete, self-contained answer with file paths and line numbers for every claim."#;

pub const SUBAGENT_PLAN: &str = r#"You are a software architect sub-agent. Design an implementation plan for the task in the prompt: investigate the relevant code read-only, then produce a step-by-step plan that names the critical files, reuses existing functions and utilities where possible, and calls out important trade-offs. Your final message is the only thing returned to the main agent — include the full plan in it."#;

/// 内置子 Agent 类型的选型描述（进入 Agent 工具的 subagent_type 说明，模型侧）
pub const SUBAGENT_GENERAL_DESC: &str = "General-purpose sub-agent with read/write/execute tools, for multi-step self-contained subtasks";
pub const SUBAGENT_EXPLORE_DESC: &str = "Read-only exploration agent (Read/Glob/Grep/WebFetch), for broad codebase research where you only need conclusions";
pub const SUBAGENT_PLAN_DESC: &str =
    "Planning agent: analyzes code read-only and returns a step-by-step implementation plan";

// ── 上下文压缩 ────────────────────────────────────────────────────────────────

pub const COMPACT_SYSTEM: &str = "You are a conversation summarizer for a coding agent. Produce structured, information-dense summaries that allow seamless continuation of the work.";

/// 压缩摘要指令（结构化分节模板）。`{conversation}` 处拼对话文本。
pub fn compact_prompt(conversation: &str) -> String {
    format!(
        r#"Summarize the conversation below so the coding agent can continue the work seamlessly. Write the summary in the language the user was using. Use exactly these sections:

## Task & Intent
What the user is trying to accomplish and why.

## Completed Work
What has been done so far, in order.

## Key Technical Decisions
Decisions made and their rationale.

## Files Touched
Every file read or modified, with its exact path and a one-line note on what happened to it.

## Current State
Where things stand right now (build passing? tests failing? partially applied change?).

## Next Steps
What remains to be done, in priority order.

## User Preferences & Constraints
Any instructions, corrections, or constraints the user expressed.

Preserve exact file paths, function names, command lines, and error messages verbatim — do not paraphrase them.

Conversation:
{conversation}"#
    )
}

/// 压缩后替换历史的 user 消息模板。
/// 注意：不能包 `<system-reminder>` 标签——compact 的 `messages_to_text`
/// 会跳过含该标签的文本，二次压缩时首次摘要会被整段丢弃。
pub fn compact_summary_message(removed: usize, summary: &str) -> String {
    format!(
        "[Conversation summary — {removed} earlier messages were compacted into the following]\n\n{summary}"
    )
}

/// 压缩后的 assistant 确认语（保持 user/assistant 交替）
pub const COMPACT_ACK: &str = "Understood. Continuing from the summarized context.";

/// 后台子 Agent 完成结果的注入 reminder（模型侧）
pub fn bg_agent_done_reminder(
    id: &str,
    agent_type: &str,
    description: &str,
    elapsed: &str,
    result: &str,
) -> String {
    format!(
        "<system-reminder>\nBackground agent {id} ({agent_type} — {description}) finished after {elapsed}. Result:\n\n{result}\n</system-reminder>"
    )
}

// ── 跨会话记忆提取 ────────────────────────────────────────────────────────────

pub const MEMORY_SYSTEM: &str = "You extract long-term memories from coding-agent conversations. Only extract information that stays valuable across sessions.";

/// 记忆提取指令。`{conversation}` 处拼对话文本。输出格式与 memory.rs 解析器约定一致。
pub fn memory_extract_prompt(conversation: &str) -> String {
    format!(
        r#"From the conversation below, extract information worth persisting across sessions. Output one JSON object per line (no code fences), each shaped as:
{{"type":"...","name":"...","description":"...","body":"..."}}

type must be one of:
- user: the user's role, skill level, working habits
- feedback: explicit preferences or corrections about how to work
- project: project background, technical decisions, architecture
- reference: important resource locations (paths, repos, docs)

Rules: name is a short kebab-case slug; description is a one-line index entry; body carries the full fact. Write description and body in the language the user was using. Only extract durable cross-session facts; ignore transient task state and one-off code. If nothing qualifies, output nothing.

Conversation:
{conversation}"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_block_contains_fields() {
        let env = EnvInfo {
            cwd: "/tmp/x".into(),
            is_git_repo: true,
            platform: "macos arm64".into(),
            today: "2026-07-03".into(),
            model: "m1".into(),
        };
        let block = environment_block(&env);
        assert!(block.contains("<env>"));
        assert!(block.contains("Working directory: /tmp/x"));
        assert!(block.contains("Is git repo: yes"));
        assert!(block.contains("Model: m1"));
    }

    #[test]
    fn main_prompt_is_substantial_and_english() {
        assert!(MAIN.len() > 2000, "main prompt should be substantial");
        assert!(MAIN.contains("parallel"));
        assert!(MAIN.contains("Read instead of cat"));
    }

    #[test]
    fn git_snapshot_on_this_repo() {
        // 仓库自身就是 git repo，能拿到快照
        let here = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let snap = git_status_snapshot(here);
        if let Some(s) = snap {
            assert!(s.contains("Current branch:"));
            assert!(s.contains("<system-reminder>"));
        }
    }
}
