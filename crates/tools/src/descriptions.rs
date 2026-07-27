//! 工具描述集中管理（英文原创措辞，模型侧文本）。
//!
//! 工具描述发给模型而非用户，统一英文、不走 i18n：避免模型行为随 locale
//! 漂移，且保持 `&'static str` 零开销。用户可见的工具输出仍可本地化。

pub const READ: &str = "Reads a file from the filesystem. Returns content with line-number prefixes (`N\\tcontent`). Supports offset/limit for reading a slice of a large file — files over 200KB must be read in slices. Image files (png/jpg/gif/webp/bmp) are returned as actual images you can see. Prefer this tool over `cat`/`head`/`tail` in Bash. Multiple Read calls in one response run in parallel.";

pub const WRITE: &str = "Writes content to a file, replacing anything already there. You MUST Read an existing file before overwriting it. Parent directories are created automatically. Prefer Edit for partial changes to existing files; use Write only for new files or full rewrites.";

pub const EDIT: &str = "Performs an exact string replacement in a file. You MUST Read the file first — old_string has to match the current content exactly, including indentation and whitespace. old_string must be unique in the file, or set replace_all=true to replace every occurrence. If the match fails, re-Read the file and adjust. Prefer this over `sed -i` in Bash.";

pub const BASH: &str = "Executes a command in the system shell and returns stdout/stderr. Use it for builds, tests, git, and package managers. Do NOT use it for file reading or searching — use Read/Grep/Glob instead, they are faster and formatted for you. Use absolute paths and avoid `cd`. Quote paths containing spaces. Long-running commands are killed at the timeout (default 120s, max 600s); for processes meant to keep running (dev servers, watchers), set run_in_background=true and read output later with BashOutput. Output over 30KB is truncated keeping head and tail.";

pub const GLOB: &str = "Finds files by glob pattern (`**/*.rs`, `src/**/*.test.ts`, ...). Respects .gitignore. Returns absolute paths. Use this instead of `find` in Bash. Can run in parallel with other read-only calls.";

pub const GREP: &str = "Searches file contents with a regular expression. Respects .gitignore and skips binary files. output_mode: files_with_matches (default, paths only), content (matching lines as `path:line:text`, supports -A/-B/-C context lines), or count (per-file match counts). Use this instead of `grep`/`rg` in Bash. Can run in parallel with other read-only calls.";

pub const WEBFETCH: &str = "Fetches a URL and converts the page to plain text (HTML tags stripped, capped at 50KB). Good for docs, READMEs, and API references. Cannot execute JavaScript, so client-rendered pages may come back empty. Can run in parallel with other read-only calls.";

pub const WEBSEARCH: &str = "Searches the web and returns the top results (title, URL, and a short snippet) plus a synthesized answer when available. Use it for current events, up-to-date facts, library/API docs, or anything beyond your training cutoff; follow up with WebFetch to read a specific result in full. Can run in parallel with other read-only calls.";

pub const TODO_WRITE: &str = "Creates or replaces the structured task list shown to the user. Call it for multi-step tasks (3+ steps): once up front to plan, then again on every status change. Each call replaces the whole list, so always pass every item. Keep exactly one item in_progress at a time; mark items completed immediately when done, never in batches. status: pending | in_progress | completed; priority (optional): high | medium | low.";

pub const ASK_QUESTION: &str = "Presents the user a structured questionnaire (1-4 questions, each with 2-4 options) and waits for their answers. Use it only at genuine decision points — ambiguous requirements or choices only the user can make. Do not use it for anything you can resolve yourself from the code or sensible defaults. An \"Other\" free-text option is appended to every question automatically.";

pub const EXIT_PLAN_MODE: &str = "Call this when you are in plan mode and have finished designing the implementation plan. Pass the complete plan (Markdown) as the `plan` parameter; the user reviews it and, on approval, execution mode is enabled automatically. Do not ask for approval in prose — this tool IS the approval request.";

/// SubAgent 工具描述模板：`{types}` 处由运行时替换为可用类型列表。
pub const SUB_AGENT_TEMPLATE: &str = "Spawns an independent sub-agent to complete a task, optionally in the background (run_in_background=true returns immediately; the result is injected into the conversation when ready). Sub-agents are STATELESS and ONE-SHOT: they see only your prompt — not this conversation — and you cannot send follow-ups, so the prompt must be a complete, self-contained task description stating exactly what to investigate or do and what the final report must contain. Use them for multi-step subtasks worth isolating, or broad codebase research; do NOT spawn one when you already know the two or three files to read. Multiple Agent calls in one response run concurrently.\n\nAvailable agent types:\n{types}";

/// computer-use 工具描述：作为 Anthropic 原生工具（`native` 字段非空）注册，
/// 实际发给模型的 schema 由 Anthropic 内置，不使用这里的 description/
/// input_schema；两者仅供本地工具列表/详情展示（如 `/tools`、TUI 面板）使用。
pub const COMPUTER: &str = "Foreground compatibility computer-use tool. Read-only screenshots/zoom are available, but mutating actions synthesize global mouse/keyboard input and are disabled by default in v1.4. Prefer `window_capture` + `app_computer`, which bind actions to a stable window and do not move the user's pointer or activate an app. The action schema is built into the model by Anthropic; this description is for local display only and is not sent to the model.";

