//! 配置备份的导出与导入（`wyj-code export` / `wyj-code import`）。
//!
//! # 装什么、为什么只装这些
//!
//! 备份范围刻意收窄到**换一台机器后真正需要搬走、且又必须由人决定是否合并**
//! 的三类东西：
//!
//! 1. `config.toml` 里的 **`[[profiles]]` 与 `active_profile``——分组是一套套
//!    端点凭据，换机后必须重建；这是本功能存在的首要理由。
//! 2. 全局 skill（`~/.wyj-code/skills/`）——用户自己写/装进来的斜杠命令正文。
//! 3. 全局 `installed.json` 里**属于 skill 的那部分**（版本 / 来源 / 启用状态 /
//!    v2 `extensions` 索引）。缺了它，`/skills` 面板会把导进来的技能显示成
//!    「手动配置」、无法升级、启用状态也丢了。
//!
//! **刻意不装**的：`mcp_servers`、各类全局开关（`[auto_mode]` / `[evolution]` /
//! `[storage]` / `[tools]` …）、会话、记忆、演化、workspaces、CAS。理由分两类：
//! 会话与运行态是「另一台机器上的另一段历史」，搬过去毫无意义；而全局开关与
//! MCP server 是**对这台机器的策略**，整块覆盖会悄悄改掉用户的权限档位与自动
//! 批准规则——那是最不该被一次「恢复备份」改变的东西。
//!
//! # 合并语义（导入）
//!
//! - **profile 按 `name` 合并，同名以导入包为准**；本机独有的分组原样保留。
//!   `active_profile` 只有在导入包里那个名字**本机原本就有**时才采用——否则一次
//!   导入会把你当前会话的端点切走，而这个动作没有任何可见的提示。
//! - **skill 目录已存在则整个跳过并报告**，绝不覆盖。用户手改过的 SKILL.md 是
//!   用户资产，被备份里的旧版本静默盖掉是不可接受的；需要更新就自己删掉再导。
//! - `installed.json` 的 skill 条目按 `name` union，同名以导入包为准（与 profile
//!   同规则），但**只动 skill 相关字段**，MCP / 插件 / marketplace 源一概不碰。
//!
//! # 明文密钥
//!
//! 默认**包含** `api_key`（否则备份在换机场景下没有意义），因此导出后必须由
//! 调用方醒目提示，且归档文件在 unix 上强制 `0600`。包里同时写入正文可读的
//! `manifest.json`，用户改名成 `.zip` 就能看清自己打包了什么。

use crate::archive;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use wyj_config::backup::ProfilesFragment;
use wyj_config::Config;

/// 归档内 manifest 的文件名。刻意放根目录、名字可读。
pub const MANIFEST_NAME: &str = "manifest.json";
/// 配置片段的归档路径（只含 profiles 与 active_profile 的 toml）。
pub const CONFIG_NAME: &str = "config.profiles.toml";
/// 全局 skill 在归档里的前缀。
pub const SKILLS_PREFIX: &str = "skills/";
/// 全局 `installed.json` 的 skill 片段在归档里的路径。
pub const LOCKFILE_NAME: &str = "installed.skills.json";

/// 当前归档格式版本。导入时**只接受已知版本**：格式变了却按老规则解析，
/// 会静默丢掉新字段，用户以为"备份恢复了"，实际少了一半东西。
pub const BUNDLE_FORMAT: u32 = 1;

/// 归档条目数上限——备份是小文件集合，超过这个数只可能是选错了文件。
const MAX_ENTRIES: usize = 4_096;
/// 解压后总体积上限，防 zip bomb。
const MAX_TOTAL_BYTES: u64 = 128 * 1024 * 1024;

// ── manifest ────────────────────────────────────────────────────────────────

/// `manifest.json` 的正文。**同时是导入时的自检依据**：导入前先看它，就能
/// 在没动任何文件的情况下告诉用户这个包是谁导的、什么时候、装了几样东西。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleManifest {
    /// 格式版本，见 [`BUNDLE_FORMAT`]。
    pub format: u32,
    /// 导出时的 wyj-code 版本，供诊断"这份备份来自哪个版本"。
    pub app_version: String,
    /// 导出时间（UTC，`YYYY-MM-DDTHH:MM:SSZ`）。
    pub exported_at: String,
    /// 导出机的主机名（尽力而为，拿不到就空串）。
    #[serde(default)]
    pub hostname: String,
    /// 是否含明文 API Key。导入侧据此给出安全提醒。
    pub contains_secrets: bool,
    /// 导出的 profile 名、skill 名，供导入前预览。
    #[serde(default)]
    pub profiles: Vec<String>,
    #[serde(default)]
    pub skills: Vec<String>,
}

// ── 导出 ────────────────────────────────────────────────────────────────────

/// 导出结果摘要，供 CLI 打人类可读输出。
#[derive(Debug, Clone)]
pub struct ExportReport {
    pub manifest: BundleManifest,
    /// 归档写入的绝对路径。
    pub path: PathBuf,
    /// 归档文件字节数。
    pub bytes: u64,
    /// 按当前 `api_key` 字段非空统计的、**含明文密钥**的分组名。
    pub profiles_with_keys: Vec<String>,
}

