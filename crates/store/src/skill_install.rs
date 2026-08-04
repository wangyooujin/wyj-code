//! Skill 安装编排：从 marketplace 缓存目录读取 `.md` 内容原样写入 skill 目录 + lockfile。
//!
//! 严格遵守"不改动 SKILL.md 文件格式"：写入的 `.md` 就是 marketplace 仓库里原样的
//! markdown 文件内容，marketplace.json 里的 description/version 只是清单展示层元数据。

use crate::lockfile::{self, InstallScope, InstalledManifest, InstalledSkillEntry, SkillSource};
use crate::marketplace::{self, MarketplaceSkillEntry};
use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub struct SkillInstallRequest {
    pub marketplace_id: String,
    pub marketplace_url: String,
    pub entry: MarketplaceSkillEntry,
    pub scope: InstallScope,
    pub name_override: Option<String>,
}

pub struct GeneratedSkillInstallRequest<'a> {
    pub name: &'a str,
    pub content: &'a str,
    pub scope: InstallScope,
    pub source_id: &'a str,
}

#[derive(Debug, Serialize, Deserialize)]
struct GeneratedSkillBackup {
    previous_skill: Option<InstalledSkillEntry>,
    previous_extension: Option<lockfile::ExtensionLockEntry>,
    had_previous_content: bool,
}

fn skill_dir(scope: InstallScope, cwd: &Path) -> Result<PathBuf> {
    match scope {
        InstallScope::Global => Ok(wyj_config::config_dir()?.join("skills")),
        InstallScope::Project => Ok(wyj_config::project_config_dir(cwd).join("skills")),
    }
}

fn upsert_skill_entry(manifest: &mut InstalledManifest, entry: InstalledSkillEntry) {
    if let Some(existing) = manifest.skills.iter_mut().find(|e| e.name == entry.name) {
        *existing = entry;
    } else {
        manifest.skills.push(entry);
    }
}

fn validate_generated_name(name: &str) -> Result<()> {
    anyhow::ensure!(!name.is_empty(), "generated Skill name is empty");
    anyhow::ensure!(
        name.chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_')),
        "generated Skill name may only contain ASCII letters, digits, '-' or '_'"
    );
    Ok(())
}

fn generated_backup_paths(dest_path: &Path, source_id: &str) -> (PathBuf, PathBuf) {
    let digest = format!("{:x}", Sha256::digest(source_id.as_bytes()));
    let stem = dest_path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("skill");
    let prefix = format!(".{stem}.evolution-{}", &digest[..12]);
    let parent = dest_path.parent().unwrap_or_else(|| Path::new("."));
    (
        parent.join(format!("{prefix}.content.bak")),
        parent.join(format!("{prefix}.json")),
    )
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4().simple()));
    if let Err(error) = std::fs::write(&tmp, bytes) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error.into());
    }
    if let Err(error) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error.into());
    }
    Ok(())
}

