//! Skill 安装编排：从 marketplace 缓存目录读取 `.md` 内容原样写入 skill 目录 + lockfile。
//!
//! 严格遵守"不改动 SKILL.md 文件格式"：写入的 `.md` 就是 marketplace 仓库里原样的
//! markdown 文件内容，marketplace.json 里的 description/version 只是清单展示层元数据。

use crate::lockfile::{self, InstallScope, InstalledManifest, InstalledSkillEntry, SkillSource};
use crate::marketplace::{self, MarketplaceSkillEntry};
use anyhow::{Context, Result};
use chrono::Utc;
use std::path::{Path, PathBuf};

pub struct SkillInstallRequest {
    pub marketplace_id: String,
    pub marketplace_url: String,
    pub entry: MarketplaceSkillEntry,
    pub scope: InstallScope,
    pub name_override: Option<String>,
}

fn skill_dir(scope: InstallScope, cwd: &Path) -> Result<PathBuf> {
    match scope {
        InstallScope::Global => Ok(wyj_config::config_dir()?.join("skills")),
        InstallScope::Project => Ok(cwd.join(".wyj").join("skills")),
    }
}

fn upsert_skill_entry(manifest: &mut InstalledManifest, entry: InstalledSkillEntry) {
    if let Some(existing) = manifest.skills.iter_mut().find(|e| e.name == entry.name) {
        *existing = entry;
    } else {
        manifest.skills.push(entry);
    }
}

/// 从 marketplace 缓存目录读取 `entry.path` 对应 `.md` 内容，原样写入
/// `~/.wyj-code/skills/<name>.md`（Global）或 `<cwd>/.wyj/skills/<name>.md`（Project），
/// 同时写对应 scope 的 lockfile（同名视为"覆盖式升级"）。
pub fn install_skill(req: &SkillInstallRequest, cwd: &Path) -> Result<()> {
    let cache_dir = marketplace::marketplace_cache_dir(&req.marketplace_url)?;
    install_skill_from_cache(&cache_dir, req, cwd)
}

/// `install_skill` 的核心逻辑，`cache_dir` 抽出为参数便于测试注入临时目录
/// （避免测试触碰真实 `~/.wyj-code`）。
fn install_skill_from_cache(cache_dir: &Path, req: &SkillInstallRequest, cwd: &Path) -> Result<()> {
    let source_path = cache_dir.join(&req.entry.path);
    let content = std::fs::read_to_string(&source_path)
        .with_context(|| format!("读取 marketplace skill 文件失败: {}", source_path.display()))?;

    let name = req
        .name_override
        .clone()
        .unwrap_or_else(|| req.entry.name.clone());
    let dest_dir = skill_dir(req.scope, cwd)?;
    std::fs::create_dir_all(&dest_dir)
        .with_context(|| format!("创建 skill 目录失败: {}", dest_dir.display()))?;
    let dest_path = dest_dir.join(format!("{name}.md"));
    std::fs::write(&dest_path, content)
        .with_context(|| format!("写入 skill 文件失败: {}", dest_path.display()))?;

    let mut manifest = lockfile::load_scope(req.scope, cwd)?;
    let now = Utc::now();
    let existing_installed_at = manifest
        .skills
        .iter()
        .find(|e| e.name == name)
        .map(|e| e.installed_at);
    upsert_skill_entry(
        &mut manifest,
        InstalledSkillEntry {
            name,
            version: Some(req.entry.version.clone()),
            scope: req.scope,
            source: Some(SkillSource {
                marketplace_id: req.marketplace_id.clone(),
                marketplace_url: req.marketplace_url.clone(),
                entry_path: req.entry.path.clone(),
            }),
            enabled: true,
            installed_at: existing_installed_at.unwrap_or(now),
            updated_at: now,
        },
    );
    lockfile::save_scope(req.scope, cwd, &manifest)
}

/// 升级：重新 sync 该 skill 所属 marketplace 拿最新版本，若版本变化则重新拷贝内容
/// 并返回 `Upgraded`；否则返回 `AlreadyLatest` 且不做任何改动。仅对纳管条目
/// （`source.is_some()`）开放。
pub async fn upgrade_skill(
    name: &str,
    scope: InstallScope,
    cwd: &Path,
) -> Result<crate::UpgradeOutcome> {
    let manifest = lockfile::load_scope(scope, cwd)?;
    let entry = manifest
        .skills
        .iter()
        .find(|e| e.name == name)
        .ok_or_else(|| anyhow::anyhow!("未找到已安装的 skill: {name}"))?;
    let source = entry
        .source
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("'{name}' 是手动配置项，没有版本信息，无法升级"))?;

    let entries = marketplace::sync_marketplace(&source.marketplace_url).await?;
    let latest = entries
        .into_iter()
        .find(|e| e.path == source.entry_path)
        .ok_or_else(|| {
            anyhow::anyhow!("marketplace 清单中未找到该 skill: {}", source.entry_path)
        })?;

    if Some(latest.version.as_str()) == entry.version.as_deref() {
        return Ok(crate::UpgradeOutcome::AlreadyLatest {
            version: latest.version,
        });
    }

    let req = SkillInstallRequest {
        marketplace_id: source.marketplace_id.clone(),
        marketplace_url: source.marketplace_url.clone(),
        entry: latest.clone(),
        scope,
        name_override: Some(name.to_string()),
    };
    install_skill(&req, cwd)?;
    Ok(crate::UpgradeOutcome::Upgraded {
        version: latest.version,
    })
}

