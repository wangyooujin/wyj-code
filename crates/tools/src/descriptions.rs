//! 工具描述集中管理（英文原创措辞，模型侧文本）。
//!
//! 工具描述发给模型而非用户，统一英文、不走 i18n：避免模型行为随 locale
//! 漂移，且保持 `&'static str` 零开销。用户可见的工具输出仍可本地化。

pub const READ: &str = "Reads a file from the filesystem. Returns content with line-number prefixes (`N\\tcontent`). Supports offset/limit for reading a slice of a large file — files over 200KB must be read in slices. Image files (png/jpg/gif/webp/bmp) are returned as actual images you can see. Prefer this tool over `cat`/`head`/`tail` in Bash. Multiple Read calls in one response run in parallel.";

pub const WRITE: &str = "Writes content to a file, replacing anything already there. You MUST Read an existing file before overwriting it. Parent directories are created automatically. Prefer Edit for partial changes to existing files; use Write only for new files or full rewrites.";

pub const EDIT: &str = "Performs an exact string replacement in a file. You MUST Read the file first — old_string has to match the current content exactly, including indentation and whitespace. old_string must be unique in the file, or set replace_all=true to replace every occurrence. If the match fails, re-Read the file and adjust. Prefer this over `sed -i` in Bash.";

pub const BASH: &str = "Executes a command in the system shell and returns stdout/stderr. Use it for builds, tests, git, and package managers. Do NOT use it for file reading or searching — use Read/Grep/Glob instead, they are faster and formatted for you. Use absolute paths and avoid `cd`. Quote paths containing spaces. Long-running commands are killed at the timeout (default 120s, max 600s). Output over 30KB is truncated keeping head and tail.";

pub const GLOB: &str = "Finds files by glob pattern (`**/*.rs`, `src/**/*.test.ts`, ...). Respects .gitignore. Returns absolute paths. Use this instead of `find` in Bash. Can run in parallel with other read-only calls.";

pub const GREP: &str = "Searches file contents with a regular expression. Respects .gitignore and skips binary files; returns matching lines with `path:line` prefixes (capped at 500 matches). Use this instead of `grep`/`rg` in Bash. Can run in parallel with other read-only calls.";

pub const WEBFETCH: &str = "Fetches a URL and converts the page to plain text (HTML tags stripped, capped at 50KB). Good for docs, READMEs, and API references. Cannot execute JavaScript, so client-rendered pages may come back empty. Can run in parallel with other read-only calls.";

pub const TODO_WRITE: &str = "Creates or replaces the structured task list shown to the user. Call it for multi-step tasks (3+ steps): once up front to plan, then again on every status change. Each call replaces the whole list, so always pass every item. Keep exactly one item in_progress at a time; mark items completed immediately when done, never in batches. status: pending | in_progress | completed; priority (optional): high | medium | low.";

pub const ASK_QUESTION: &str = "Presents the user a structured questionnaire (1-4 questions, each with 2-4 options) and waits for their answers. Use it only at genuine decision points — ambiguous requirements or choices only the user can make. Do not use it for anything you can resolve yourself from the code or sensible defaults. An \"Other\" free-text option is appended to every question automatically.";

pub const EXIT_PLAN_MODE: &str = "Call this when you are in plan mode and have finished designing the implementation plan. Pass the complete plan (Markdown) as the `plan` parameter; the user reviews it and, on approval, execution mode is enabled automatically. Do not ask for approval in prose — this tool IS the approval request.";

/// SubAgent 工具描述模板：`{types}` 处由运行时替换为可用类型列表。
pub const SUB_AGENT_TEMPLATE: &str = "Spawns an independent sub-agent to complete a task, optionally in the background (run_in_background=true returns immediately; the result is injected into the conversation when ready). Sub-agents are STATELESS and ONE-SHOT: they see only your prompt — not this conversation — and you cannot send follow-ups, so the prompt must be a complete, self-contained task description stating exactly what to investigate or do and what the final report must contain. Use them for multi-step subtasks worth isolating, or broad codebase research; do NOT spawn one when you already know the two or three files to read. Multiple Agent calls in one response run concurrently.\n\nAvailable agent types:\n{types}";

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