/// 把当前全局配置与全局 skill 导出到 `dest`。
///
/// `config` 是调用方已经加载好的配置（避免本模块自己去猜路径/cwd），`home`
/// 是数据根（通常是 `~/.wyj-code`），`dest` 是目标文件。
///
/// 目标父目录不存在会创建；目标已存在会**直接覆盖**——调用方负责确认。
pub fn export(
    config: &Config,
    home: &Path,
    dest: &Path,
    app_version: &str,
    exported_at: &str,
) -> Result<ExportReport> {
    if let Some(parent) = dest.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建导出目录失败: {}", parent.display()))?;
    }

    let fragment = ProfilesFragment::from_config(config);
    let fragment_toml = fragment.to_toml()?;

    let skills_dir = home.join("skills");
    let skill_files = collect_skill_files(&skills_dir)?;
    let skill_names = roots_of(&skill_files);

    // 全局 lockfile 只取 skill 相关部分：MCP / 插件 / marketplace 源一概不进包。
    let lockfile_skills = collect_lockfile_skills(&home.join("installed.json"));

    let profiles_with_keys: Vec<String> = config
        .profiles
        .iter()
        .filter(|p| p.api_key.as_deref().is_some_and(|k| !k.trim().is_empty()))
        .map(|p| p.name.clone())
        .collect();

    let manifest = BundleManifest {
        format: BUNDLE_FORMAT,
        app_version: app_version.to_string(),
        exported_at: exported_at.to_string(),
        hostname: hostname(),
        contains_secrets: !profiles_with_keys.is_empty(),
        profiles: config.profiles.iter().map(|p| p.name.clone()).collect(),
        skills: skill_names.clone(),
    };

    let mut buf: Vec<u8> = Vec::new();
    {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let options: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        let mut write_file = |name: &str, data: &[u8]| -> Result<()> {
            writer
                .start_file(name, options)
                .with_context(|| format!("写入归档条目失败: {name}"))?;
            writer
                .write_all(data)
                .with_context(|| format!("写入归档条目失败: {name}"))?;
            Ok(())
        };

        write_file(
            MANIFEST_NAME,
            serde_json::to_string_pretty(&manifest)?.as_bytes(),
        )?;
        write_file(CONFIG_NAME, fragment_toml.as_bytes())?;
        if let Some(lockfile_skills) = &lockfile_skills {
            write_file(
                LOCKFILE_NAME,
                serde_json::to_string_pretty(lockfile_skills)?.as_bytes(),
            )?;
        }
        for rel in &skill_files {
            let abs = skills_dir.join(rel);
            let data = std::fs::read(&abs)
                .with_context(|| format!("读取 skill 文件失败: {}", abs.display()))?;
            write_file(&format!("{SKILLS_PREFIX}{rel}"), &data)?;
        }
        writer.finish().context("收尾归档失败")?;
    }

    // 先写临时文件再 rename：中途失败不会留下半截归档冒充"备份好了"。
    let tmp = dest.with_extension(format!("wyj-tmp-{}", std::process::id()));
    if let Err(error) = std::fs::write(&tmp, &buf) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error).with_context(|| format!("写入归档失败: {}", dest.display()));
    }
    restrict_permissions(&tmp);
    if let Err(error) = std::fs::rename(&tmp, dest) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error).with_context(|| format!("替换归档失败: {}", dest.display()));
    }

    Ok(ExportReport {
        manifest,
        path: dest.to_path_buf(),
        bytes: buf.len() as u64,
        profiles_with_keys,
    })
}

/// 递归收集 `skills/` 下的**普通文件**相对路径（`/` 分隔）。
///
/// 符号链接直接跳过：备份里塞一个指向 `/etc/passwd` 的链接，导入到另一台
/// 机器上就是一次任意文件写入。目录本身不单独占条目，空目录自然丢失——
/// 空目录对 skill 没有语义（加载器按 `SKILL.md` 与 `*.md` 找命令）。
fn collect_skill_files(skills_dir: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    if !skills_dir.is_dir() {
        return Ok(out);
    }
    walk(skills_dir, "", &mut out)?;
    out.sort();
    Ok(out)
}

fn walk(base: &Path, prefix: &str, out: &mut Vec<String>) -> Result<()> {
    let dir = if prefix.is_empty() {
        base.to_path_buf()
    } else {
        base.join(prefix)
    };
    let entries =
        std::fs::read_dir(&dir).with_context(|| format!("读取目录失败: {}", dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let rel = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };
        let file_type = entry
            .file_type()
            .with_context(|| format!("读取文件类型失败: {}", path.display()))?;
        if file_type.is_symlink() {
            tracing::warn!("跳过符号链接（不写入备份）: {}", path.display());
            continue;
        }
        // 根目录下的非 skill 文件（SkillHub 安装锁 / macOS 残留）既不进归档
        // 也不算 skill：导入侧从不还原它们，装进去只是白占体积，还会在
        // manifest 里被列成一个假技能。
        if prefix.is_empty() && NON_SKILL_ROOT_FILES.contains(&name) {
            continue;
        }
        if file_type.is_dir() {
            walk(base, &rel, out)?;
        } else if file_type.is_file() {
            out.push(rel);
        }
    }
    Ok(())
}

/// 从 skill 文件列表里提取 skill 安装根：`foo/SKILL.md` → `foo`、`bar.md` → `bar.md`、
/// 命名空间目录 `@org/name/SKILL.md` → `@org/name`。
///
/// 判定规则与 `commands::skill` 的加载器一致：**目录里有 `SKILL.md` 就是目录式
/// skill**，根是那个目录；单组件 `.md` 是单文件 skill，根是文件本身（保留 `.md`
/// 后缀，因为导入侧要拿它去比对磁盘上是否存在同名文件）。
fn roots_of(rels: &[String]) -> Vec<String> {
    let present: BTreeSet<&str> = rels.iter().map(String::as_str).collect();
    let mut roots = BTreeSet::new();
    for rel in rels {
        let root = skill_root_of(rel, &present);
        if NON_SKILL_ROOT_FILES.contains(&root.as_str()) {
            continue;
        }
        roots.insert(root);
    }
    roots.into_iter().collect()
}