/// 安装由 Evolution 生成且已经人工批准的 Skill。Skill 文件、回滚 sidecar 与
/// lockfile 都采用原子替换；任一步失败都会恢复安装前内容。
pub fn install_generated_skill(
    req: &GeneratedSkillInstallRequest<'_>,
    cwd: &Path,
) -> Result<PathBuf> {
    validate_generated_name(req.name)?;
    anyhow::ensure!(
        req.content.contains("---") && req.content.contains("name:"),
        "generated Skill is missing required frontmatter"
    );
    let dest_dir = skill_dir(req.scope, cwd)?;
    std::fs::create_dir_all(&dest_dir)?;
    let dest_path = dest_dir.join(format!("{}.md", req.name));
    let (content_backup, metadata_backup) = generated_backup_paths(&dest_path, req.source_id);
    let previous_content = std::fs::read(&dest_path).ok();
    let mut manifest = lockfile::load_scope(req.scope, cwd)?;
    let extension_id = format!("skill:{}", req.name);
    let previous_skill = manifest
        .skills
        .iter()
        .find(|entry| entry.name == req.name)
        .cloned();
    let previous_extension = manifest
        .extensions
        .iter()
        .find(|entry| entry.id == extension_id)
        .cloned();
    let backup = GeneratedSkillBackup {
        previous_skill,
        previous_extension,
        had_previous_content: previous_content.is_some(),
    };
    if let Some(bytes) = &previous_content {
        write_atomic(&content_backup, bytes)?;
    }
    write_atomic(&metadata_backup, &serde_json::to_vec_pretty(&backup)?)?;
    if let Err(error) = write_atomic(&dest_path, req.content.as_bytes()) {
        let _ = std::fs::remove_file(&content_backup);
        let _ = std::fs::remove_file(&metadata_backup);
        return Err(error).context("write generated Skill");
    }

    let now = Utc::now();
    let installed_at = backup
        .previous_skill
        .as_ref()
        .map(|entry| entry.installed_at)
        .unwrap_or(now);
    upsert_skill_entry(
        &mut manifest,
        InstalledSkillEntry {
            name: req.name.to_string(),
            version: Some("evolution-v1".to_string()),
            scope: req.scope,
            source: None,
            enabled: true,
            installed_at,
            updated_at: now,
        },
    );
    lockfile::upsert_extension(
        &mut manifest,
        lockfile::ExtensionLockEntry {
            id: extension_id,
            kind: lockfile::ExtensionKind::Skill,
            scope: req.scope,
            source: Some(format!("evolution:{}", req.source_id)),
            version: Some("evolution-v1".to_string()),
            commit: None,
            digest: Some(format!("{:x}", Sha256::digest(req.content.as_bytes()))),
            enabled: true,
            dependencies: Vec::new(),
            installed_at,
            updated_at: now,
        },
    );
    if let Err(error) = lockfile::save_scope(req.scope, cwd, &manifest) {
        match previous_content {
            Some(bytes) => {
                let _ = write_atomic(&dest_path, &bytes);
            }
            None => {
                let _ = std::fs::remove_file(&dest_path);
            }
        }
        let _ = std::fs::remove_file(&content_backup);
        let _ = std::fs::remove_file(&metadata_backup);
        return Err(error).context("write generated Skill lockfile; installation rolled back");
    }
    Ok(dest_path)
}

/// 回滚指定 Evolution candidate 安装的 Skill，并恢复它覆盖前的文件与 lockfile
/// 条目。sidecar 不存在时拒绝操作，避免误删用户手工维护的同名 Skill。
pub fn rollback_generated_skill(
    name: &str,
    source_id: &str,
    scope: InstallScope,
    cwd: &Path,
) -> Result<PathBuf> {
    validate_generated_name(name)?;
    let dest_path = skill_dir(scope, cwd)?.join(format!("{name}.md"));
    let (content_backup, metadata_backup) = generated_backup_paths(&dest_path, source_id);
    let backup: GeneratedSkillBackup =
        serde_json::from_slice(&std::fs::read(&metadata_backup).with_context(|| {
            format!(
                "missing Evolution rollback metadata: {}",
                metadata_backup.display()
            )
        })?)?;
    let current_content = std::fs::read(&dest_path).ok();
    let mut manifest = lockfile::load_scope(scope, cwd)?;
    manifest.skills.retain(|entry| entry.name != name);
    lockfile::remove_extension(&mut manifest, &format!("skill:{name}"));
    if let Some(entry) = backup.previous_skill.clone() {
        upsert_skill_entry(&mut manifest, entry);
    }
    if let Some(entry) = backup.previous_extension.clone() {
        lockfile::upsert_extension(&mut manifest, entry);
    }
    if backup.had_previous_content {
        write_atomic(
            &dest_path,
            &std::fs::read(&content_backup).context("read generated Skill content backup")?,
        )?;
    } else if dest_path.exists() {
        std::fs::remove_file(&dest_path)?;
    }
    if let Err(error) = lockfile::save_scope(scope, cwd, &manifest) {
        if let Some(bytes) = current_content {
            let _ = write_atomic(&dest_path, &bytes);
        }
        return Err(error).context("restore generated Skill lockfile");
    }
    let _ = std::fs::remove_file(content_backup);
    let _ = std::fs::remove_file(metadata_backup);
    Ok(dest_path)
}

