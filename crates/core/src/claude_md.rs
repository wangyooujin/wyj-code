//! 记忆文件加载（全局 + 项目祖先链）。
//!
//! - 查找范围：全局 `~/.wyj-code/` + 从 git 仓库根到 cwd 的祖先链。
//! - **按槽位独立回退**（v0.2.31 起）。每级目录分两个槽位，各自按候选链
//!   顺序探测、**命中第一个即停**：
//!   - 主槽位：`AGENTS.md` → `CLAUDE.md` → `GEMINI.md` → `AGENT.md`
//!   - 个人覆盖槽：`AGENTS.local.md` → `CLAUDE.local.md`
//!
//!   `AGENTS.md` 仍是首选（对齐 AGENTS.md 开放标准，Codex / Cursor /
//!   Gemini CLI 等均采用）；回退只为覆盖「项目根只有某种工具的规约文件」
//!   这类既有仓库——这些文件是项目作者写的规约，读不到等于整个项目的
//!   约定对模型不可见，代价远大于多试几个 `is_file()` 调用。
//!
//!   **槽位独立**而非「整个家族二选一」，是为了保住 personal override
//!   语义：主文件走 `AGENTS.md` 而个人私有覆盖写在 `CLAUDE.local.md`
//!   时仍然生效，不会被静默丢弃。两个槽位各自只取一个文件，
//!   因此**永远不存在同一份规约被注入两遍**的情况。
//! - 支持 `@path/to/file` 递归导入（深度上限 4，跳过 fenced code block）。
//! - **子目录动态加载向上逐级查找**（见 [`ClaudeMdLoader::maybe_dir_reminder`]）：
//!   工具触达某个文件后，从其所在目录一路查到项目根，沿途每个「有规约文件且尚未
//!   展示过」的目录注入一次。只查触达目录本身会让有 `src/` 层的模块规约永远失联。
//!   上界是项目根，越界的绝对路径只取其自身，不向上爬。
//! - **注入量封顶**：单目录规约文本按 [`MAX_RULE_FILE_BYTES`] 截断并显式标注。
//! - 不缓存文件内容，只缓存"哪些目录参与"这个列表；内容每次调用都重新读盘，
//!   保证运行期间编辑立即生效、压缩后依然完整（因为每轮都重新拼装，不依赖历史消息）。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const MAX_IMPORT_DEPTH: u8 = 4;

/// 主槽位候选文件名，按优先级排列（越靠前越优先）。`AGENTS.md` 是
/// AGENTS.md 开放标准的规范名，其余是各工具的既有约定名，仅作回退：
/// 没有它们的话，「根目录只有 CLAUDE.md」的老仓库会被当成没有任何项目
/// 规约，模型对项目约定完全失明。`AGENT.md`（单数）是实践中常见的
/// 手写变体，排在最后。
const MEMORY_FILES: [&str; 4] = ["AGENTS.md", "CLAUDE.md", "GEMINI.md", "AGENT.md"];

/// 个人覆盖槽候选文件名，语义同 [`MEMORY_FILES`]：local 视作个人私有
/// 增补追加（不提交 git），排在主文件之后注入。
const MEMORY_LOCAL_FILES: [&str; 2] = ["AGENTS.local.md", "CLAUDE.local.md"];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ClaudeMdSource {
    Global,
    Project,
    Subdir,
}

/// 供 `/memory` 面板展示的候选文件（含尚不存在、可供创建的路径）
pub struct DiscoveredFile {
    pub path: PathBuf,
    pub source: ClaudeMdSource,
    pub exists: bool,
}

/// 单个目录内注入的规约文本字节上限。
///
/// 规约是**一次性**注入（`seen_dirs` 去重后不再重发），所以这个上限比记忆快照的
/// `MAX_CONTEXT_BYTES`(8KB) 宽松得多——它要挡的是「把生成物 / 数据导出 / 巨型文档
/// 误写成 `AGENTS.md`」这类事故，而不是压缩正常规约。64KB 足以覆盖本仓库最大的
/// `crates/tui/CLAUDE.md`(51KB)，正常规约一个都不会被切。
///
/// 截断**保留头部并显式标注**：`char` 边界安全地切，绝不 panic；静默丢尾会让
/// 模型以为自己读全了规则，后果比读到一半更糟。
const MAX_RULE_FILE_BYTES: usize = 64 * 1024;