/// computer-use 工具描述模板（custom 工具模式，`{width}`/`{height}` 由
/// `computer_custom_description()` 在运行时替换为实际下采样目标分辨率）。
///
/// 这段文字**会**发给模型，且是模型理解该工具调用约定的唯一来源——不像
/// Anthropic 原生 computer 工具那样有 Claude 训练时习得的隐式调用约定兜底
/// （第三方 Anthropic 协议兼容端点，如 MiniMax/GLM/Kimi，说的是 Anthropic
/// 协议但没有这层训练），因此必须完整列出每个 action 需要哪些字段、坐标
/// 系是什么、以及"先截图再行动、行动后再截图核实"的基本工作流，不能依赖
/// 模型自行猜测。第三方模型在全屏未 zoom 截图上编造"读到"的诊断文字/配置值
/// 是已发生过的真实故障（把画面里另一个终端窗口的文字脑补成本次工具失败的
/// 原因，还编出了根本不存在的措辞），因此显式禁止"报告未经工具结果或 zoom
/// 裁剪证实的内容"；`computer` 不绑定窗口、作用于当前最前台窗口，因此切到
/// 它之前必须先把目标 App 切到最前台，否则会点到/截到叠在上面的其它窗口
/// （包括模型自己所在的终端）。
const COMPUTER_CUSTOM_TEMPLATE: &str = "LEGACY FOREGROUND COMPATIBILITY TOOL. Prefer `window_capture` + `app_computer`: those tools bind actions to a stable application window and do not move the user's pointer or activate an app. This tool takes full-desktop screenshots and, for mutations, synthesizes GLOBAL mouse/keyboard input. Foreground mutation is disabled by default and may return `requires_foreground_takeover`; never retry that error automatically and never treat it as permission to fall back from `app_computer`.\n\nScreenshots are returned at {width}x{height} pixels — every coordinate you pass (`coordinate`/`start_coordinate`/`region`) MUST be within that space and is rescaled to the real display. A mutation requires a foreground observation established by a prior `screenshot`; that observation stays valid across multiple mutations (e.g. click a field, then `type` into it) as long as the focused window genuinely hasn't changed — it does NOT need to be refreshed before every single action. Take a new `screenshot` only after `target_changed`, or after roughly 20 actions (you'll be told to). Exact external-input monitoring gives the human absolute priority; stop on `target_changed`, `user_active`, `preempted_by_user`, `input_monitor_unavailable`, or `screen_locked`. Do not retry any of those results automatically.\n\nThe full-desktop screenshot is downscaled. Before reading precise small text, numbers, or anything that looks like configuration/diagnostic output, call `zoom` on the region instead of guessing — never report a value, error code, or file content as fact unless you actually read it in a tool result or a zoomed crop; say so plainly if it stays illegible instead of inventing plausible text. This tool has no target window: every action hits whatever is actually frontmost, so before clicking, dragging, or zooming on a specific app, first confirm that app is the frontmost window (re-activate it, e.g. `open -a \"App Name\"`) — an overlapping window, including your own terminal, is what gets hit otherwise. To open an application, prefer Bash and then observe it. Reading the user's own on-screen messages/documents is an ordinary screenshot task and needs no application API.\n\nActions:\n- `screenshot`: capture the desktop and (re)establish the foreground observation later mutations are validated against.\n- `zoom`: high-fidelity crop; requires `region: [x0, y0, x1, y1]`.\n- `cursor_position`: report the global cursor in the {width}x{height} space.\n- `wait`: pause; optional `duration` (default 1, max 5 seconds).\n- `mouse_move`: global pointer move; requires `coordinate`.\n- `left_click` / `right_click` / `middle_click` / `double_click`: global click; optional `coordinate`, optional modifier combo in `text`.\n- `left_click_drag`: global drag; requires end `coordinate`, optional `start_coordinate`.\n- `key`: global key combo in `text`.\n- `type`: global text in `text`, sent in small preemptible chunks.\n- `scroll`: global scroll; requires `scroll_direction`, optional `coordinate` and `scroll_amount`.";

/// 按当前截图目标分辨率渲染 [`COMPUTER_CUSTOM_TEMPLATE`]。
pub fn computer_custom_description(target_width: u32, target_height: u32) -> String {
    COMPUTER_CUSTOM_TEMPLATE
        .replace("{width}", &target_width.to_string())
        .replace("{height}", &target_height.to_string())
}

// ── schema 字段描述（各工具 input_schema 里复用的英文文案）──────────────────

pub const FIELD_BASH_COMMAND: &str = "The shell command to execute";
pub const FIELD_BASH_TIMEOUT: &str = "Timeout in seconds (default 120, max 600)";
pub const FIELD_BASH_DESCRIPTION: &str =
    "One short sentence describing what this command does, shown to the user";
pub const FIELD_FILE_PATH: &str =
    "Path to the file (absolute preferred; relative resolves against the working directory)";
pub const FIELD_READ_OFFSET: &str = "Line number to start reading from (0-based)";
pub const FIELD_READ_LIMIT: &str = "Maximum number of lines to read";
pub const FIELD_WRITE_CONTENT: &str = "Full content to write to the file";
pub const FIELD_EDIT_OLD: &str =
    "Exact text to replace — must match the file content byte-for-byte, including indentation";
pub const FIELD_EDIT_NEW: &str = "Replacement text";
pub const FIELD_EDIT_REPLACE_ALL: &str =
    "Replace every occurrence instead of requiring old_string to be unique (default false)";
pub const FIELD_GLOB_PATTERN: &str = "Glob pattern, e.g. \"**/*.rs\" or \"src/**/*.test.ts\"";
pub const FIELD_GLOB_PATH: &str = "Directory to search in (defaults to the working directory)";
pub const FIELD_GREP_PATTERN: &str = "Regular expression to search for";
pub const FIELD_GREP_PATH: &str =
    "File or directory to search in (defaults to the working directory)";
pub const FIELD_WEBFETCH_URL: &str = "The URL to fetch (http/https)";

pub const FIELD_WEBSEARCH_QUERY: &str =
    "The search query. Prefer specific, keyword-rich queries over full sentences.";
