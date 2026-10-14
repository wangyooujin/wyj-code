//! Grep 工具 — 正则内容搜索（三种输出模式 + 上下文行）
//!
//! 两级实现，对外行为一致：
//!
//! 1. **rg 快路径**（默认）：系统 `PATH` 里有 ripgrep 时直接 spawn `rg`，吃它的 SIMD 匹配 +
//!    多线程遍历 + mmap 引擎。输出格式与原生实现逐字对齐（匹配行 `path:line:text`，上下文行
//!    `path-line-text`，rg 自己插入的 `--` 分隔符在回填时剔除）。
//! 2. **纯 Rust 兜底**：没装 ripgrep 时走 `ignore` 的 `build_parallel()`（按 CPU 核数并行，
//!    命中上限后 `WalkState::Quit` 提前收工）+ regex-automata 抽出的字面量预筛（memchr/memmem
//!    先扫整文件、再逐行过滤候选），保证没有 rg 时也不会退化成「单线程逐行 is_match」的旧水平。
//!
//! 忽略语义两边一致：跳过 `.git` 目录，`.gitignore`/`.ignore`/`.git/info/exclude` 在任何目录都
//! 生效（对齐 ripgrep 默认；旧实现 `hidden(false)` 会连 `.git/objects` 一起遍历，且 ignore crate
//! 的 `require_git` 默认 true，导致非 git 仓库里 `.gitignore` 静默失效）。

use anyhow::Result;
use async_trait::async_trait;
use ignore::{WalkBuilder, WalkState};
use regex_automata::util::prefilter::Prefilter;
use regex_automata::{MatchKind, Span};
use serde::Deserialize;
use serde_json::Value;
use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use wyj_api::types::ToolDefinition;
use wyj_core::tool::{Tool, ToolContext, ToolResult};

const MAX_MATCHES: usize = 500;
/// content 模式默认输出行数上限
const DEFAULT_CONTENT_LIMIT: usize = 200;
/// files_with_matches 模式默认文件数上限
const DEFAULT_FILES_LIMIT: usize = 100;
/// 并行遍历线程数上限
const MAX_WALK_THREADS: usize = 16;

pub struct GrepTool;

