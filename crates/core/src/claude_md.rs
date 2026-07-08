//! CLAUDE.md 记忆文件加载：对齐 Claude Code 的查找范围与注入方式。
//!
//! - 查找范围：全局 `~/.claude/CLAUDE.md` + 从 git 仓库根到 cwd 的祖先链，
//!   每级目录内 `CLAUDE.md`/`CLAUDE.local.md` 都存在就都读（local 视作个人覆盖追加），
//!   两者都不存在则回退读 `AGENTS.md`。
//! - 支持 `@path/to/file` 递归导入（深度上限 4，跳过 fenced code block）。
//! - 不缓存文件内容，只缓存"哪些目录参与"这个列表；内容每次调用都重新读盘，
//!   保证运行期间编辑立即生效、压缩后依然完整（因为每轮都重新拼装，不依赖历史消息）。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const MAX_IMPORT_DEPTH: u8 = 4;

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

/// 每次 Agent 构建时扫描一次，确定参与注入的目录列表；内容按需每轮重新读盘。
pub struct ClaudeMdLoader {
    global_dir: Option<PathBuf>,
    /// 祖先链目录，root → cwd 顺序
    chain_dirs: Vec<PathBuf>,
    /// 子目录动态加载去重（含 global_dir/chain_dirs 本身，避免重复展示）
    seen_dirs: Mutex<HashSet<PathBuf>>,
}

impl ClaudeMdLoader {
    pub fn new(cwd: &Path) -> Self {
        let global_dir = wyj_config::claude_home_dir().ok();
        let root = find_git_root(cwd).unwrap_or_else(|| cwd.to_path_buf());
        let chain_dirs = collect_chain(&root, cwd);

        let mut seen = HashSet::new();
        if let Some(g) = &global_dir {
            seen.insert(g.clone());
        }
        for d in &chain_dirs {
            seen.insert(d.clone());
        }

        Self {
            global_dir,
            chain_dirs,
            seen_dirs: Mutex::new(seen),
        }
    }

    /// 每轮对话开始时调用：重新读盘拼出完整的 `<system-reminder>` 文本，
    /// 无任何文件时返回 None。
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
            "The following CLAUDE.md memory files apply to the current project. Follow their instructions.",
            &sections,
        ))
    }

    /// 工具触达新目录时调用：若该目录此前未展示过且存在 CLAUDE.md 系文件，
    /// 返回一段独立的 reminder 文本；否则返回 None。
    pub fn maybe_dir_reminder(&self, dir: &Path) -> Option<String> {
        let dir = dir.to_path_buf();
        {
            let mut seen = self.seen_dirs.lock().unwrap();
            if seen.contains(&dir) {
                return None;
            }
            seen.insert(dir.clone());
        }
        let text = load_dir_files(&dir)?;
        Some(wrap_reminder(
            "The directory you just accessed has additional CLAUDE.md instructions. Follow them as well:",
            &[(ClaudeMdSource::Subdir, dir, text)],
        ))
    }
}

/// 供 `/memory` 面板调用的纯函数：列出当前 cwd 适用的全部候选文件（含不存在的）。
pub fn discover_files(cwd: &Path) -> Vec<DiscoveredFile> {
    let mut out = vec![];
    if let Ok(g) = wyj_config::claude_home_dir() {
        push_dir_candidates(&mut out, &g, ClaudeMdSource::Global);
    }
    let root = find_git_root(cwd).unwrap_or_else(|| cwd.to_path_buf());
    for d in collect_chain(&root, cwd) {
        push_dir_candidates(&mut out, &d, ClaudeMdSource::Project);
    }
    out
}

fn push_dir_candidates(out: &mut Vec<DiscoveredFile>, dir: &Path, source: ClaudeMdSource) {
    let claude = dir.join("CLAUDE.md");
    let local = dir.join("CLAUDE.local.md");
    let agents = dir.join("AGENTS.md");
    let claude_exists = claude.is_file();
    let local_exists = local.is_file();
    let agents_exists = agents.is_file();

    if claude_exists || (!local_exists && !agents_exists) {
        out.push(DiscoveredFile {
            path: claude,
            source,
            exists: claude_exists,
        });
    }
    if local_exists {
        out.push(DiscoveredFile {
            path: local,
            source,
            exists: true,
        });
    }
    if !claude_exists && !local_exists && agents_exists {
        out.push(DiscoveredFile {
            path: agents,
            source,
            exists: true,
        });
    }
}

/// 单个目录内的 CLAUDE.md 系文件读取规则：CLAUDE.md 和 CLAUDE.local.md 都存在就都读
/// （local 追加在后，视作个人覆盖增补）；两者都不存在则回退读 AGENTS.md。
fn load_dir_files(dir: &Path) -> Option<String> {
    let claude = dir.join("CLAUDE.md");
    let local = dir.join("CLAUDE.local.md");
    let has_claude = claude.is_file();
    let has_local = local.is_file();

    let mut parts = vec![];
    if has_claude {
        if let Some(c) = read_and_resolve(&claude, 0) {
            parts.push(c);
        }
    }
    if has_local {
        if let Some(c) = read_and_resolve(&local, 0) {
            parts.push(c);
        }
    }
    if !has_claude && !has_local {
        let agents = dir.join("AGENTS.md");
        if agents.is_file() {
            if let Some(c) = read_and_resolve(&agents, 0) {
                parts.push(c);
            }
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
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

pub(crate) fn find_git_root(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        if d.join(".git").exists() {
            return Some(d.to_path_buf());
        }
        dir = d.parent();
    }
    None
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
    fn load_dir_files_merges_claude_and_local() {
        let dir = unique_dir("merge");
        std::fs::write(dir.join("CLAUDE.md"), "shared rules").unwrap();
        std::fs::write(dir.join("CLAUDE.local.md"), "my private override").unwrap();
        let text = load_dir_files(&dir).unwrap();
        assert!(text.contains("shared rules"));
        assert!(text.contains("my private override"));
    }

    #[test]
    fn load_dir_files_falls_back_to_agents_md() {
        let dir = unique_dir("agents-fallback");
        std::fs::write(dir.join("AGENTS.md"), "agents content").unwrap();
        let text = load_dir_files(&dir).unwrap();
        assert!(text.contains("agents content"));
    }

    #[test]
    fn load_dir_files_ignores_agents_md_when_claude_md_present() {
        let dir = unique_dir("agents-ignored");
        std::fs::write(dir.join("CLAUDE.md"), "claude wins").unwrap();
        std::fs::write(dir.join("AGENTS.md"), "should not appear").unwrap();
        let text = load_dir_files(&dir).unwrap();
        assert!(text.contains("claude wins"));
        assert!(!text.contains("should not appear"));
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
    fn find_git_root_walks_up_to_dot_git() {
        let root = unique_dir("git-root");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let nested = root.join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(find_git_root(&nested), Some(root));
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
        std::fs::write(dir.join("CLAUDE.md"), "subdir notes").unwrap();
        let loader = ClaudeMdLoader {
            global_dir: None,
            chain_dirs: vec![],
            seen_dirs: Mutex::new(HashSet::new()),
        };
        assert!(loader.maybe_dir_reminder(&dir).is_some());
        assert!(loader.maybe_dir_reminder(&dir).is_none());
    }
}