/// 每次 Agent 构建时扫描一次，确定参与注入的目录列表；内容按需每轮重新读盘。
pub struct ClaudeMdLoader {
    global_dir: Option<PathBuf>,
    /// 项目根。子目录动态加载**向上查找的硬边界**：越过它就爬到 `/Volumes`、
    /// `~` 这些与本项目无关的目录去了，而那里恰好可能有个 `AGENTS.md`。
    project_root: PathBuf,
    /// 祖先链目录，root → cwd 顺序
    chain_dirs: Vec<PathBuf>,
    /// 子目录动态加载去重（含 global_dir/chain_dirs 本身，避免重复展示）
    seen_dirs: Mutex<HashSet<PathBuf>>,
}

impl ClaudeMdLoader {
    pub fn new(cwd: &Path) -> Self {
        let global_dir = wyj_config::global_config_dir().ok();
        // 归一是必须的，不是洁癖：`maybe_dir_reminder` 拿 `project_root` 做
        // `starts_with` 边界判断、拿 `seen_dirs` 去重，只要 root / cwd / 工具入参
        // 三者的路径拼写不一致（`./a/b` vs `a/b`），边界就会漏判或去重失效。
        let root = normalize_lexically(&wyj_config::project_root(cwd));
        let cwd = normalize_lexically(cwd);
        let chain_dirs = collect_chain(&root, &cwd);

        let mut seen = HashSet::new();
        if let Some(g) = &global_dir {
            seen.insert(g.clone());
        }
        for d in &chain_dirs {
            seen.insert(d.clone());
        }

        Self {
            global_dir,
            project_root: root,
            chain_dirs,
            seen_dirs: Mutex::new(seen),
        }
    }

    /// 每轮对话开始时调用：重新读盘拼出完整的 `<system-reminder>` 文本，
    /// 无任何文件时返回 None。
    ///
    /// **同步版本**：会阻塞当前线程。`run_turn` 这类 async 上下文请用
    /// [`Self::turn_reminder_async`]——规约文件链动辄几十 KB，每轮都要
    /// 同步读盘，在 async 运行时里做会占住 worker 线程。
    pub fn turn_reminder(&self) -> Option<String> {
        let mut sections: Vec<(ClaudeMdSource, PathBuf, String)> = vec![];
        if let Some(g) = &self.global_dir {
            if let Some(text) = load_dir_files(g) {
                sections.push((ClaudeMdSource::Global, g.clone(), text));
            }
        }
        for d in &self.chain_dirs {
            if let Some(text) = load_dir_files(d) {
                sections.push((ClaudeMdSource::Project, d.clone(), text));
            }
        }
        if sections.is_empty() {
            return None;
        }
        Some(wrap_reminder(
            "The following project memory files apply to the current project. Follow their instructions.",
            &sections,
        ))
    }

    /// [`Self::turn_reminder`] 的异步版本：把同步读盘放进 `spawn_blocking`，
    /// 不占住 async 运行时的 worker 线程。
    ///
    /// **不缓存内容**：每轮仍然重新读盘，运行期间编辑规约文件立即生效
    /// （这是本模块的既有语义，见模块文档）。这里只改「在哪个线程读」，
    /// 不改「什么时候读」。
    pub async fn turn_reminder_async(&self) -> Option<String> {
        // `ClaudeMdLoader` 只含 `PathBuf` / `Mutex<HashSet>`，可跨线程移动；
        // 但 `turn_reminder` 借用 `self`，因此克隆一份廉价的状态再搬进去。
        let global_dir = self.global_dir.clone();
        let chain_dirs = self.chain_dirs.clone();
        tokio::task::spawn_blocking(move || {
            let mut sections: Vec<(ClaudeMdSource, PathBuf, String)> = vec![];
            if let Some(g) = &global_dir {
                if let Some(text) = load_dir_files(g) {
                    sections.push((ClaudeMdSource::Global, g.clone(), text));
                }
            }
            for d in &chain_dirs {
                if let Some(text) = load_dir_files(d) {
                    sections.push((ClaudeMdSource::Project, d.clone(), text));
                }
            }
            if sections.is_empty() {
                return None;
            }
            Some(wrap_reminder(
                "The following project memory files apply to the current project. Follow their instructions.",
                &sections,
            ))
        })
        .await
        .ok()
        .flatten()
    }