#[derive(Deserialize)]
struct Input {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    include: Option<String>,
    #[serde(default)]
    case_sensitive: Option<bool>,
    /// "content" | "files_with_matches"（默认）| "count"
    #[serde(default)]
    output_mode: Option<String>,
    /// 匹配行后附带的行数（仅 content 模式）
    #[serde(default, rename = "-A")]
    after: Option<usize>,
    /// 匹配行前附带的行数（仅 content 模式）
    #[serde(default, rename = "-B")]
    before: Option<usize>,
    /// 匹配行前后各附带的行数（仅 content 模式；被 -A/-B 覆盖）
    #[serde(default, rename = "-C")]
    context: Option<usize>,
    /// 输出条数上限（content=行数，files=文件数，count=文件数）
    #[serde(default)]
    head_limit: Option<usize>,
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "Grep"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: crate::descriptions::GREP.to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": crate::descriptions::FIELD_GREP_PATTERN
                    },
                    "path": {
                        "type": "string",
                        "description": crate::descriptions::FIELD_GREP_PATH
                    },
                    "include": {
                        "type": "string",
                        "description": "Only search files matching this glob, e.g. *.rs or *.{ts,tsx}"
                    },
                    "case_sensitive": {
                        "type": "boolean",
                        "description": "Case-sensitive matching (default true)"
                    },
                    "output_mode": {
                        "type": "string",
                        "enum": ["content", "files_with_matches", "count"],
                        "description": "content: matching lines with line numbers (supports -A/-B/-C); files_with_matches: only file paths (default); count: match count per file"
                    },
                    "-A": {
                        "type": "integer",
                        "description": "Lines to show after each match (content mode only)"
                    },
                    "-B": {
                        "type": "integer",
                        "description": "Lines to show before each match (content mode only)"
                    },
                    "-C": {
                        "type": "integer",
                        "description": "Lines to show before and after each match (content mode only)"
                    },
                    "head_limit": {
                        "type": "integer",
                        "description": "Cap the output at the first N lines/files (default: 200 lines for content, 100 files otherwise)"
                    }
                },
                "required": ["pattern"]
            }),
            native: None,
        }
    }

    /// Grep 是纯只读内容搜索，无副作用，可安全并发执行。
    fn parallel_safe(&self) -> bool {
        true
    }

    async fn run(&self, input: Value, ctx: &dyn ToolContext) -> Result<ToolResult> {
        let inp: Input = serde_json::from_value(input)?;
        let case_sensitive = inp.case_sensitive.unwrap_or(true);
        let re = regex_build(&inp.pattern, case_sensitive)?;

        let mode = match inp.output_mode.as_deref() {
            Some("content") => OutputMode::Content,
            Some("count") => OutputMode::Count,
            // 默认对齐 Claude Code：只列文件路径，需要行内容时显式选 content
            Some("files_with_matches") | None => OutputMode::Files,
            Some(other) => {
                return Ok(ToolResult::err(format!(
                    "未知 output_mode `{other}`（可选 content / files_with_matches / count）"
                )))
            }
        };
        let after = inp.after.or(inp.context).unwrap_or(0);
        let before = inp.before.or(inp.context).unwrap_or(0);
        let head_limit = inp.head_limit.unwrap_or(match mode {
            OutputMode::Content => DEFAULT_CONTENT_LIMIT,
            _ => DEFAULT_FILES_LIMIT,
        });

        let root = match &inp.path {
            Some(p) => {
                let pb = std::path::Path::new(p);
                if pb.is_absolute() {
                    pb.to_path_buf()
                } else {
                    ctx.cwd().join(pb)
                }
            }
            None => ctx.cwd().to_path_buf(),
        };

        let include_glob = inp
            .include
            .as_deref()
            .and_then(|g| globset::Glob::new(g).ok())
            .map(|g| g.compile_matcher());

        let spec = SearchSpec {
            root: root.clone(),
            re,
            pattern: inp.pattern,
            case_sensitive,
            mode,
            after,
            before,
            head_limit,
            include_glob,
            raw_include: inp.include.clone(),
        };

        // 快路径：系统装了 ripgrep 就直接用它（忽略规则与输出格式已对齐原生实现）。
        let (lines, truncated) = match rg_backend(&root, &spec) {
            Some(res) => res,
            // 兜底：纯 Rust 并行遍历 + 字面量预筛；整个同步过程放进 spawn_blocking 避免阻塞 tokio。
            None => tokio::task::spawn_blocking(move || native_search(&spec))
                .await
                .map_err(|e| anyhow::anyhow!("Grep 任务执行失败: {e}"))?,
        };

        if lines.is_empty() {
            return Ok(ToolResult::ok("未找到匹配内容"));
        }
        let mut out = lines.join("\n");
        if truncated {
            out.push_str(&format!("\n（已截断，仅显示前 {head_limit} 条）"));
        }
        Ok(ToolResult::ok(out))
    }
}

#[derive(Clone, Copy, PartialEq)]
enum OutputMode {
    Content,
    Files,
    Count,
}

/// 一次搜索的全部参数（rg 快路径与纯 Rust 兜底共用）。
struct SearchSpec {
    root: PathBuf,
    re: regex::Regex,
    /// 原始 pattern（预筛要在「未加 (?i) 前缀」的原文上做 Hir 抽取）
    pattern: String,
    case_sensitive: bool,
    mode: OutputMode,
    after: usize,
    before: usize,
    head_limit: usize,
    include_glob: Option<globset::GlobMatcher>,
    /// 原始 include glob（rg 用 `-g` 传原串；原生实现走已编译的 matcher）
    raw_include: Option<String>,
}

// ---------------------------------------------------------------- rg 快路径

/// 在 `PATH` 里找 ripgrep（只探测一次）。
fn rg_binary() -> Option<&'static Path> {
    static RG: OnceLock<Option<PathBuf>> = OnceLock::new();
    RG.get_or_init(|| {
        let name = if cfg!(windows) { "rg.exe" } else { "rg" };
        std::env::var_os("PATH")
            .map(|path| {
                std::env::split_paths(&path)
                    .map(|dir| dir.join(name))
                    .find(|c| c.is_file())
            })
            .unwrap_or(None)
    })
    .as_deref()
}

