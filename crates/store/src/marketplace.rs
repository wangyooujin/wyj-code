//! Skill git marketplace：用户添加 git 仓库 URL 作为远程 Skill 源，
//! wyj-code clone/pull 到本地缓存目录后解析仓库根目录的 `marketplace.json` 清单。
//!
//! wyj-code 自定义的简化清单格式，不追求与 Claude Code marketplace.json 字段级兼容。

use crate::lockfile::{self, MarketplaceSource};
use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use tokio::process::Command;

#[derive(Debug, Clone, Deserialize)]
pub struct MarketplaceManifest {
    pub schema_version: u32,
    #[serde(default)]
    pub name: String,
    pub skills: Vec<MarketplaceSkillEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MarketplaceSkillEntry {
    pub name: String,
    pub description: String,
    pub version: String,
    /// 相对仓库根的路径，如 "skills/code-reviewer.md"
    pub path: String,
}

/// 对 git URL 取一个稳定的短 id，仅做缓存目录名/lockfile 主键用，无安全性要求。
pub fn marketplace_id(git_url: &str) -> String {
    let mut hasher = DefaultHasher::new();
    git_url.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn marketplaces_root() -> Result<PathBuf> {
    Ok(wyj_config::config_dir()?.join("marketplaces"))
}

fn cache_dir_under(cache_root: &Path, git_url: &str) -> PathBuf {
    cache_root.join(marketplace_id(git_url))
}

pub fn marketplace_cache_dir(git_url: &str) -> Result<PathBuf> {
    Ok(cache_dir_under(&marketplaces_root()?, git_url))
}

/// clone（首次）或 pull（已存在）marketplace 仓库到 `cache_root` 下，随后解析
/// `marketplace.json` 清单。`cache_root` 抽出为参数是为了在测试里注入临时目录，
/// 避免污染开发者真实的 `~/.wyj-code`；对外的 [`sync_marketplace`] 固定用真实根目录。
async fn sync_marketplace_in(
    cache_root: &Path,
    git_url: &str,
) -> Result<Vec<MarketplaceSkillEntry>> {
    let dir = cache_dir_under(cache_root, git_url);
    let dir_str = dir.to_string_lossy().to_string();

    // git 子进程默认继承父进程的 stdout/stderr——在 TUI（alternate screen）模式下，
    // clone/pull 的进度输出会直接写穿终端画面造成花屏，必须重定向掉。
    if dir.join(".git").exists() {
        let output = Command::new("git")
            .args(["-C", &dir_str, "pull", "--ff-only"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .output()
            .await
            .context("执行 git pull 失败（是否已安装 git？）")?;
        if !output.status.success() {
            bail!(
                "git pull 失败: {git_url}\n{}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
    } else {
        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建 marketplace 缓存目录失败: {}", parent.display()))?;
        }
        let output = Command::new("git")
            .args(["clone", "--depth", "1", git_url, &dir_str])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .output()
            .await
            .context("执行 git clone 失败（是否已安装 git？）")?;
        if !output.status.success() {
            bail!(
                "git clone 失败: {git_url}\n{}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
    }

    let manifest_path = dir.join("marketplace.json");
    if manifest_path.exists() {
        let content = std::fs::read_to_string(&manifest_path)
            .with_context(|| format!("marketplace.json 读取失败: {}", manifest_path.display()))?;
        let manifest: MarketplaceManifest = serde_json::from_str(&content)
            .with_context(|| format!("解析 marketplace.json 失败: {}", manifest_path.display()))?;
        return Ok(manifest.skills);
    }
    // 没有 wyj-code 自定义的 marketplace.json 清单——多数社区现成的 Claude Code
    // 命令/skill 集合（如 wshobson/commands）并不知道我们这个格式，退化为自动
    // 扫描仓库里的 .md 文件，让这些仓库不需要专门适配就能直接当默认源使用。
    auto_discover_skills(&dir)
}

const EXCLUDED_DIR_NAMES: &[&str] = &[".git", ".github", ".hg", ".svn", "node_modules"];
const EXCLUDED_FILE_NAMES: &[&str] = &[
    "readme.md",
    "license.md",
    "contributing.md",
    "changelog.md",
    "code_of_conduct.md",
];

/// repo 根目录没有 `marketplace.json` 时的兜底：递归扫描 `.md` 文件，每个文件当
/// 一个 skill 条目。version 用整个仓库当前 HEAD 的短 commit hash——没有逐文件
/// 版本号可用，仓库内容有更新即视为"有新版本"，语义上是准确的。
fn auto_discover_skills(dir: &Path) -> Result<Vec<MarketplaceSkillEntry>> {
    let version = git_head_short(dir).unwrap_or_else(|| "unknown".to_string());
    let mut entries = Vec::new();
    let mut seen_names = std::collections::HashSet::new();
    collect_md_files(dir, dir, &mut entries, &mut seen_names, &version);
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(entries)
}

pub fn git_head_short(dir: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["-C", &dir.to_string_lossy(), "rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn collect_md_files(
    root: &Path,
    current: &Path,
    entries: &mut Vec<MarketplaceSkillEntry>,
    seen_names: &mut std::collections::HashSet<String>,
    version: &str,
) {
    let Ok(read_dir) = std::fs::read_dir(current) else {
        return;
    };
    for item in read_dir.flatten() {
        let path = item.path();
        let file_name = item.file_name().to_string_lossy().to_string();
        if path.is_dir() {
            if file_name.starts_with('.') || EXCLUDED_DIR_NAMES.contains(&file_name.as_str()) {
                continue;
            }
            collect_md_files(root, &path, entries, seen_names, version);
            continue;
        }
        if !file_name.to_lowercase().ends_with(".md") {
            continue;
        }
        if EXCLUDED_FILE_NAMES.contains(&file_name.to_lowercase().as_str()) {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let rel_path = path.strip_prefix(root).unwrap_or(&path);
        let name = if seen_names.contains(stem) {
            // 文件名跨目录冲突：用"父目录-文件名"消歧
            let parent_name = path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str())
                .unwrap_or("");
            format!("{parent_name}-{stem}")
        } else {
            stem.to_string()
        };
        seen_names.insert(stem.to_string());
        let description = std::fs::read_to_string(&path)
            .ok()
            .and_then(|content| first_heading(&content))
            .unwrap_or_else(|| stem.to_string());
        entries.push(MarketplaceSkillEntry {
            name,
            description,
            version: version.to_string(),
            path: rel_path.to_string_lossy().replace('\\', "/"),
        });
    }
}

/// 与 `crates/commands/src/skill.rs` 的解析约定保持一致：跳过 YAML frontmatter，
/// 取正文第一个 `# 标题` 行作为展示用描述。
fn first_heading(content: &str) -> Option<String> {
    let content = content
        .strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---\n"))
        .map(|(_, body)| body)
        .unwrap_or(content);
    content
        .lines()
        .find_map(|l| l.strip_prefix("# ").map(|t| t.trim().to_string()))
}

pub async fn sync_marketplace(git_url: &str) -> Result<Vec<MarketplaceSkillEntry>> {
    let root = marketplaces_root()?;
    let entries = sync_marketplace_in(&root, git_url).await?;
    mark_synced(&marketplace_id(git_url)).ok();
    Ok(entries)
}

/// 添加一个 marketplace 源（只写全局 lockfile，已存在同 id 直接返回已有项）。
pub fn add_marketplace(git_url: &str) -> Result<MarketplaceSource> {
    let mut manifest = lockfile::load_global()?;
    let id = marketplace_id(git_url);
    if let Some(existing) = manifest.marketplaces.iter().find(|m| m.id == id) {
        return Ok(existing.clone());
    }
    let source = MarketplaceSource {
        id,
        git_url: git_url.to_string(),
        added_at: Utc::now(),
        last_synced_at: None,
    };
    manifest.marketplaces.push(source.clone());
    lockfile::save_global(&manifest)?;
    Ok(source)
}

/// 默认预置的 marketplace 源：一个真实存在、MIT 协议、持续维护的第三方社区
/// Claude Code 命令集合（57 个 tools/workflows，`tools/*.md` + `workflows/*.md`
/// 布局），本身没有 wyj-code 的 `marketplace.json` 清单，靠上面的自动扫描兜底
/// 解析。这是第三方内容，不隶属本项目、未经逐条审核，用户可在 `/skills` 的
/// Marketplaces tab 里随时移除或换成自己的源。
pub const DEFAULT_MARKETPLACE_URL: &str = "https://github.com/wshobson/commands.git";

fn default_marketplace_source() -> MarketplaceSource {
    MarketplaceSource {
        id: marketplace_id(DEFAULT_MARKETPLACE_URL),
        git_url: DEFAULT_MARKETPLACE_URL.to_string(),
        added_at: Utc::now(),
        last_synced_at: None,
    }
}

/// 首次使用（全局 lockfile 里一条 marketplace 源都没有）时预置默认源并持久化，
/// 之后原样返回已有列表。与 `registry::ensure_default_registry` 是同一套模式。
pub fn ensure_default_marketplace() -> Result<Vec<MarketplaceSource>> {
    let mut manifest = lockfile::load_global()?;
    if manifest.marketplaces.is_empty() {
        manifest.marketplaces.push(default_marketplace_source());
        lockfile::save_global(&manifest)?;
    }
    Ok(manifest.marketplaces)
}

/// 移除 marketplace 源记录 + 删除对应缓存目录。
pub fn remove_marketplace(id: &str) -> Result<()> {
    let mut manifest = lockfile::load_global()?;
    let Some(pos) = manifest.marketplaces.iter().position(|m| m.id == id) else {
        return Ok(());
    };
    let source = manifest.marketplaces.remove(pos);
    lockfile::save_global(&manifest)?;

    let dir = marketplace_cache_dir(&source.git_url)?;
    if dir.exists() {
        std::fs::remove_dir_all(&dir).ok();
    }
    Ok(())
}

fn mark_synced(id: &str) -> Result<()> {
    let mut manifest = lockfile::load_global()?;
    if let Some(m) = manifest.marketplaces.iter_mut().find(|m| m.id == id) {
        m.last_synced_at = Some(Utc::now());
        lockfile::save_global(&manifest)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;

    fn init_test_repo(root: &Path) {
        StdCommand::new("git")
            .args(["init", "-q"])
            .current_dir(root)
            .status()
            .unwrap();
        StdCommand::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(root)
            .status()
            .unwrap();
        StdCommand::new("git")
            .args(["config", "user.name", "test"])
            .current_dir(root)
            .status()
            .unwrap();

        std::fs::create_dir_all(root.join("skills")).unwrap();
        std::fs::write(
            root.join("skills").join("hello.md"),
            "# 测试 skill\n打印 Hello。",
        )
        .unwrap();
        std::fs::write(
            root.join("marketplace.json"),
            r#"{"schema_version":1,"name":"local-test","skills":[
                {"name":"hello","description":"测试 skill","version":"0.1.0","path":"skills/hello.md"}
            ]}"#,
        )
        .unwrap();

        StdCommand::new("git")
            .args(["add", "-A"])
            .current_dir(root)
            .status()
            .unwrap();
        StdCommand::new("git")
            .args(["commit", "-q", "-m", "init"])
            .current_dir(root)
            .status()
            .unwrap();
    }

    #[test]
    fn marketplace_id_is_deterministic_and_distinct() {
        let a = marketplace_id("https://github.com/example/skills-a");
        let b = marketplace_id("https://github.com/example/skills-a");
        let c = marketplace_id("https://github.com/example/skills-b");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[tokio::test]
    async fn sync_marketplace_clones_and_parses_manifest() {
        let repo_dir = tempfile::tempdir().unwrap();
        init_test_repo(repo_dir.path());
        let cache_root = tempfile::tempdir().unwrap();

        let git_url = format!("file://{}", repo_dir.path().display());
        let entries = sync_marketplace_in(cache_root.path(), &git_url)
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "hello");
        assert_eq!(entries[0].version, "0.1.0");
        assert_eq!(entries[0].path, "skills/hello.md");

        let cache_dir = cache_dir_under(cache_root.path(), &git_url);
        assert!(cache_dir.join("skills").join("hello.md").exists());

        // 二次 sync 走 pull 分支，不应报错
        sync_marketplace_in(cache_root.path(), &git_url)
            .await
            .unwrap();
    }

    /// 没有 marketplace.json、也没有任何其他 .md 文件（只有 README）时，退化为
    /// 自动扫描应得到空列表，而不是报错——空 skill 列表和"没有清单"是两回事。
    #[tokio::test]
    async fn sync_marketplace_missing_manifest_falls_back_to_empty_when_no_md_files() {
        let repo_dir = tempfile::tempdir().unwrap();
        StdCommand::new("git")
            .args(["init", "-q"])
            .current_dir(repo_dir.path())
            .status()
            .unwrap();
        StdCommand::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(repo_dir.path())
            .status()
            .unwrap();
        StdCommand::new("git")
            .args(["config", "user.name", "test"])
            .current_dir(repo_dir.path())
            .status()
            .unwrap();
        std::fs::write(repo_dir.path().join("README.md"), "no manifest here").unwrap();
        StdCommand::new("git")
            .args(["add", "-A"])
            .current_dir(repo_dir.path())
            .status()
            .unwrap();
        StdCommand::new("git")
            .args(["commit", "-q", "-m", "init"])
            .current_dir(repo_dir.path())
            .status()
            .unwrap();

        let cache_root = tempfile::tempdir().unwrap();
        let git_url = format!("file://{}", repo_dir.path().display());
        let entries = sync_marketplace_in(cache_root.path(), &git_url)
            .await
            .unwrap();
        assert!(entries.is_empty());
    }

    /// 没有 marketplace.json、但仓库里有常规约定的 .md 命令文件时（如
    /// wshobson/commands 那种 tools/*.md + workflows/*.md 布局，带
    /// `--- model: ... ---` frontmatter），应能自动发现并正确解析描述/路径。
    #[tokio::test]
    async fn sync_marketplace_auto_discovers_conventional_md_layout() {
        let repo_dir = tempfile::tempdir().unwrap();
        StdCommand::new("git")
            .args(["init", "-q"])
            .current_dir(repo_dir.path())
            .status()
            .unwrap();
        StdCommand::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(repo_dir.path())
            .status()
            .unwrap();
        StdCommand::new("git")
            .args(["config", "user.name", "test"])
            .current_dir(repo_dir.path())
            .status()
            .unwrap();
        std::fs::create_dir_all(repo_dir.path().join("tools")).unwrap();
        std::fs::write(
            repo_dir.path().join("tools").join("doc-generate.md"),
            "---\nmodel: claude-sonnet-4-0\n---\n\n# Automated Documentation Generation\n\n生成文档。\n\n$ARGUMENTS",
        )
        .unwrap();
        std::fs::write(repo_dir.path().join("README.md"), "not a skill").unwrap();
        StdCommand::new("git")
            .args(["add", "-A"])
            .current_dir(repo_dir.path())
            .status()
            .unwrap();
        StdCommand::new("git")
            .args(["commit", "-q", "-m", "init"])
            .current_dir(repo_dir.path())
            .status()
            .unwrap();

        let cache_root = tempfile::tempdir().unwrap();
        let git_url = format!("file://{}", repo_dir.path().display());
        let entries = sync_marketplace_in(cache_root.path(), &git_url)
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "doc-generate");
        assert_eq!(entries[0].description, "Automated Documentation Generation");
        assert_eq!(entries[0].path, "tools/doc-generate.md");
        assert!(!entries[0].version.is_empty());
        assert_ne!(entries[0].version, "unknown");
    }
}