    /// 工具触达新目录时调用：从该目录**逐级向上**找，直到项目根，沿途注入每个
    /// 未展示过且确有规约文件的目录。返回一段 reminder 文本；全都没有则 None。
    ///
    /// 为什么要向上找而不是只看触达目录本身：工具报上来的是**文件的直接父目录**，
    /// 而规约习惯写在模块根（`crates/core/CLAUDE.md`）。只看 `crates/core/src/`
    /// 会永远返回 None，于是「按目录惰性下沉」在有 `src/` 层的 Rust / TS 仓库里
    /// 形同虚设——模块规约内容全部对模型不可见，而这恰恰是它们唯一的存在理由。
    pub fn maybe_dir_reminder(&self, dir: &Path) -> Option<String> {
        let start = normalize_lexically(dir);
        // 触达项目外的路径时只看它自己：继续向上会把 `/`、`~`、`/Volumes` 这些
        // 与本项目无关的目录也扫一遍，而那里出现的 AGENTS.md 与本次任务无关，
        // 纯属往上下文里塞噪声。
        let bounded = start.starts_with(&self.project_root);

        let mut sections: Vec<(ClaudeMdSource, PathBuf, String)> = vec![];
        let mut cur = start;
        loop {
            if let Some(text) = self.take_unseen(&cur) {
                sections.push((ClaudeMdSource::Subdir, cur.clone(), text));
            }
            if !bounded || cur == self.project_root {
                break;
            }
            match cur.parent() {
                Some(p) => cur = p.to_path_buf(),
                None => break,
            }
        }

        if sections.is_empty() {
            return None;
        }
        Some(wrap_reminder(
            "The directories you just accessed have additional project instructions. Follow them as well:",
            &sections,
        ))
    }

    /// 首次见到该目录时才读盘注入；见过了返回 None。
    ///
    /// `seen` 在读盘**之前**落盘是有意的：同一轮里多个工具命中同一目录（`Grep`
    /// 带 path + `Read` 带 file_path）时只会探一次盘，而内容仍随每轮重读，
    /// 运行期改文件照样生效（见模块注释）。
    fn take_unseen(&self, dir: &Path) -> Option<String> {
        {
            let mut seen = self.seen_dirs.lock().unwrap();
            if !seen.insert(dir.to_path_buf()) {
                return None;
            }
        }
        load_dir_files(dir)
    }
}

/// 供 `/memory` 面板调用的纯函数：列出当前 cwd 适用的全部候选文件（含不存在的）。
pub fn discover_files(cwd: &Path) -> Vec<DiscoveredFile> {
    let mut out = vec![];
    if let Ok(g) = wyj_config::global_config_dir() {
        push_dir_candidates(&mut out, &g, ClaudeMdSource::Global);
    }
    let root = wyj_config::project_root(cwd);
    for d in collect_chain(&root, cwd) {
        push_dir_candidates(&mut out, &d, ClaudeMdSource::Project);
    }
    out
}

/// 该目录**已经存在**的规约文件名（走与注入完全相同的 `resolve_slot` 解析线）。
///
/// 供 `/init` 决定生成目标：仓库里既然已经有 `CLAUDE.md`，就该更新那一份，
/// 而不是凭空再写一份内容重复的 `AGENTS.md`——两份同名职责不同文件名的
/// 规约文件是漂移的温床，而「命中即停」只保证模型不双份注入，并不阻止
/// 维护者把它们写成两份不同步的副本。
///
/// 一个候选都不存在时返回 `None`，此时用规范名 `MEMORY_FILES[0]`（`AGENTS.md`）
/// 作创建目标。
pub fn existing_memory_file_name(dir: &Path) -> Option<String> {
    resolve_slot(dir, &MEMORY_FILES).and_then(|(path, _)| {
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
    })
}

fn push_dir_candidates(out: &mut Vec<DiscoveredFile>, dir: &Path, source: ClaudeMdSource) {
    // 与 `load_dir_files` 走同一条解析线（同一个 `resolve_slot`），保证面板
    // 显示的就是实际会注入的那个文件，不存在"面板说有、实际没读"的漂移。
    let main = resolve_slot(dir, &MEMORY_FILES);
    let local = resolve_slot(dir, &MEMORY_LOCAL_FILES);

    match main {
        Some((path, _)) => out.push(DiscoveredFile {
            path,
            source,
            exists: true,
        }),
        // 一个候选都不命中：列出规范名供 `/memory` 面板提供创建入口
        //（exists=false）。
        None => out.push(DiscoveredFile {
            path: dir.join(MEMORY_FILES[0]),
            source,
            exists: false,
        }),
    }
    if let Some((path, _)) = local {
        out.push(DiscoveredFile {
            path,
            source,
            exists: true,
        });
    }
}

/// 在单个目录内按候选链解析一个槽位，返回**第一个有实际内容**的
/// （路径, 解析后内容）。
///
/// "有实际内容" 而非单纯 `is_file()`：一个 0 字节 / 全空白的 `AGENTS.md`
/// 等于没有规约，回退到下一个候选比就此停下更符合直觉，也不会让
/// `/memory` 面板显示一个实际不生效的文件。
fn resolve_slot(dir: &Path, names: &[&str]) -> Option<(PathBuf, String)> {
    for name in names {
        let path = dir.join(name);
        if let Some(content) = read_and_resolve(&path, 0) {
            return Some((path, content));
        }
    }
    None
}