/// 用 ripgrep 执行搜索；没装 rg、spawn 失败或 rg 自身报错时返回 None，交给纯 Rust 兜底。
fn rg_backend(root: &Path, spec: &SearchSpec) -> Option<(Vec<String>, bool)> {
    let exe = rg_binary()?;
    let mut args: Vec<OsString> = vec![
        // 固定输出形态：永不染色、不分组标题、永远带文件名（否则单文件搜索会退化成只打行号）
        OsString::from("--color=never"),
        OsString::from("--no-heading"),
        OsString::from("-H"),
        // 忽略语义对齐原生实现：含隐藏文件、.gitignore 全局生效、跳过 .git 目录
        OsString::from("--hidden"),
        OsString::from("--no-messages"),
        OsString::from("-g"),
        OsString::from("!.git"),
        OsString::from("-g"),
        OsString::from("!.git/**"),
    ];
    if spec.case_sensitive {
        args.push(OsString::from("--case-sensitive"));
    } else {
        args.push(OsString::from("--ignore-case"));
    }
    match spec.mode {
        OutputMode::Files => args.push(OsString::from("-l")),
        OutputMode::Count => args.push(OsString::from("-c")),
        OutputMode::Content => {
            args.push(OsString::from("-n"));
            if spec.after > 0 {
                args.push(OsString::from(format!("-A{}", spec.after)));
            }
            if spec.before > 0 {
                args.push(OsString::from(format!("-B{}", spec.before)));
            }
        }
    }
    args.push(OsString::from("-e"));
    args.push(OsString::from(&spec.pattern));
    if let Some(raw) = spec.raw_include.as_deref() {
        args.push(OsString::from("-g"));
        args.push(OsString::from(raw));
    }
    args.push(OsString::from("--"));
    args.push(root.as_os_str().to_owned());

    let mut child = Command::new(exe)
        .args(&args)
        // 用户自己的 rg 配置（--smart-case / --hidden / 自定义 glob）会破坏语义一致性，显式清空
        .env("RIPGREP_CONFIG_PATH", "")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    // rg 输出的每行都以同一个 root 前缀开头，据此精确定位 `path:line:text` 里的行号段，
    // 避免把正文里的 `:123:`（上下文行也长这样）误计成匹配行、让 500 条上限提前触发。
    let prefix = format!("{}:", root.display());

    let mut out: Vec<String> = Vec::new();
    let mut matches = 0usize;
    let mut truncated = false;
    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else { break };
        if line == "--" {
            continue; // rg 用 `--` 分隔相邻上下文组，原生实现不输出
        }
        let is_match = spec.mode == OutputMode::Content && is_match_line(&line, &prefix);
        out.push(line);
        if is_match {
            matches += 1;
        }
        if out.len() >= spec.head_limit || matches >= MAX_MATCHES {
            truncated = true;
            break;
        }
    }

    let status = if truncated {
        // 提前收工：别让 rg 继续把整棵树扫完
        let _ = child.kill();
        child.wait().ok()
    } else {
        child.wait().ok()
    };
    if !truncated {
        // 2 = rg 自身报错（非法 pattern / glob 不被支持等）→ 交给纯 Rust 兜底再试一次
        if status
            .as_ref()
            .and_then(|s| s.code())
            .is_some_and(|c| c >= 2)
            && out.is_empty()
        {
            return None;
        }
    }
    // rg 多线程遍历的输出顺序不确定；files/count 模式按路径排序保证结果稳定
    if matches!(spec.mode, OutputMode::Files | OutputMode::Count) {
        out.sort_unstable();
    }
    Some((out, truncated))
}