/// `~/.wyj-code/skills/` 根下的**非 skill** 文件：SkillHub 的安装锁与 macOS
/// 打包残留。它们随目录一起备份（保留安装来源信息有用），但不该被当作一个
/// 叫这个名字的 skill 列进 manifest——那会让用户以为导出了一个假技能。
const NON_SKILL_ROOT_FILES: [&str; 2] = [".skills_store_lock.json", ".DS_Store"];

fn skill_root_of(rel: &str, present: &BTreeSet<&str>) -> String {
    let parts: Vec<&str> = rel.split('/').collect();
    if parts.len() == 1 {
        // 单文件 skill（`bar.md`）或根目录下的非 markdown 文件。后者不是 skill，
        // 但备份按「原样搬运」处理，落到下面同一个分支就够了。
        return parts[0].to_string();
    }
    // 目录式：找**最深的、含 SKILL.md 的祖先目录**。逐层试探而不是只看第一段，
    // 因为命名空间 skill 是 `@org/name/SKILL.md`，根应为 `@org/name`。
    for depth in (1..parts.len()).rev() {
        let candidate = parts[..depth].join("/");
        let marker = format!("{candidate}/SKILL.md");
        if present.contains(marker.as_str()) {
            return candidate;
        }
    }
    // 没有 SKILL.md 的目录（`references/` 之类）按最外层目录归组。
    parts[0].to_string()
}

/// 全局 `installed.json` 里与 skill 有关的条目（`skills` + `extensions` 中的
/// skill 项）。读不到就返回空——备份**不应该**因为一个损坏的 lockfile 而失败。
fn collect_lockfile_skills(lockfile_path: &Path) -> Option<serde_json::Value> {
    let manifest = crate::lockfile::load_global_at(lockfile_path).ok()?;
    let skills: Vec<_> = manifest.skills.clone();
    let extensions: Vec<_> = manifest
        .extensions
        .iter()
        .filter(|e| matches!(e.kind, crate::lockfile::ExtensionKind::Skill))
        .cloned()
        .collect();
    if skills.is_empty() && extensions.is_empty() {
        return None;
    }
    Some(serde_json::json!({
        "skills": skills,
        "extensions": extensions,
    }))
}

fn hostname() -> String {
    // macOS 既没有 `HOSTNAME`（那是 bash 的输出而非环境变量）也没有
    // `/etc/hostname`，所以这里多级回退；全都拿不到就返回空串——它只是诊断
    // 信息，不值得为一个"看起来很完整"的字段引入 libc 依赖。
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .or_else(|_| std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string()))
        .unwrap_or_default()
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    // 归档含明文 API Key，能读就等于能免费用你的额度。
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) {}

// ── 导入 ────────────────────────────────────────────────────────────────────

/// 导入要写到哪里。抽成参数是为了测试能注入 tempdir——本 crate 的约定是
/// **测试绝不触碰真实 `~/.wyj-code`**。
#[derive(Debug, Clone)]
pub struct ImportTargets {
    /// `config.toml` 的绝对路径。
    pub config_path: PathBuf,
    /// 全局 skills 目录（`~/.wyj-code/skills`）。
    pub skills_dir: PathBuf,
    /// 全局 `installed.json`（`~/.wyj-code/installed.json`）。
    pub lockfile_path: PathBuf,
}

impl ImportTargets {
    /// 生产路径：`~/.wyj-code/config.toml` + `~/.wyj-code/skills`。
    pub fn production() -> Result<Self> {
        let base = wyj_config::config_dir()?;
        Ok(Self {
            config_path: base.join("config.toml"),
            skills_dir: base.join("skills"),
            lockfile_path: base.join("installed.json"),
        })
    }
}

/// 导入结果。`dry_run` 时字段含义不变，只是磁盘没被动过。
#[derive(Debug, Clone)]
pub struct ImportReport {
    pub manifest: BundleManifest,
    /// 本机原本没有、被新加进来的分组名。
    pub profiles_added: Vec<String>,
    /// 本机已有、被导入包覆盖的分组名。
    pub profiles_replaced: Vec<String>,
    /// 实际切换了 `active_profile` 时的 `(旧, 新)`。
    pub active_profile_change: Option<(String, String)>,
    /// 新写入的 skill 根（目录名或 `xxx.md`）。
    pub skills_added: Vec<String>,
    /// 磁盘上已存在、因此整个跳过的 skill 根。
    pub skills_skipped: Vec<String>,
    /// 写入 `installed.json` 的 skill 元数据条数。
    pub lockfile_entries: usize,
    pub dry_run: bool,
}