/// 单个目录内的记忆文件读取规则：主槽位与个人覆盖槽各取一个文件，
/// 两个都命中时 local 追加在后（视作个人覆盖增补）。
///
/// 结果受 [`MAX_RULE_FILE_BYTES`] 约束：主槽位与 local 各自独立封顶后才拼接，
/// 所以「一个超大主文件 + 正常 local」不会把 local 挤掉。
fn load_dir_files(dir: &Path) -> Option<String> {
    let parts: Vec<String> = [MEMORY_FILES.as_slice(), MEMORY_LOCAL_FILES.as_slice()]
        .iter()
        .filter_map(|names| resolve_slot(dir, names))
        .map(|(_, content)| truncate_rule_text(&content))
        .collect();

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

/// 超长规约文本按字节封顶，保留头部并显式标注截断。
///
/// 回退到 `char` 边界：`MAX_RULE_FILE_BYTES` 落在多字节字符中间时直接切会 panic
/// （Rust 的 String 不允许半个 code point），而中文规约里这个概率不是零。
fn truncate_rule_text(text: &str) -> String {
    if text.len() <= MAX_RULE_FILE_BYTES {
        return text.to_string();
    }
    // 边界回退走 `crate::textutil::floor_char_boundary`（全仓唯一实现），
    // 不再手写一份相同的循环。
    let end = crate::textutil::floor_char_boundary(text, MAX_RULE_FILE_BYTES);
    format!(
        "{}\n\n[truncated: this rules file is {} bytes; only the first {} bytes were loaded]",
        &text[..end],
        text.len(),
        end
    )
}

/// 纯字符串的路径归一：消掉 `.` 与 `..`，**不解析符号链接**。
///
/// 为什么不用 `fs::canonicalize`：macOS 上 `/tmp` 会变成 `/private/tmp`，
/// 于是同一个目录在 `seen_dirs` 里成了两条记录，同一份规约被注入两次——而重复注入
/// 恰恰是这个模块要杜绝的事（见 `resolve_slot` 的「命中即停」）。代价是 `..` 跨
/// 符号链接那一种形态不做文件系统语义解析，但工具入参极少产生这种路径。
fn normalize_lexically(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                // `..` 弹到根或空路径时丢弃：`/..` 仍是 `/`，而不是弹出一个
                // `/..` 让后面的 `starts_with(project_root)` 比对失配。
                if !out.pop() && !out.has_root() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

fn read_and_resolve(path: &Path, depth: u8) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    if content.trim().is_empty() {
        return None;
    }
    let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
    Some(resolve_imports(&content, base_dir, depth))
}

fn import_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"(?:^|\s)@(\S+)").unwrap())
}

/// 解析 `@path/to/file` 导入语法：跳过 fenced code block，递归深度上限 MAX_IMPORT_DEPTH。
fn resolve_imports(content: &str, base_dir: &Path, depth: u8) -> String {
    if depth >= MAX_IMPORT_DEPTH {
        return content.to_string();
    }

    let mut out = String::with_capacity(content.len());
    let mut in_fence = false;
    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if in_fence {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        out.push_str(&expand_line_imports(line, base_dir, depth));
        out.push('\n');
    }
    out
}

fn expand_line_imports(line: &str, base_dir: &Path, depth: u8) -> String {
    let re = import_regex();
    if !re.is_match(line) {
        return line.to_string();
    }
    let mut out = String::with_capacity(line.len());
    let mut last = 0;
    for caps in re.captures_iter(line) {
        let m = caps.get(0).unwrap();
        let token = caps.get(1).unwrap().as_str();
        out.push_str(&line[last..m.start()]);
        // 保留匹配开头的空白/行首（group 0 含前导空白，token 不含）
        let prefix_len = m.as_str().len() - token.len() - 1;
        out.push_str(&m.as_str()[..prefix_len]);
        match try_import(token, base_dir, depth) {
            Some(expanded) => out.push_str(&expanded),
            None => {
                out.push('@');
                out.push_str(token);
            }
        }
        last = m.end();
    }
    out.push_str(&line[last..]);
    out
}

fn try_import(token: &str, base_dir: &Path, depth: u8) -> Option<String> {
    // 只处理看起来像路径的 token，避免把 email@domain.com 误判为导入
    if !token.contains('/') && !token.ends_with(".md") {
        return None;
    }
    let resolved = resolve_import_path(token, base_dir)?;
    if !resolved.is_file() {
        return None;
    }
    let content = std::fs::read_to_string(&resolved).ok()?;
    let child_base = resolved.parent().unwrap_or(base_dir);
    let expanded = resolve_imports(&content, child_base, depth + 1);
    Some(format!(
        "\n--- @{token} ---\n{}\n--- end @{token} ---\n",
        expanded.trim()
    ))
}