/// 判断一行是不是匹配行（`path:line:text`，上下文行是 `path-line-text`）。
/// rg 的每行都以同一 root 前缀开头，故切掉前缀后找**第一个** `:数字:`（匹配）或 `-数字-`
/// （上下文）即可精确区分；路径里含 `:`/`-数字-` 的冷门情形由「谁先出现谁说了算」兜底，
/// 最差也只是把 500 条上限算得略保守。前缀对不上时退回宽松启发式。
fn is_match_line(line: &str, prefix: &str) -> bool {
    match line.strip_prefix(prefix) {
        Some(rest) => match first_sep_group(rest) {
            Some((_, true)) => true,
            Some((_, false)) => false,
            None => loose_is_match_line(line),
        },
        None => loose_is_match_line(line),
    }
}

/// 找出 `X数字X`（X 为 `:` 或 `-`）首次出现的分隔符与其是否为冒号。
fn first_sep_group(s: &str) -> Option<(usize, bool)> {
    let b = s.as_bytes();
    for (i, &x) in b.iter().enumerate() {
        if x != b':' && x != b'-' {
            continue;
        }
        let mut k = i + 1;
        while k < b.len() && b[k].is_ascii_digit() {
            k += 1;
        }
        if k > i + 1 && b.get(k) == Some(&x) {
            return Some((i, x == b':'));
        }
    }
    None
}

/// 宽松启发式：路径本身可能含 `:`，从左往右找第一个「全数字且非首段」的分隔点。
fn loose_is_match_line(line: &str) -> bool {
    let mut seen = false;
    for seg in line.split(':').skip(1) {
        if seen {
            return true;
        }
        if !seg.is_empty() && seg.bytes().all(|b| b.is_ascii_digit()) {
            seen = true;
        }
    }
    false
}

// ------------------------------------------------------- 纯 Rust 并行兜底

#[derive(Default)]
struct Accum {
    out: Vec<String>,
    matches: usize,
    truncated: bool,
}

impl Accum {
    /// 追加一行；触及上限时置 `truncated` 并返回 false，调用方应立即停止扫描。
    fn push(&mut self, line: String, is_match: bool, head_limit: usize) -> bool {
        self.out.push(line);
        if is_match {
            self.matches += 1;
        }
        self.truncated = self.out.len() >= head_limit || self.matches >= MAX_MATCHES;
        !self.truncated
    }
}

fn native_search(spec: &SearchSpec) -> (Vec<String>, bool) {
    let prefilter = Arc::new(build_prefilter(&spec.pattern, spec.case_sensitive));
    let acc = Mutex::new(Accum::default());
    let threads =
        std::thread::available_parallelism().map_or(4, |n| n.get().clamp(1, MAX_WALK_THREADS));

    let mut builder = WalkBuilder::new(&spec.root);
    builder
        .hidden(false) // 包含隐藏文件（对齐 rg --hidden）
        .ignore(true) // .ignore
        .git_ignore(true) // .gitignore
        .git_global(true)
        .git_exclude(true)
        .require_git(false) // 非 git 仓库里也生效（ignore crate 默认 true，与 rg 不一致）
        .threads(threads)
        .filter_entry(|e| e.file_name() != OsStr::new(".git"));
    builder.build_parallel().run(|| {
        let acc = &acc;
        let prefilter = Arc::clone(&prefilter);
        Box::new(move |entry: Result<ignore::DirEntry, ignore::Error>| {
            let Ok(entry) = entry else {
                return WalkState::Continue;
            };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                return WalkState::Continue;
            }
            let path = entry.path();
            if let Some(g) = &spec.include_glob {
                if !path.file_name().is_some_and(|n| g.is_match(n)) {
                    return WalkState::Continue;
                }
            }
            let mut guard = acc.lock().unwrap_or_else(|e| e.into_inner());
            if guard.truncated {
                return WalkState::Quit;
            }
            scan_file(path, spec, prefilter.as_ref().as_ref(), &mut guard);
            if guard.truncated {
                WalkState::Quit
            } else {
                WalkState::Continue
            }
        }) as Box<dyn FnMut(Result<ignore::DirEntry, ignore::Error>) -> WalkState + Send>
    });

    let acc = acc.into_inner().unwrap_or_else(|e| e.into_inner());
    (acc.out, acc.truncated)
}