/// 只读检查一份备份：解析 manifest 并校验格式版本，**不碰任何磁盘状态**。
///
/// 存在的意义是让导入能在动文件之前就报出「这不是 wyj-code 备份」「格式版本
/// 太新」这类硬错误，而不是解压到一半才发现。
pub fn inspect(path: &Path) -> Result<BundleManifest> {
    let bytes =
        std::fs::read(path).with_context(|| format!("读取备份文件失败: {}", path.display()))?;
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .with_context(|| format!("不是有效的 zip 归档: {}", path.display()))?;
    let entries = archive::list_entries(&mut archive)?;
    if !entries.iter().any(|name| name == MANIFEST_NAME) {
        bail!(
            "备份里没有 {MANIFEST_NAME}，这不是 wyj-code 导出的备份文件（若它是一个技能包，请用 `wyj-code skillhub install`）"
        );
    }
    let mut file = archive
        .by_name(MANIFEST_NAME)
        .context("读取 manifest 失败")?;
    let mut text = String::new();
    file.read_to_string(&mut text)
        .context("读取 manifest 失败")?;
    let manifest: BundleManifest =
        serde_json::from_str(&text).context("manifest 格式无法解析，备份可能已损坏")?;
    if manifest.format != BUNDLE_FORMAT {
        bail!(
            "备份格式版本为 {}，当前 wyj-code 只认识 {BUNDLE_FORMAT}；请升级 wyj-code 后再导入",
            manifest.format
        );
    }
    Ok(manifest)
}

/// 把备份合并进当前配置与全局 skills。
///
/// `config` 由调用方加载（带着 `loaded_baseline`），本函数**只改内存里的
/// `profiles`/`active_profile` 然后调 [`Config::save_to`]**——不整份覆盖磁盘，
/// 因此本机的 mcp_servers 与各类全局开关不会被这次导入碰掉。
///
/// 合并规则见模块文档。`dry_run` 为真时只计算报告，一个字节都不写。
pub fn import(
    path: &Path,
    config: &mut Config,
    targets: &ImportTargets,
    dry_run: bool,
) -> Result<ImportReport> {
    let manifest = inspect(path)?;
    let bytes =
        std::fs::read(path).with_context(|| format!("读取备份文件失败: {}", path.display()))?;
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .with_context(|| format!("不是有效的 zip 归档: {}", path.display()))?;
    let entries = archive::list_entries(&mut archive)?;
    if entries.len() > MAX_ENTRIES {
        bail!("备份条目过多（{} 条，上限 {MAX_ENTRIES}）", entries.len());
    }

    // 1) profiles
    let fragment = read_config_fragment(&mut archive, &entries)?;
    let local_names: BTreeSet<String> = config.profiles.iter().map(|p| p.name.clone()).collect();
    // 「本机还是空白配置」必须在**合并之前**判定：合并会把导入的分组 push 进
    // `config.profiles`，之后再问"是不是只有占位 default"必然为假，于是换机
    // 场景永远不会跟随备份的激活分组。
    let was_blank = config
        .profiles
        .iter()
        .all(|p| p.name == "default" && p.api_key.is_none() && p.base_url.trim().is_empty());
    let mut added = Vec::new();
    let mut replaced = Vec::new();
    for incoming in fragment.profiles {
        match config.profiles.iter_mut().find(|p| p.name == incoming.name) {
            Some(existing) => {
                replaced.push(incoming.name.clone());
                *existing = incoming;
            }
            None => {
                added.push(incoming.name.clone());
                config.profiles.push(incoming);
            }
        }
    }
    // `active_profile` 的采用条件是「本机原本就有这个分组」**或**「本机还是空白
    // 配置」。前者的理由是不让一次恢复备份静默把你当前会话的端点切到另一个
    // 供应商；后者是换机主场景——新机器上只有一个自动补的占位 `default`，
    // 这时不跟随备份等于导入完还得手动敲一次 `/profile`，而这个功能存在的
    // 理由正是"搬过去就能直接用"。
    let active_change = if fragment.active_profile != config.active_profile
        && (local_names.contains(&fragment.active_profile) || was_blank)
        && config
            .profiles
            .iter()
            .any(|p| p.name == fragment.active_profile)
    {
        let old = config.active_profile.clone();
        config.active_profile = fragment.active_profile.clone();
        Some((old, fragment.active_profile))
    } else {
        None
    };

    // 2) skills：先在归档条目里判重，再决定写入
    let total_bytes: u64 = entries
        .iter()
        .filter_map(|name| archive.by_name(name).ok().map(|f| f.size()))
        .sum();
    if total_bytes > MAX_TOTAL_BYTES {
        bail!("备份解压后体积超限（{total_bytes} 字节，上限 {MAX_TOTAL_BYTES}）");
    }
    let rels: Vec<String> = entries
        .iter()
        .filter_map(|name| name.strip_prefix(SKILLS_PREFIX))
        .map(str::to_string)
        .collect();
    let mut roots = roots_of(&rels);
    let mut skipped = Vec::new();
    roots.retain(|root| {
        let target = targets.skills_dir.join(root);
        if target.exists() {
            skipped.push(root.clone());
            false
        } else {
            true
        }
    });

    let mut report = ImportReport {
        manifest,
        profiles_added: added,
        profiles_replaced: replaced,
        active_profile_change: active_change,
        skills_added: roots.clone(),
        skills_skipped: skipped,
        lockfile_entries: 0,
        dry_run,
    };

    if dry_run {
        report.lockfile_entries = count_lockfile_entries(&mut archive, &entries)?;
        return Ok(report);
    }

    // 3) 落盘：配置 → skill 文件 → lockfile。
    //
    // 顺序不是随意的：先写配置（失败则整个导入中止、磁盘上什么都没变），
    // 再写 skill。反过来会出现「技能进来了但 profile 没进来」，而技能目录
    // 只增不改，重跑一次导入也不会补上配置。
    config
        .save_to(&targets.config_path)
        .with_context(|| format!("写入配置失败: {}", targets.config_path.display()))?;

    report.lockfile_entries = extract_skills(&mut archive, &entries, targets, &roots)?;
    Ok(report)
}