/// 从 marketplace 缓存目录读取 `entry.path` 对应 `.md` 内容，原样写入
/// `~/.wyj-code/skills/<name>.md`（Global）或 `<cwd>/.wyj-code/skills/<name>.md`（Project），
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
    let previous_content = std::fs::read(&dest_path).ok();
    std::fs::write(&dest_path, content)
        .with_context(|| format!("写入 skill 文件失败: {}", dest_path.display()))?;

    let mut manifest = lockfile::load_scope(req.scope, cwd)?;
    let now = Utc::now();
    let existing_installed_at = manifest
        .skills
        .iter()
        .find(|e| e.name == name)
        .map(|e| e.installed_at);
    let extension_id = format!("skill:{name}");
    let marketplace_commit = marketplace::git_head_short(cache_dir);
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
    lockfile::upsert_extension(
        &mut manifest,
        lockfile::ExtensionLockEntry {
            id: extension_id,
            kind: lockfile::ExtensionKind::Skill,
            scope: req.scope,
            source: Some(req.marketplace_url.clone()),
            version: Some(req.entry.version.clone()),
            commit: marketplace_commit,
            digest: None,
            enabled: true,
            dependencies: Vec::new(),
            installed_at: existing_installed_at.unwrap_or(now),
            updated_at: now,
        },
    );
    if let Err(error) = lockfile::save_scope(req.scope, cwd, &manifest) {
        match previous_content {
            Some(content) => {
                let _ = std::fs::write(&dest_path, content);
            }
            None => {
                let _ = std::fs::remove_file(&dest_path);
            }
        }
        return Err(error).context("写入 skill lockfile 失败，已回滚 skill 文件");
    }
    Ok(())
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
    lockfile::remove_extension(&mut manifest, &format!("skill:{name}"));
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
    let id = format!("skill:{name}");
    if let Some(existing) = manifest.extensions.iter_mut().find(|e| e.id == id) {
        existing.enabled = enabled;
        existing.updated_at = now;
    } else {
        lockfile::upsert_extension(
            &mut manifest,
            lockfile::ExtensionLockEntry {
                id,
                kind: lockfile::ExtensionKind::Skill,
                scope,
                source: None,
                version: None,
                commit: None,
                digest: None,
                enabled,
                dependencies: Vec::new(),
                installed_at: now,
                updated_at: now,
            },
        );
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
            .join(".wyj-code")
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

    #[test]
    fn generated_skill_install_and_rollback_restore_previous_state() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join(".wyj-code").join("skills");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let path = skill_dir.join("release.md");
        std::fs::write(&path, "old skill").unwrap();
        set_skill_enabled("release", InstallScope::Project, dir.path(), false).unwrap();

        let content = "---\nname: release\ndescription: \"release safely\"\n---\n\nRun checks.\n";
        install_generated_skill(
            &GeneratedSkillInstallRequest {
                name: "release",
                content,
                scope: InstallScope::Project,
                source_id: "cand-skill-1",
            },
            dir.path(),
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
        let installed = lockfile::load_project(dir.path()).unwrap();
        assert!(installed.skills[0].enabled);
        assert_eq!(
            installed.extensions[0].source.as_deref(),
            Some("evolution:cand-skill-1")
        );

        rollback_generated_skill("release", "cand-skill-1", InstallScope::Project, dir.path())
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old skill");
        let restored = lockfile::load_project(dir.path()).unwrap();
        assert!(!restored.skills[0].enabled);
        assert!(restored.extensions[0].source.is_none());
    }
}