fn scan_file(path: &Path, spec: &SearchSpec, prefilter: Option<&Prefilter>, acc: &mut Accum) {
    let Ok(content) = std::fs::read(path) else {
        return;
    };
    // 整文件级粗筛放在最前面：字节级 memmem，一个候选都定位不到就直接返回，
    // 连二进制 NUL 扫描和 UTF-8 校验都省掉（NUL 判据只用于排除文件，故顺序无关）。
    if let Some(pre) = prefilter {
        if pre.find(&content, Span::from(0..content.len())).is_none() {
            return;
        }
    }
    if content.contains(&0u8) {
        return; // 跳过二进制文件
    }
    let text = String::from_utf8_lossy(&content);

    let display = path.display();
    match spec.mode {
        OutputMode::Files => {
            if text
                .lines()
                .any(|l| is_hit(prefilter, l) && spec.re.is_match(l))
            {
                acc.push(display.to_string(), false, spec.head_limit);
            }
        }
        OutputMode::Count => {
            let n = text
                .lines()
                .filter(|l| is_hit(prefilter, l) && spec.re.is_match(l))
                .count();
            if n > 0 {
                acc.push(format!("{display}:{n}"), false, spec.head_limit);
            }
        }
        OutputMode::Content => {
            // before 环形缓冲 + after 倒计时；上下文行用 `path-line-text`
            // 分隔（rg 惯例），匹配行用 `path:line:text`
            let mut before_buf: VecDeque<(usize, &str)> = VecDeque::with_capacity(spec.before + 1);
            let mut after_remaining = 0usize;
            let mut last_printed: Option<usize> = None;
            for (idx, line) in text.lines().enumerate() {
                let lineno = idx + 1;
                if is_hit(prefilter, line) && spec.re.is_match(line) {
                    // 补 before 上下文（跳过已输出过的行）
                    for (bn, bl) in before_buf.iter() {
                        if last_printed.map_or(true, |lp| *bn > lp)
                            && !acc.push(format!("{display}-{bn}-{bl}"), false, spec.head_limit)
                        {
                            return;
                        }
                    }
                    before_buf.clear();
                    if !acc.push(format!("{display}:{lineno}:{line}"), true, spec.head_limit) {
                        return;
                    }
                    last_printed = Some(lineno);
                    after_remaining = spec.after;
                } else if after_remaining > 0 {
                    if !acc.push(format!("{display}-{lineno}-{line}"), false, spec.head_limit) {
                        return;
                    }
                    last_printed = Some(lineno);
                    after_remaining -= 1;
                } else if spec.before > 0 {
                    if before_buf.len() == spec.before {
                        before_buf.pop_front();
                    }
                    before_buf.push_back((lineno, line));
                }
            }
        }
    }
}

/// 预筛粗筛：有预筛时先用 memchr/memmem 定位候选，只有候选行才值得进 regex 引擎。
#[inline]
fn is_hit(prefilter: Option<&Prefilter>, line: &str) -> bool {
    match prefilter {
        Some(pre) => pre
            .find(line.as_bytes(), Span::from(0..line.len()))
            .is_some(),
        None => true,
    }
}

/// 从正则里抽字面量做预筛：绝大多数真实模式（标识符、路径、关键字）都含字面量，
/// 用 memchr/memmem 粗筛能省掉绝大部分 `re.is_match` 调用。抽不出（如 `a.*b`、纯字符类）
/// 返回 None，退回逐行 `re.is_match`。
fn build_prefilter(pattern: &str, case_sensitive: bool) -> Option<Prefilter> {
    let hir = if case_sensitive {
        regex_syntax::parse(pattern).ok()?
    } else {
        // 让 Hir 自己按 Unicode 大小写折叠展开，拿到「所有可能字面量」的并集；
        // 不能简单把 needle 转小写——那会漏掉 ſ/İ 这类折叠特例造成漏报。
        regex_syntax::ParserBuilder::new()
            .case_insensitive(true)
            .build()
            .parse(pattern)
            .ok()?
    };
    let seq = regex_syntax::hir::literal::Extractor::new().extract(&hir);
    let literals = seq.literals()?;
    // 出现空字面量 = 模式可匹配空串（如 `a*`/`^`），预筛没有过滤价值
    if literals.is_empty() || literals.iter().any(|l| l.as_bytes().is_empty()) {
        return None;
    }
    let needles: Vec<&[u8]> = literals.iter().map(|l| l.as_bytes()).collect();
    let pre = Prefilter::new(MatchKind::LeftmostFirst, &needles)?;
    pre.is_fast().then_some(pre)
}