fn resolve_import_path(token: &str, base_dir: &Path) -> Option<PathBuf> {
    let p = Path::new(token);
    if p.is_absolute() {
        return Some(p.to_path_buf());
    }
    if let Some(stripped) = token.strip_prefix("~/") {
        let home = wyj_config::home_dir().ok()?;
        return Some(home.join(stripped));
    }
    Some(base_dir.join(p))
}

/// reminder 包装（模型侧文本，英文，不走 i18n）
fn wrap_reminder(intro: &str, sections: &[(ClaudeMdSource, PathBuf, String)]) -> String {
    let mut body = String::from(intro);
    body.push_str("\n\n");
    for (source, path, content) in sections {
        let source_en = match source {
            ClaudeMdSource::Global => "global",
            ClaudeMdSource::Project => "project",
            ClaudeMdSource::Subdir => "subdirectory",
        };
        body.push_str(&format!(
            "Contents of {} ({source_en}):\n\n",
            path.display()
        ));
        body.push_str(content.trim());
        body.push_str("\n\n");
    }
    format!("<system-reminder>\n{}\n</system-reminder>", body.trim_end())
}

/// 收集从 root 到 cwd（含两端）的目录链，按 root → cwd 顺序返回。
fn collect_chain(root: &Path, cwd: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![cwd.to_path_buf()];
    let mut cur = cwd.to_path_buf();
    while cur != root {
        match cur.parent() {
            Some(p) => {
                cur = p.to_path_buf();
                dirs.push(cur.clone());
            }
            None => break,
        }
    }
    dirs.reverse();
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_dir(name: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("wyj-code-claude-md-test-{name}-{n}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn load_dir_files_merges_agents_and_local() {
        let dir = unique_dir("merge");
        std::fs::write(dir.join("AGENTS.md"), "shared rules").unwrap();
        std::fs::write(dir.join("AGENTS.local.md"), "my private override").unwrap();
        let text = load_dir_files(&dir).unwrap();
        assert!(text.contains("shared rules"));
        assert!(text.contains("my private override"));
    }

    #[test]
    fn main_slot_falls_back_to_claude_md_when_no_agents_md() {
        // 「项目根只有 CLAUDE.md」是最常见的既有仓库形态：读不到它等于
        // 整个项目的约定对模型不可见，代价远大于多试几个 is_file()。
        let dir = unique_dir("claude-fallback");
        std::fs::write(dir.join("CLAUDE.md"), "claude project rules").unwrap();
        let text = load_dir_files(&dir).unwrap();
        assert!(text.contains("claude project rules"));
    }

    #[test]
    fn main_slot_stops_at_agents_md_when_both_present() {
        // 命中第一个即停：绝不把两份内容都注入（重复内容白烧 token，
        // 且两个文件冲突时模型行为不可预测）。
        let dir = unique_dir("agents-precedence");
        std::fs::write(dir.join("AGENTS.md"), "committed rules").unwrap();
        std::fs::write(dir.join("AGENTS.local.md"), "personal override").unwrap();
        std::fs::write(dir.join("CLAUDE.md"), "lower priority noise").unwrap();
        std::fs::write(dir.join("AGENT.md"), "tail candidate noise").unwrap();
        let text = load_dir_files(&dir).unwrap();
        assert!(text.contains("committed rules"));
        assert!(text.contains("personal override"));
        assert!(!text.contains("lower priority noise"));
        assert!(!text.contains("tail candidate noise"));
    }

    #[test]
    fn main_slot_reaches_gemini_and_agent_tail_candidates() {
        let gemini = unique_dir("gemini-tail");
        std::fs::write(gemini.join("GEMINI.md"), "gemini rules").unwrap();
        assert!(load_dir_files(&gemini).unwrap().contains("gemini rules"));

        let agent = unique_dir("agent-tail");
        std::fs::write(agent.join("AGENT.md"), "singular agent rules").unwrap();
        assert!(load_dir_files(&agent)
            .unwrap()
            .contains("singular agent rules"));
    }

    #[test]
    fn local_slot_falls_back_to_claude_local_md() {
        // 两个槽位**独立**回退：主文件走 AGENTS.md、私有覆盖写在
        // CLAUDE.local.md 时仍然生效，不被静默丢弃。
        let dir = unique_dir("local-fallback");
        std::fs::write(dir.join("AGENTS.md"), "shared rules").unwrap();
        std::fs::write(dir.join("CLAUDE.local.md"), "claude private override").unwrap();
        let text = load_dir_files(&dir).unwrap();
        assert!(text.contains("shared rules"));
        assert!(text.contains("claude private override"));
    }

    #[test]
    fn blank_primary_falls_through_to_next_candidate() {
        // 0 字节 / 全空白的主文件等于「这里没有规约」，应继续往下探，
        // 否则面板会显示一个实际不生效的 AGENTS.md。
        let dir = unique_dir("blank-primary");
        std::fs::write(dir.join("AGENTS.md"), "   \n\n").unwrap();
        std::fs::write(dir.join("CLAUDE.md"), "real rules").unwrap();
        let text = load_dir_files(&dir).unwrap();
        assert!(text.contains("real rules"));
    }

    /// `/init` 的生成目标必须跟着仓库现状走，否则会在已有 `CLAUDE.md` 的仓库里
    /// 凭空造一份内容重复的 `AGENTS.md`——「命中即停」只保证模型不双份注入，
    /// 管不住两份副本各自漂移。这是本条不变式的钉子。
    #[test]
    fn init_target_follows_the_file_that_would_actually_be_read() {
        let dir = unique_dir("init-target-existing");
        std::fs::write(dir.join("CLAUDE.md"), "root rules").unwrap();
        assert_eq!(
            existing_memory_file_name(&dir).as_deref(),
            Some("CLAUDE.md"),
            "已有 CLAUDE.md 时 /init 必须更新它，而不是新建 AGENTS.md"
        );

        // 两个都在时按注入的优先级走（AGENTS.md 首选），不能各写各的。
        std::fs::write(dir.join("AGENTS.md"), "agents rules").unwrap();
        assert_eq!(
            existing_memory_file_name(&dir).as_deref(),
            Some("AGENTS.md")
        );
    }

    /// 一个候选都不存在时返回 None，调用方据此回退到规范名 AGENTS.md。
    #[test]
    fn init_target_is_none_when_no_candidate_exists() {
        let dir = unique_dir("init-target-none");
        assert_eq!(existing_memory_file_name(&dir), None);
        // 全空白的 AGENTS.md 也不算「有」，否则会写进一个永不生效的文件。
        std::fs::write(dir.join("AGENTS.md"), "  \n ").unwrap();
        assert_eq!(existing_memory_file_name(&dir), None);
    }

    #[test]
    fn push_dir_candidates_lists_the_file_that_is_actually_read() {
        // 面板与注入必须同源：不能出现「面板列了 GEMINI.md，实际读的是
        // CLAUDE.md」这种漂移。
        let dir = unique_dir("discover-fallback");
        std::fs::write(dir.join("CLAUDE.md"), "root rules").unwrap();
        std::fs::write(dir.join("CLAUDE.local.md"), "private rules").unwrap();
        std::fs::write(dir.join("GEMINI.md"), "must not win").unwrap();

        let mut out = vec![];
        push_dir_candidates(&mut out, &dir, ClaudeMdSource::Project);
        let shown: Vec<String> = out.iter().map(|f| f.path.display().to_string()).collect();
        assert!(shown.iter().any(|p| p.ends_with("CLAUDE.md")));
        assert!(shown.iter().any(|p| p.ends_with("CLAUDE.local.md")));
        assert!(!shown.iter().any(|p| p.ends_with("GEMINI.md")));
        assert!(out.iter().all(|f| f.exists));
    }

    #[test]
    fn push_dir_candidates_offers_agents_md_as_creation_entry_when_empty() {
        // 一个候选都不存在时仍要给出规范名的创建入口（/memory 面板回车新建）。
        let dir = unique_dir("discover-empty");
        let mut out = vec![];
        push_dir_candidates(&mut out, &dir, ClaudeMdSource::Project);
        assert_eq!(out.len(), 1);
        assert!(out[0].path.ends_with("AGENTS.md"));
        assert!(!out[0].exists);
    }

    #[test]
    fn ancestor_chain_injects_fallback_files_at_every_level() {
        // 端到端：祖先链每一级都走同一套回退规则（根 CLAUDE.md + 子目录
        // GEMINI.md 同时被注入）。这里手工装配 loader 而不调 `new()`，
        // 避开进程级 project_root override 与真实 `~/.wyj-code` 的干扰。
        let root = unique_dir("chain-fallback-root");
        let cwd = root.join("sub");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(root.join("CLAUDE.md"), "root level rules").unwrap();
        std::fs::write(cwd.join("GEMINI.md"), "sub level rules").unwrap();

        let loader = ClaudeMdLoader {
            global_dir: None,
            project_root: root.clone(),
            chain_dirs: vec![root.clone(), cwd.clone()],
            seen_dirs: Mutex::new(HashSet::from([root.clone(), cwd.clone()])),
        };
        let reminder = loader.turn_reminder().unwrap();
        assert!(reminder.contains("root level rules"));
        assert!(reminder.contains("sub level rules"));
        // 文件名已泛化，不再硬编码 AGENTS.md（回退生效时那句会变成假话）。
        assert!(!reminder.contains("AGENTS.md memory files"));
    }

    #[test]
    fn load_dir_files_returns_none_when_empty() {
        let dir = unique_dir("empty");
        assert!(load_dir_files(&dir).is_none());
    }

    #[test]
    fn resolve_imports_expands_relative_reference() {
        let dir = unique_dir("import");
        std::fs::write(dir.join("shared.md"), "shared body").unwrap();
        let content = "intro line\n@shared.md\ntail line";
        let expanded = resolve_imports(content, &dir, 0);
        assert!(expanded.contains("shared body"));
        assert!(expanded.contains("intro line"));
        assert!(expanded.contains("tail line"));
    }

    #[test]
    fn resolve_imports_skips_fenced_code_blocks() {
        let dir = unique_dir("import-fenced");
        std::fs::write(dir.join("shared.md"), "shared body").unwrap();
        let content = "```\n@shared.md\n```\n";
        let expanded = resolve_imports(content, &dir, 0);
        assert!(!expanded.contains("shared body"));
        assert!(expanded.contains("@shared.md"));
    }

    #[test]
    fn resolve_imports_leaves_unresolvable_token_untouched() {
        let dir = unique_dir("import-missing");
        let content = "contact me@example.com please";
        let expanded = resolve_imports(content, &dir, 0);
        assert_eq!(expanded.trim_end(), content);
    }

    #[test]
    fn collect_chain_orders_root_to_cwd() {
        let root = unique_dir("chain-root");
        let cwd = root.join("a").join("b");
        std::fs::create_dir_all(&cwd).unwrap();
        let chain = collect_chain(&root, &cwd);
        assert_eq!(chain.first(), Some(&root));
        assert_eq!(chain.last(), Some(&cwd));
        assert_eq!(chain.len(), 3);
    }

    #[test]
    fn maybe_dir_reminder_only_fires_once_per_dir() {
        let dir = unique_dir("subdir-dedup");
        std::fs::write(dir.join("AGENTS.md"), "subdir notes").unwrap();
        let loader = ClaudeMdLoader {
            global_dir: None,
            project_root: dir.clone(),
            chain_dirs: vec![],
            seen_dirs: Mutex::new(HashSet::new()),
        };
        assert!(loader.maybe_dir_reminder(&dir).is_some());
        assert!(loader.maybe_dir_reminder(&dir).is_none());
    }

    /// 回归钉子：触达目录是 `crates/core/src/` 而规约写在 `crates/core/`。
    /// 旧实现只看触达目录本身，永远返回 None，模块规约对模型完全不可见——
    /// 而「子目录惰性下沉」是本模块存在的理由，注释里写着的功能实际没生效。
    #[test]
    fn dir_reminder_walks_up_to_find_rules_in_an_ancestor() {
        let root = unique_dir("walk-up-root");
        let module = root.join("crates/core");
        let src = module.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(module.join("CLAUDE.md"), "core module rules").unwrap();

        let loader = ClaudeMdLoader {
            global_dir: None,
            project_root: root.clone(),
            chain_dirs: vec![root.clone()],
            seen_dirs: Mutex::new(HashSet::from([root.clone()])),
        };
        let reminder = loader
            .maybe_dir_reminder(&src)
            .expect("src/ 下无规约，但父目录 crates/core/ 有，必须注入");
        assert!(reminder.contains("core module rules"));
        assert!(reminder.contains(&module.display().to_string()));
    }

    /// 沿途**每一级**都要注入，不是只注入最近的那一级：嵌套模块各写一份
    /// 规约时，模型需要看到全部，不能因为 `inner/` 命中就跳过 `outer/`。
    #[test]
    fn dir_reminder_injects_every_unseen_level_on_the_way_up() {
        let root = unique_dir("walk-up-multi-root");
        let outer = root.join("outer");
        let inner = outer.join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(outer.join("AGENTS.md"), "outer rules").unwrap();
        std::fs::write(inner.join("AGENTS.md"), "inner rules").unwrap();

        let loader = ClaudeMdLoader {
            global_dir: None,
            project_root: root.clone(),
            chain_dirs: vec![root.clone()],
            seen_dirs: Mutex::new(HashSet::from([root.clone()])),
        };
        let reminder = loader.maybe_dir_reminder(&inner).unwrap();
        assert!(reminder.contains("outer rules"));
        assert!(reminder.contains("inner rules"));
    }

    /// 项目根是硬边界：读 `a/b/c/x.rs` 不该把项目根**之上**的目录也扫一遍——
    /// 那里出现的 AGENTS.md 与本项目无关。边界处（项目根本身）已在
    /// `seen_dirs` 里，所以既不重复注入、也不会因为「根上恰好有文件」而多发一次。
    #[test]
    fn dir_reminder_never_walks_above_project_root() {
        // 规约文件放在**项目根的上一层**，但那一层是本测试独占的临时目录，
        // 不去动 `temp_dir()` 本身——那会和并行跑的其它用例抢同一个 AGENTS.md。
        let outer = unique_dir("walk-up-boundary-outer");
        let root = outer.join("project");
        let deep = root.join("a/b/c");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(outer.join("AGENTS.md"), "OUTSIDE-THE-PROJECT rules").unwrap();

        let loader = ClaudeMdLoader {
            global_dir: None,
            project_root: root.clone(),
            chain_dirs: vec![root.clone()],
            seen_dirs: Mutex::new(HashSet::from([root.clone()])),
        };
        // 项目内没有任何规约文件 → 整条链上都是 None，边界外的那个不能被带进来。
        assert!(loader.maybe_dir_reminder(&deep).is_none());
    }

    /// 触达项目**外**的绝对路径时只取其自身，不向上爬。
    #[test]
    fn dir_reminder_outside_project_root_stops_immediately() {
        let root = unique_dir("walk-up-outside-root");
        let outside = unique_dir("walk-up-outside-target");
        std::fs::write(root.join("AGENTS.md"), "root rules").unwrap();
        std::fs::write(outside.join("AGENTS.md"), "outside rules").unwrap();

        let loader = ClaudeMdLoader {
            global_dir: None,
            project_root: root.clone(),
            chain_dirs: vec![root.clone()],
            seen_dirs: Mutex::new(HashSet::from([root.clone()])),
        };
        let reminder = loader.maybe_dir_reminder(&outside).unwrap();
        assert!(reminder.contains("outside rules"));
        assert!(
            !reminder.contains("root rules"),
            "项目外路径不该触发项目根规约的注入"
        );
    }

    /// 同一个目录经由不同拼写（`a/./b`、`a/b`、`a/c/../b`）到达时只能注入一次：
    /// 归一化不到位会让 `seen_dirs` 失效、同一份规约重复进上下文。
    #[test]
    fn dir_reminder_dedupes_across_lexically_equal_paths() {
        let root = unique_dir("walk-up-dedup-root");
        let a = root.join("a");
        let b = a.join("b");
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(b.join("AGENTS.md"), "deep rules").unwrap();

        let loader = ClaudeMdLoader {
            global_dir: None,
            project_root: root.clone(),
            chain_dirs: vec![root.clone()],
            seen_dirs: Mutex::new(HashSet::from([root.clone()])),
        };
        assert!(loader.maybe_dir_reminder(&b).is_some());
        for spelling in [a.join("b"), a.join("./b"), a.join("c/../b")] {
            assert!(
                loader.maybe_dir_reminder(&spelling).is_none(),
                "{} 与首轮是同一目录，不应再次注入",
                spelling.display()
            );
        }
    }

    /// 超长规约必须被封顶**且显式标注**：静默丢尾会让模型以为自己读全了规则。
    #[test]
    fn oversized_rules_are_truncated_with_an_explicit_marker() {
        let dir = unique_dir("oversize");
        std::fs::write(
            dir.join("AGENTS.md"),
            "x".repeat(MAX_RULE_FILE_BYTES + 4096),
        )
        .unwrap();
        let text = load_dir_files(&dir).unwrap();
        assert!(text.contains("[truncated:"));
        assert!(
            text.len() < MAX_RULE_FILE_BYTES + 4096,
            "截断后应显著短于原文"
        );
        // 未超限的文件一个字节都不能动（`resolve_imports` 会保留行尾换行，
        // 所以这里比对 trim 后的结果而不是原文）。
        let small = unique_dir("undersize");
        std::fs::write(small.join("AGENTS.md"), "short rules").unwrap();
        assert_eq!(load_dir_files(&small).unwrap().trim_end(), "short rules");
    }

    /// 截断点落在多字节字符中间时必须回退到 `char` 边界，不能 panic。
    /// 中文规约里 UTF-8 序列占 3 字节，这个边界不是理论问题。
    #[test]
    fn truncation_lands_on_a_char_boundary() {
        let text = "规".repeat(MAX_RULE_FILE_BYTES / 3 + 64);
        let out = truncate_rule_text(&text);
        assert!(out.contains("[truncated:"));
        assert!(out.starts_with("规"));
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }
}