/// 读归档里的 `config.profiles.toml`。
fn read_config_fragment(
    archive: &mut zip::ZipArchive<std::io::Cursor<Vec<u8>>>,
    entries: &[String],
) -> Result<ProfilesFragment> {
    if !entries.iter().any(|name| name == CONFIG_NAME) {
        bail!("备份里没有 {CONFIG_NAME}，无法恢复任何 profile");
    }
    let mut file = archive
        .by_name(CONFIG_NAME)
        .context("读取 profile 片段失败")?;
    let mut text = String::new();
    file.read_to_string(&mut text)
        .context("读取 profile 片段失败")?;
    ProfilesFragment::from_toml(&text)
}

/// 只统计、不写入——dry-run 也要能说清楚"会写几条元数据"。
fn count_lockfile_entries(
    archive: &mut zip::ZipArchive<std::io::Cursor<Vec<u8>>>,
    entries: &[String],
) -> Result<usize> {
    Ok(read_lockfile_fragment(archive, entries)?
        .map(|f| f.skills.len() + f.extensions.len())
        .unwrap_or(0))
}

#[derive(Debug, Clone, Default, Deserialize)]
struct LockfileFragment {
    #[serde(default)]
    skills: Vec<crate::lockfile::InstalledSkillEntry>,
    #[serde(default)]
    extensions: Vec<crate::lockfile::ExtensionLockEntry>,
}

fn read_lockfile_fragment(
    archive: &mut zip::ZipArchive<std::io::Cursor<Vec<u8>>>,
    entries: &[String],
) -> Result<Option<LockfileFragment>> {
    if !entries.iter().any(|name| name == LOCKFILE_NAME) {
        return Ok(None);
    }
    let mut file = archive
        .by_name(LOCKFILE_NAME)
        .context("读取 skill 元数据失败")?;
    let mut text = String::new();
    file.read_to_string(&mut text)
        .context("读取 skill 元数据失败")?;
    let fragment: LockfileFragment =
        serde_json::from_str(&text).context("skill 元数据无法解析，备份可能已损坏")?;
    Ok(Some(fragment))
}