fn regex_build(pattern: &str, case_sensitive: bool) -> Result<regex::Regex> {
    let re = if case_sensitive {
        regex::Regex::new(pattern)?
    } else {
        regex::Regex::new(&format!("(?i){pattern}"))?
    };
    Ok(re)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::ToolCtx;
    use serde_json::json;

    async fn run_grep(input: Value, dir: &std::path::Path) -> String {
        let ctx = ToolCtx::new(dir);
        GrepTool.run(input, &ctx).await.unwrap().content
    }

    /// 强制走纯 Rust 兜底（模拟没装 ripgrep 的机器），与 rg 快路径共用同一套断言。
    async fn run_grep_native(input: Value, dir: &std::path::Path) -> String {
        let inp: Input = serde_json::from_value(input).unwrap();
        let case_sensitive = inp.case_sensitive.unwrap_or(true);
        let re = regex_build(&inp.pattern, case_sensitive).unwrap();
        let spec = SearchSpec {
            root: dir.to_path_buf(),
            re,
            pattern: inp.pattern,
            case_sensitive,
            mode: match inp.output_mode.as_deref() {
                Some("content") => OutputMode::Content,
                Some("count") => OutputMode::Count,
                _ => OutputMode::Files,
            },
            after: inp.after.or(inp.context).unwrap_or(0),
            before: inp.before.or(inp.context).unwrap_or(0),
            head_limit: inp.head_limit.unwrap_or(200),
            include_glob: inp
                .include
                .as_deref()
                .and_then(|g| globset::Glob::new(g).ok())
                .map(|g| g.compile_matcher()),
            raw_include: inp.include.clone(),
        };
        let (lines, truncated) = native_search(&spec);
        let mut out = lines.join("\n");
        if truncated {
            out.push_str(&format!("\n（已截断，仅显示前 {} 条）", spec.head_limit));
        }
        if out.is_empty() {
            "未找到匹配内容".to_string()
        } else {
            out
        }
    }

    struct FixtureDir(std::path::PathBuf);
    impl FixtureDir {
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for FixtureDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fixture_dir() -> FixtureDir {
        let dir = std::env::temp_dir().join(format!(
            "wyj-grep-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("a.txt"),
            "line one\nMATCH here\nline three\nline four\nMATCH again\nline six\n",
        )
        .unwrap();
        std::fs::write(dir.join("b.txt"), "nothing\nto see\n").unwrap();
        std::fs::write(dir.join("c.rs"), "fn MATCH() {}\n").unwrap();
        std::fs::write(dir.join("d.bin"), "MATCH\x00binary\n").unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git/config"), "MATCH in git dir\n").unwrap();
        std::fs::write(dir.join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(dir.join("ignored.txt"), "MATCH in ignored\n").unwrap();
        FixtureDir(dir)
    }

    /// rg 快路径与纯 Rust 兜底必须给出等价结果。
    async fn assert_both_ways(input: Value, dir: &std::path::Path) -> String {
        let via_rg = run_grep(input.clone(), dir).await;
        let via_native = run_grep_native(input, dir).await;
        let mut a: Vec<&str> = via_rg.lines().collect();
        let mut b: Vec<&str> = via_native.lines().collect();
        a.sort_unstable();
        b.sort_unstable();
        assert_eq!(a, b, "rg 快路径与纯 Rust 兜底结果不一致");
        via_rg
    }

    #[tokio::test]
    async fn default_mode_lists_files_only() {
        let dir = fixture_dir();
        let out = assert_both_ways(json!({"pattern": "MATCH"}), dir.path()).await;
        assert!(out.contains("a.txt"));
        assert!(!out.contains("b.txt"));
        assert!(!out.contains("MATCH here"), "默认模式不应输出行内容");
    }

    #[tokio::test]
    async fn content_mode_with_context_lines() {
        let dir = fixture_dir();
        let out = assert_both_ways(
            json!({"pattern": "MATCH here", "output_mode": "content", "-C": 1}),
            dir.path(),
        )
        .await;
        assert!(out.contains(":2:MATCH here"), "匹配行带 path:line: 前缀");
        assert!(out.contains("-1-line one"), "before 上下文用 - 分隔");
        assert!(out.contains("-3-line three"), "after 上下文用 - 分隔");
        assert!(!out.contains("\n--\n"), "rg 的 -- 分隔符不应出现在输出里");
    }

    #[tokio::test]
    async fn count_mode_reports_per_file_counts() {
        let dir = fixture_dir();
        let out = assert_both_ways(
            json!({"pattern": "MATCH", "output_mode": "count"}),
            dir.path(),
        )
        .await;
        assert!(out.contains("a.txt:2"), "got: {out}");
        assert!(out.contains("c.rs:1"), "got: {out}");
    }

    #[tokio::test]
    async fn head_limit_truncates() {
        let dir = fixture_dir();
        let out = assert_both_ways(
            json!({"pattern": "line", "output_mode": "content", "head_limit": 2}),
            dir.path(),
        )
        .await;
        assert!(out.contains("已截断"), "got: {out}");
    }

    #[tokio::test]
    async fn skips_git_dir_binary_and_gitignored() {
        let dir = fixture_dir();
        let out = assert_both_ways(
            json!({"pattern": "MATCH", "output_mode": "content"}),
            dir.path(),
        )
        .await;
        assert!(!out.contains(".git/config"), ".git 目录应被跳过");
        assert!(
            !out.contains("ignored.txt"),
            ".gitignore 命中的文件应被跳过"
        );
        assert!(!out.contains("d.bin"), "二进制文件应被跳过");
    }

    #[tokio::test]
    async fn include_glob_filters_by_basename() {
        let dir = fixture_dir();
        let out =
            assert_both_ways(json!({"pattern": "MATCH", "include": "*.rs"}), dir.path()).await;
        assert!(out.contains("c.rs"), "got: {out}");
        assert!(!out.contains("a.txt"), "got: {out}");
    }

    #[tokio::test]
    async fn case_insensitive_matches() {
        let dir = fixture_dir();
        let out = assert_both_ways(
            json!({"pattern": "match", "case_sensitive": false, "output_mode": "count"}),
            dir.path(),
        )
        .await;
        assert!(out.contains("a.txt:2"), "got: {out}");
    }

    #[tokio::test]
    async fn anchored_pattern_prefilter_has_no_false_negative() {
        let dir = fixture_dir();
        let out = assert_both_ways(
            json!({"pattern": r"^MATCH", "output_mode": "count"}),
            dir.path(),
        )
        .await;
        assert!(out.contains("a.txt:2"), "got: {out}");
    }

    #[test]
    fn match_line_detection() {
        assert!(is_match_line("a.txt:12:hello", "/tmp/x/"));
        assert!(is_match_line("/tmp/x/a:b.txt:12:hello", "/tmp/x/"));
        assert!(!is_match_line("a.txt-12-hello", "/tmp/x/"));
        assert!(!is_match_line("a.txt", "/tmp/x/"));
        // 上下文行的正文里带 `:数字:`，不算匹配行（否则 500 条上限会提前触发）
        assert!(!is_match_line("/tmp/x/a.txt-12-a:1:b", "/tmp/x/"));
        // 前缀对不上时退回宽松启发式，仍能认出匹配行
        assert!(is_match_line("a.txt:12:hello", "/other/"));
    }

    #[test]
    fn prefilter_is_conservative() {
        // 能抽出字面量 → 有预筛（大小写两种都要抽到全部折叠变体）
        assert!(build_prefilter("MATCH", true).is_some());
        assert!(build_prefilter("match", false).is_some());
        assert!(build_prefilter(r"^MATCH\b", true).is_some());
        // 抽不出 / 可匹配空串 → 无预筛（退回逐行 is_match，宁可慢不能漏）
        assert!(build_prefilter("", true).is_none());
        assert!(build_prefilter("a*", true).is_none());
        assert!(build_prefilter("[0-9]+", true).is_none());
    }
}