/// 卸载：删除对应 scope 目录下的 `.md` 文件 + 从 lockfile 删除 entry。
pub fn uninstall_skill(name: &str, scope: InstallScope, cwd: &Path) -> Result<()> {
    let dest_dir = skill_dir(scope, cwd)?;
    let dest_path = dest_dir.join(format!("{name}.md"));
    if dest_path.exists() {
        std::fs::remove_file(&dest_path)
            .with_context(|| format!("删除 skill 文件失败: {}", dest_path.display()))?;
    }
    let mut manifest = lockfile::load_scope(scope, cwd)?;
    manifest.skills.retain(|e| e.name != name);
    lockfile::save_scope(scope, cwd, &manifest)
}

/// 启用/禁用（仅改 lockfile.enabled；内置/手动 skill 若无记录会补一条
/// `source: None` 的记录用来持久化 enabled 状态）。
pub fn set_skill_enabled(name: &str, scope: InstallScope, cwd: &Path, enabled: bool) -> Result<()> {
    let mut manifest = lockfile::load_scope(scope, cwd)?;
    let now = Utc::now();
    if let Some(existing) = manifest.skills.iter_mut().find(|e| e.name == name) {
        existing.enabled = enabled;
        existing.updated_at = now;
    } else {
        manifest.skills.push(InstalledSkillEntry {
            name: name.to_string(),
            version: None,
            scope,
            source: None,
            enabled,
            installed_at: now,
            updated_at: now,
        });
    }
    lockfile::save_scope(scope, cwd, &manifest)
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

    fn fake_request(marketplace_url: &str) -> SkillInstallRequest {
        SkillInstallRequest {
            marketplace_id: marketplace::marketplace_id(marketplace_url),
            marketplace_url: marketplace_url.to_string(),
            entry: MarketplaceSkillEntry {
                name: "hello".to_string(),
                description: "测试 skill".to_string(),
                version: "0.1.0".to_string(),
                path: "skills/hello.md".to_string(),
            },
            scope: InstallScope::Project,
            name_override: None,
        }
    }

    #[test]
    fn install_then_uninstall_roundtrip() {
        // 用测试 git 仓库本身充当"缓存目录"（布局与真实 marketplace 缓存一致：
        // <cache_dir>/skills/hello.md + marketplace.json），避免触碰真实 ~/.wyj-code。
        let repo_dir = tempfile::tempdir().unwrap();
        init_test_repo(repo_dir.path());
        let marketplace_url = format!("file://{}", repo_dir.path().display());
        let project_dir = tempfile::tempdir().unwrap();

        let req = fake_request(&marketplace_url);
        install_skill_from_cache(repo_dir.path(), &req, project_dir.path()).unwrap();

        let dest_path = project_dir
            .path()
            .join(".wyj")
            .join("skills")
            .join("hello.md");
        assert!(dest_path.exists());
        assert_eq!(
            std::fs::read_to_string(&dest_path).unwrap(),
            "# 测试 skill\n打印 Hello。"
        );

        let manifest = lockfile::load_project(project_dir.path()).unwrap();
        assert_eq!(manifest.skills.len(), 1);
        assert!(manifest.skills[0].is_managed());
        assert_eq!(manifest.skills[0].version.as_deref(), Some("0.1.0"));

        uninstall_skill("hello", InstallScope::Project, project_dir.path()).unwrap();
        assert!(!dest_path.exists());
        let manifest = lockfile::load_project(project_dir.path()).unwrap();
        assert!(manifest.skills.is_empty());
    }

    #[test]
    fn set_enabled_creates_shadow_entry_for_manual_skill() {
        let dir = tempfile::tempdir().unwrap();
        set_skill_enabled("manual-skill", InstallScope::Project, dir.path(), false).unwrap();
        let manifest = lockfile::load_project(dir.path()).unwrap();
        assert_eq!(manifest.skills.len(), 1);
        assert!(!manifest.skills[0].enabled);
        assert!(!manifest.skills[0].is_managed());
    }
}