/// 把 `roots` 对应的 skill 文件写进 `targets.skills_dir`，并把元数据并入全局
/// `installed.json`。返回并入的元数据条数。
///
/// 写入走**暂存目录 + rename**：单个 skill 可能几十个文件（`references/` 整棵树），
/// 直接往目标目录里逐个写，中途失败会留下半个 skill 让加载器读到残缺正文。
fn extract_skills(
    archive: &mut zip::ZipArchive<std::io::Cursor<Vec<u8>>>,
    entries: &[String],
    targets: &ImportTargets,
    roots: &[String],
) -> Result<usize> {
    if !roots.is_empty() {
        std::fs::create_dir_all(&targets.skills_dir)
            .with_context(|| format!("创建 skills 目录失败: {}", targets.skills_dir.display()))?;
        let staging = targets
            .skills_dir
            .join(format!(".wyj-import-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(&staging)
            .with_context(|| format!("创建暂存目录失败: {}", staging.display()))?;

        let result = (|| -> Result<()> {
            let rel_roots: BTreeSet<&str> = roots.iter().map(String::as_str).collect();
            let rels: BTreeSet<&str> = entries
                .iter()
                .filter_map(|n| n.strip_prefix(SKILLS_PREFIX))
                .collect();
            for name in entries {
                let Some(rel) = name.strip_prefix(SKILLS_PREFIX) else {
                    continue;
                };
                if !rel_roots.contains(skill_root_of(rel, &rels).as_str()) {
                    continue;
                }
                let mut file = archive
                    .by_name(name)
                    .with_context(|| format!("读取备份条目失败: {name}"))?;
                let target = archive::resolve_within(&staging, rel)
                    .with_context(|| format!("拒绝越界条目: {name}"))?;
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("创建目录失败: {}", parent.display()))?;
                }
                let mut buf = Vec::new();
                file.read_to_end(&mut buf)
                    .with_context(|| format!("读取备份条目失败: {name}"))?;
                std::fs::write(&target, &buf)
                    .with_context(|| format!("写入 skill 文件失败: {}", target.display()))?;
            }
            Ok(())
        })();

        if let Err(error) = result {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(error);
        }

        // 逐个 rename 就位。目标在暂存期间可能被别人建出来了——再查一次，
        // 存在就跳过（与上面的判重同语义，宁可少装不可覆盖）。
        for root in roots {
            let source = staging.join(root);
            if !source.exists() {
                continue;
            }
            let target = targets.skills_dir.join(root);
            if target.exists() {
                continue;
            }
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("创建目录失败: {}", parent.display()))?;
            }
            std::fs::rename(&source, &target).with_context(|| {
                format!(
                    "安装 skill 失败: {} → {}",
                    source.display(),
                    target.display()
                )
            })?;
        }
        let _ = std::fs::remove_dir_all(&staging);
    }

    // 元数据：按 name / id union，同名以导入包为准，其它字段一律不动。
    let Some(fragment) = read_lockfile_fragment(archive, entries)? else {
        return Ok(0);
    };
    let mut manifest = crate::lockfile::load_global_at(&targets.lockfile_path).unwrap_or_default();
    let mut merged = 0usize;
    for entry in fragment.skills {
        match manifest.skills.iter_mut().find(|e| e.name == entry.name) {
            Some(existing) => *existing = entry,
            None => manifest.skills.push(entry),
        }
        merged += 1;
    }
    for entry in fragment.extensions {
        crate::lockfile::upsert_extension(&mut manifest, entry);
        merged += 1;
    }
    if merged > 0 {
        crate::lockfile::save_global_at(&targets.lockfile_path, &manifest)
            .context("写入 skill 元数据失败")?;
    }
    Ok(merged)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一份 home：`<home>/skills/...` + `<home>/installed.json`。
    fn make_home(files: &[(&str, &str)]) -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        for (rel, content) in files {
            let path = home.path().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, content).unwrap();
        }
        home
    }

    fn targets_for(home: &Path) -> ImportTargets {
        ImportTargets {
            config_path: home.join("config.toml"),
            skills_dir: home.join("skills"),
            lockfile_path: home.join("installed.json"),
        }
    }

    fn config_with(profiles: &[(&str, &str)], active: &str) -> Config {
        Config {
            active_profile: active.to_string(),
            profiles: profiles
                .iter()
                .map(|(name, key)| wyj_config::Profile {
                    name: (*name).to_string(),
                    api_key: Some((*key).to_string()),
                    ..wyj_config::Profile::default()
                })
                .collect(),
            ..Config::default()
        }
    }

    fn export_to(home: &Path, config: &Config) -> PathBuf {
        let dest = home.join("out.wyj");
        export(config, home, &dest, "0.3.20", "2026-10-24T00:00:00Z").unwrap();
        dest
    }

    /// 归档里必须**只有**约定的那几类条目：profile 片段、manifest、skills/。
    /// 多出任何东西都意味着将来有人顺手把会话或凭据打了进去。
    #[test]
    fn export_contains_only_profiles_and_skills() {
        let home = make_home(&[
            ("skills/demo/SKILL.md", "---\nname: demo\n---\nbody"),
            ("skills/demo/references/a.md", "# a"),
            ("skills/single.md", "---\nname: single\n---\n"),
            // 不在 skills/ 下的东西一律不该进包
            ("sessions/x.json", "{}"),
            ("AGENTS.md", "memory"),
        ]);
        let config = config_with(&[("work", "sk-1")], "work");
        let dest = export_to(home.path(), &config);

        let bytes = std::fs::read(&dest).unwrap();
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let names = archive::list_entries(&mut zip).unwrap();

        assert!(names.contains(&MANIFEST_NAME.to_string()));
        assert!(names.contains(&CONFIG_NAME.to_string()));
        assert!(names.contains(&"skills/demo/SKILL.md".to_string()));
        assert!(names.contains(&"skills/demo/references/a.md".to_string()));
        assert!(names.contains(&"skills/single.md".to_string()));
        assert!(
            !names.iter().any(|n| n.starts_with("sessions/")),
            "会话不该进备份: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n == "AGENTS.md"),
            "全局记忆不在备份范围内: {names:?}"
        );
    }

    /// skill 根名推断：目录式取含 `SKILL.md` 的最深目录，单文件保留 `.md`，
    /// 命名空间 skill 不能退化成第一段（`@org`）。
    #[test]
    fn skill_roots_cover_flat_namespaced_and_single_file_forms() {
        let rels: Vec<String> = [
            "demo/SKILL.md",
            "demo/references/a.md",
            "single.md",
            "@org/name/SKILL.md",
            "@org/name/scripts/x.sh",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(
            roots_of(&rels),
            vec![
                "@org/name".to_string(),
                "demo".to_string(),
                "single.md".to_string()
            ]
        );
    }

    /// 导出 → 导入到"另一台机器"：profile 全量还原，skill 文件落盘，
    /// 明文 key 保留（这是备份的意义）。
    #[test]
    fn round_trip_restores_profiles_and_skills() {
        let source = make_home(&[
            ("skills/demo/SKILL.md", "---\nname: demo\n---\nbody"),
            ("skills/demo/references/a.md", "# a"),
        ]);
        let config = config_with(&[("work", "sk-secret")], "work");
        let archive_path = export_to(source.path(), &config);

        let target = make_home(&[]);
        let mut incoming = Config::default();
        let report = import(
            &archive_path,
            &mut incoming,
            &targets_for(target.path()),
            false,
        )
        .unwrap();

        assert_eq!(report.profiles_added, vec!["work".to_string()]);
        assert!(report.profiles_replaced.is_empty());
        assert_eq!(report.skills_added, vec!["demo".to_string()]);
        assert!(report.skills_skipped.is_empty());

        let restored = Config::load_file_only_at(&target.path().join("config.toml")).unwrap();
        // 目标机原本没有 config.toml，加载时补的那个空壳 `default` 分组会留下
        // ——它本来就在那儿（不导入也存在），不属于导入引入的东西。
        let work = restored
            .profile_by_name("work")
            .expect("导入的分组必须存在");
        assert_eq!(work.api_key.as_deref(), Some("sk-secret"));
        // 目标机是空白配置（只有一个占位 default），激活分组必须跟随备份，
        // 否则"搬过去就能用"不成立。
        assert_eq!(restored.active_profile, "work");

        assert_eq!(
            std::fs::read_to_string(target.path().join("skills/demo/SKILL.md")).unwrap(),
            "---\nname: demo\n---\nbody"
        );
        assert!(target.path().join("skills/demo/references/a.md").is_file());
    }

    /// 同名 profile 以导入包为准，本机独有的分组必须原样保留。
    #[test]
    fn import_merges_profiles_by_name_and_keeps_local_only_ones() {
        let source = make_home(&[]);
        let config = config_with(&[("shared", "new-key")], "shared");
        let archive_path = export_to(source.path(), &config);

        let target = make_home(&[]);
        let seed = config_with(
            &[("shared", "old-key"), ("local-only", "local-key")],
            "local-only",
        );
        // 让本机 config.toml 真实存在，`save_to` 才有可合并的磁盘基线
        seed.save_to(&target.path().join("config.toml")).unwrap();
        let mut local = Config::load_file_only_at(&target.path().join("config.toml")).unwrap();

        let report = import(
            &archive_path,
            &mut local,
            &targets_for(target.path()),
            false,
        )
        .unwrap();
        assert_eq!(report.profiles_added, Vec::<String>::new());
        assert_eq!(report.profiles_replaced, vec!["shared".to_string()]);

        let merged = Config::load_file_only_at(&target.path().join("config.toml")).unwrap();
        let names: Vec<&str> = merged.profiles.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"shared"));
        assert!(names.contains(&"local-only"), "本机独有分组不能丢");
        let shared = merged.profile_by_name("shared").unwrap();
        assert_eq!(shared.api_key.as_deref(), Some("new-key"), "同名以导入为准");
        // 本机原本激活的是 local-only，导入包激活的是 shared。
        // `shared` 本机也存在 → 允许切换（这是备份明确表达的用户意图）。
        assert_eq!(merged.active_profile, "shared");
    }

    /// 导入包里的 `active_profile` 指向一个**本机本来没有**的分组时，
    /// 不能把激活分组切过去——否则一次恢复备份会静默换掉当前端点。
    #[test]
    fn import_does_not_activate_a_profile_that_was_newly_added() {
        let source = make_home(&[]);
        let config = config_with(&[("archive-only", "k1")], "archive-only");
        let archive_path = export_to(source.path(), &config);

        let target = make_home(&[]);
        let seed = config_with(&[("keep-me", "k2")], "keep-me");
        seed.save_to(&target.path().join("config.toml")).unwrap();
        let mut local = Config::load_file_only_at(&target.path().join("config.toml")).unwrap();

        let report = import(
            &archive_path,
            &mut local,
            &targets_for(target.path()),
            false,
        )
        .unwrap();
        assert_eq!(report.profiles_added, vec!["archive-only".to_string()]);
        assert!(report.active_profile_change.is_none());

        let merged = Config::load_file_only_at(&target.path().join("config.toml")).unwrap();
        assert_eq!(merged.active_profile, "keep-me", "激活分组不该被切走");
        assert_eq!(merged.profiles.len(), 2);
    }

    /// 已存在的 skill 目录整个跳过、绝不覆盖，并在报告里列出来。
    #[test]
    fn import_never_overwrites_an_existing_skill() {
        let source = make_home(&[("skills/demo/SKILL.md", "from-backup")]);
        let config = config_with(&[("w", "k")], "w");
        let archive_path = export_to(source.path(), &config);

        let target = make_home(&[("skills/demo/SKILL.md", "locally-edited")]);
        let mut incoming = Config::default();
        let report = import(
            &archive_path,
            &mut incoming,
            &targets_for(target.path()),
            false,
        )
        .unwrap();

        assert_eq!(report.skills_added, Vec::<String>::new());
        assert_eq!(report.skills_skipped, vec!["demo".to_string()]);
        assert_eq!(
            std::fs::read_to_string(target.path().join("skills/demo/SKILL.md")).unwrap(),
            "locally-edited",
            "用户手改过的正文不能被备份盖掉"
        );
    }

    /// dry-run 一个字节都不写——包括 config.toml 与 skills/。
    #[test]
    fn dry_run_touches_nothing() {
        let source = make_home(&[("skills/demo/SKILL.md", "body")]);
        let config = config_with(&[("w", "k")], "w");
        let archive_path = export_to(source.path(), &config);

        let target = make_home(&[]);
        let mut incoming = Config::default();
        let report = import(
            &archive_path,
            &mut incoming,
            &targets_for(target.path()),
            true,
        )
        .unwrap();
        assert!(report.dry_run);
        assert_eq!(report.profiles_added, vec!["w".to_string()]);

        assert!(!target.path().join("config.toml").exists());
        assert!(!target.path().join("skills").exists());
    }

    /// skill 安装元数据按 name union 合并，且**只写 skill 相关字段**。
    #[test]
    fn import_merges_skill_metadata_without_touching_other_lockfile_arrays() {
        let source = make_home(&[]);
        std::fs::write(
            source.path().join("installed.json"),
            serde_json::json!({
                "version": 2,
                "skills": [{
                    "name": "demo",
                    "version": "1.2.3",
                    "scope": "global",
                    "enabled": true,
                    "installed_at": "2026-01-01T00:00:00Z",
                    "updated_at": "2026-01-01T00:00:00Z"
                }],
                "mcp_servers": [{
                    "name": "should-not-travel",
                    "version": null,
                    "source": null,
                    "enabled": true,
                    "installed_at": "2026-01-01T00:00:00Z",
                    "updated_at": "2026-01-01T00:00:00Z"
                }]
            })
            .to_string(),
        )
        .unwrap();
        let config = config_with(&[("w", "k")], "w");
        let archive_path = export_to(source.path(), &config);

        // 目标机已有自己的 MCP 条目，导入后必须还在。
        let target = make_home(&[]);
        std::fs::write(
            target.path().join("installed.json"),
            serde_json::json!({
                "version": 2,
                "skills": [],
                "mcp_servers": [{
                    "name": "local-mcp",
                    "version": null,
                    "source": null,
                    "enabled": true,
                    "installed_at": "2026-02-02T00:00:00Z",
                    "updated_at": "2026-02-02T00:00:00Z"
                }]
            })
            .to_string(),
        )
        .unwrap();

        let mut incoming = Config::default();
        let report = import(
            &archive_path,
            &mut incoming,
            &targets_for(target.path()),
            false,
        )
        .unwrap();
        assert_eq!(report.lockfile_entries, 1);

        let text = std::fs::read_to_string(target.path().join("installed.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        let skill_names: Vec<&str> = value["skills"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert_eq!(skill_names, vec!["demo"]);
        let mcp_names: Vec<&str> = value["mcp_servers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert_eq!(mcp_names, vec!["local-mcp"], "MCP 条目不该被备份带过来");
    }

    /// 不是 wyj-code 备份的 zip（比如技能包）要报明确错误，而不是解压出乱码。
    #[test]
    fn inspect_rejects_archives_without_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("skill.zip");
        {
            let mut buf = Vec::new();
            {
                let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
                let options: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
                writer.start_file("SKILL.md", options).unwrap();
                writer.write_all(b"body").unwrap();
                writer.finish().unwrap();
            }
            std::fs::write(&path, &buf).unwrap();
        }
        let err = inspect(&path).unwrap_err().to_string();
        assert!(err.contains(MANIFEST_NAME), "unexpected: {err}");
    }

    /// 版本号不认识时必须拒绝：按老规则解析新格式会静默丢字段。
    #[test]
    fn inspect_rejects_unknown_format_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("future.wyj");
        {
            let mut buf = Vec::new();
            {
                let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
                let options: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
                writer.start_file(MANIFEST_NAME, options).unwrap();
                writer
                    .write_all(
                        serde_json::json!({
                            "format": 999,
                            "app_version": "9.9.9",
                            "exported_at": "2026-01-01T00:00:00Z",
                            "contains_secrets": false
                        })
                        .to_string()
                        .as_bytes(),
                    )
                    .unwrap();
                writer.finish().unwrap();
            }
            std::fs::write(&path, &buf).unwrap();
        }
        let err = inspect(&path).unwrap_err().to_string();
        assert!(err.contains("999"), "unexpected: {err}");
    }

    /// 导出默认拒绝覆盖已有文件——上一份备份不该被一次手滑抹掉。
    /// 该保护在 CLI 层，数据层只管写，因此这里验证数据层的原子写本身可用。
    #[test]
    fn export_writes_the_archive_atomically_and_tightens_permissions() {
        let home = make_home(&[("skills/a/SKILL.md", "x")]);
        let config = config_with(&[("w", "k")], "w");
        let dest = export_to(home.path(), &config);
        assert!(dest.is_file());
        // 临时文件不该残留
        let leftovers: Vec<_> = std::fs::read_dir(home.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("wyj-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "不应残留临时文件");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "备份含明文密钥，必须仅属主可读写");
        }
    }

    /// manifest 要如实记录包内容与是否含密钥，供导入前的预览与安全提示。
    #[test]
    fn manifest_records_contents_and_secret_presence() {
        let home = make_home(&[("skills/a/SKILL.md", "x")]);
        let mut config = config_with(&[("with-key", "sk-1"), ("no-key", "")], "with-key");
        config.profiles[1].api_key = None;
        let dest = home.path().join("m.wyj");
        let report = export(&config, home.path(), &dest, "1.2.3", "2026-10-24T00:00:00Z").unwrap();

        assert_eq!(report.manifest.format, BUNDLE_FORMAT);
        assert!(report.manifest.contains_secrets);
        assert_eq!(report.profiles_with_keys, vec!["with-key".to_string()]);
        assert_eq!(
            report.manifest.profiles,
            vec!["with-key".to_string(), "no-key".to_string()]
        );
        assert_eq!(report.manifest.skills, vec!["a".to_string()]);

        // inspect 读回来必须是同一份内容
        let manifest = inspect(&dest).unwrap();
        assert_eq!(manifest.app_version, "1.2.3");
        assert!(manifest.contains_secrets);
    }

    /// 根目录下的非 skill 文件既不算 skill、也不进归档：导入侧从不还原它们，
    /// 带进包里只会在 manifest 里列出一个假技能名。
    #[test]
    fn non_skill_root_files_are_neither_exported_nor_listed() {
        let home = make_home(&[
            ("skills/.skills_store_lock.json", "{}"),
            ("skills/.DS_Store", "junk"),
            ("skills/demo/SKILL.md", "body"),
        ]);
        let config = config_with(&[("w", "k")], "w");
        let dest = home.path().join("x.wyj");
        let report = export(&config, home.path(), &dest, "0.3.20", "now").unwrap();

        assert_eq!(report.manifest.skills, vec!["demo".to_string()]);

        let bytes = std::fs::read(&dest).unwrap();
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let names = archive::list_entries(&mut zip).unwrap();
        assert!(
            !names.iter().any(|n| n.contains(".skills_store_lock")),
            "安装锁不该进归档: {names:?}"
        );
    }

    /// 没有 skills 目录时导出照样成功（全新机器上什么都没有很正常）。
    #[test]
    fn export_tolerates_missing_skills_dir() {
        let home = tempfile::tempdir().unwrap();
        let config = config_with(&[("w", "k")], "w");
        let dest = home.path().join("empty.wyj");
        let report = export(&config, home.path(), &dest, "0.3.20", "now").unwrap();
        assert!(report.manifest.skills.is_empty());
        assert!(dest.is_file());
    }
}
