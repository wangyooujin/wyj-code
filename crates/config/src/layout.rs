//! 落盘布局：`~/.wyj-code` 下每个目录名的**唯一**来源。
//!
//! # 为什么要有这个模块
//!
//! 在此之前，`"sessions"` / `"memory-v3"` / `"projects"` 这类字面量散落在
//! 20 多个文件里（共 140 余处 `join("…")`），没有一处集中定义。后果不是难看，
//! 而是**任何布局调整都变成逐文件考古**：把会话改成按项目分目录时，漏掉一个
//! `join("sessions")` 就会让某个子系统继续读写旧位置，表现为「数据莫名消失」
//! 而没有任何报错。
//!
//! 因此这里的约定是：**新增落盘位置一律先在此登记**，调用方只拿函数不写字符串。
//!
//! # 数据根可重定向
//!
//! [`data_root`] 优先读 `WYJ_CODE_HOME`，否则回退 `<home>/.wyj-code`。这条覆盖
//! 对测试与「把状态放到外置盘」两种场景都需要；此前没有任何改路径的入口，
//! 用户只能改源码或做符号链接。
//!
//! # 布局总览
//!
//! ```text
//! <root>/
//! ├── config.toml  settings.json  AGENTS.md  installed.json
//! ├── skills/  agents/  plugins/  marketplaces/
//! ├── sessions/<project_key>/<sid>.json[.meta.json|.json.lock|.json.owner]
//! │   └── <sid>.checkpoints/  <sid>.subagents/
//! ├── history/<project_key>/history.jsonl
//! ├── memory-v3/{global,projects/<key>}/
//! ├── evolution/<project_key>/
//! ├── indexes/<project_key>/
//! ├── workspaces/<project_key>/  workflows/<project_key>/
//! ├── projects/<project_key>/{allowed_tools,mcp_trust,skill_trust,hooks_trust}.json
//! │   └── instances/  meta.json
//! ├── cas/  spill/  logs/  model-capabilities/
//! ```
//!
//! `sessions` / `history` 从「全局平铺」改成「按项目分目录」是 v0.3.18 的改动，
//! 见 `crates/cli/src/storage_cmd.rs` 的 `migrate` 子命令。
//!
//! `storage.scope = "project"` 时，上表的 `sessions` / `history` 以及
//! [`ProjectStateRoots`] 里的三组（`memory-v3/projects`、`evolution`、
//! `indexes`）改落到 `<git-root>/.wyj-code/local/`，跟着工作副本走。
//! **`memory-v3/global/`、`projects/<key>/`（信任门控）、`cas/`、
//! `workspaces/`、`workflows/` 与各类全局运行时残留（`spill` / `logs` /
//! `model-capabilities`）不跟随**：前两者放回仓库等于让被信任的
//! 仓库自己改掉批准标记，`cas/` 是跨项目去重池（`gc` 是全局 LRU），
//! `workspaces/` 与 `workflows/` 描述的是本机运行态（worktree 路径、进程 pid），
//! 跨机器复制没有意义。

use std::path::{Path, PathBuf};

/// 覆盖数据根的环境变量。
pub const HOME_ENV: &str = "WYJ_CODE_HOME";

/// 默认数据根目录名（`<home>/.wyj-code`）。
pub const DEFAULT_DIR_NAME: &str = ".wyj-code";

/// 数据根：`WYJ_CODE_HOME` 优先，否则 `<home>/.wyj-code`。**不创建目录**。
///
/// 与 [`crate::global_config_dir`] 的区别：那个函数会解析真实主目录，本函数
/// 只做「环境变量 or 默认名」的判定，供需要注入 home 的测试路径复用。
pub fn data_root_in(home: &Path) -> PathBuf {
    env_override().unwrap_or_else(|| home.join(DEFAULT_DIR_NAME))
}

/// 数据根（会解析真实主目录），**不创建目录**。
pub fn data_root() -> crate::Result<PathBuf> {
    Ok(env_override().unwrap_or_else(|| {
        crate::home_dir()
            .map(|home| home.join(DEFAULT_DIR_NAME))
            .unwrap_or_else(|_| PathBuf::from(DEFAULT_DIR_NAME))
    }))
}

/// 给定 home 的数据根，**仅当该 home 就是本进程的真实主目录时**才接受
/// `WYJ_CODE_HOME` 覆盖。
///
/// 为什么加这个限制：`global_config_dir_in(home)` 是测试的隔离注入点——测试
/// 传一个 tempdir，期望所有落盘都在里面。若无条件让 `WYJ_CODE_HOME` 优先，
/// 开发机上只要设了该变量，`cargo test` 就会写进真实数据目录，测试之间还会
/// 互相踩。生产路径传的正是 `home_dir()`，所以覆盖照常生效。
pub fn data_root_for_home(home: &Path) -> PathBuf {
    if let Some(dir) = env_override() {
        if crate::home_dir().map(|real| real == home).unwrap_or(false) {
            return dir;
        }
    }
    home.join(DEFAULT_DIR_NAME)
}

fn env_override() -> Option<PathBuf> {
    match std::env::var_os(HOME_ENV) {
        Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
        _ => None,
    }
}

macro_rules! subdir {
    ($($fn_name:ident => $name:literal,)*) => {
        $(
            #[doc = concat!("`<root>/", $name, "`（不创建）。")]
            pub fn $fn_name(root: &Path) -> PathBuf {
                root.join($name)
            }
        )*
    };
}

subdir! {
    sessions_root => "sessions",
    history_root => "history",
    projects_root => "projects",
    memory_v3_root => "memory-v3",
    memory_root => "memory",
    evolution_root => "evolution",
    indexes_root => "indexes",
    workspaces_root => "workspaces",
    workflows_root => "workflows",
    cas_root => "cas",
    spill_root => "spill",
    logs_root => "logs",
    plugins_root => "plugins",
    marketplaces_root => "marketplaces",
    model_capabilities_root => "model-capabilities",
    skills_root => "skills",
    agents_root => "agents",
}

/// 项目级运行状态目录：`<root>/<kind>/<project_key>`。
///
/// `kind` 用 [`sessions_root`] 等的**目录名**（不是绝对路径）。之所以要这一层
/// 间接，是因为调用方手里往往同时有 base 与 kind，传路径反而更啰嗦。
pub fn project_state_dir(root: &Path, kind: &str, project_key: &str) -> PathBuf {
    root.join(kind).join(project_key)
}

/// 会话文件所在目录：`<root>/sessions/<project_key>`。
pub fn project_sessions_dir(root: &Path, project_key: &str) -> PathBuf {
    sessions_root(root).join(project_key)
}

/// 历史文件所在目录：`<root>/history/<project_key>`。
pub fn project_history_dir(root: &Path, project_key: &str) -> PathBuf {
    history_root(root).join(project_key)
}

/// 项目信任/授权记录目录：`<root>/projects/<project_key>`。
///
/// **必须在仓库内容控制不到的位置**：这些文件记录「用户批准过什么」，
/// 若放进 `<git-root>/.wyj-code/`，被信任的仓库自己就能改掉批准标记，
/// 门控形同虚设（见 `crates/store/src/project_trust.rs`）。
pub fn project_trust_dir(root: &Path, project_key: &str) -> PathBuf {
    projects_root(root).join(project_key)
}

/// 项目元信息文件：`<root>/projects/<project_key>/meta.json`。
///
/// 记录 `root` 绝对路径。没有它，磁盘上只有 `wyj-code-c69afe02` 这种哈希名，
/// 用户既认不出是哪个仓库，迁移/清理也无从判断归属。
pub fn project_meta_path(root: &Path, project_key: &str) -> PathBuf {
    project_trust_dir(root, project_key).join("meta.json")
}

/// 项目级 skill 的运行产物目录：`<root>/projects/<project_key>/skill-data`。
///
/// 与 [`project_trust_dir`] 同侧、**在仓库内容控制不到的位置**。skill 的指纹覆盖
/// `<git-root>/.wyj-code/skills/` 整棵树，任何写回那棵树的产物都会让刚批准的指纹
/// 失效；把可写数据放这里，既不动指纹也不会被下一次 `git pull` 覆盖。
pub fn project_skill_data_root(root: &Path, project_key: &str) -> PathBuf {
    project_trust_dir(root, project_key).join("skill-data")
}

/// 运行状态的落盘范围。
///
/// 默认 [`StorageScope::Home`]：状态集中在 `<data_root>/` 下按项目分目录。
/// [`StorageScope::Project`] 把**会话与历史**改放到 `<git-root>/.wyj-code/local/`，
/// 让状态跟着工作副本走（换机器、换 clone 就带走）。
///
/// # 为什么不是默认
///
/// 会话 transcript 里是工具输出原文（`.env` 内容、命令回显、密钥）。放进仓库
/// 目录后，只要用户忘了 gitignore 一次 `git add -A`，泄漏就是永久的。因此
/// 选 `project` 时会**自动写 `<git-root>/.wyj-code/local/.gitignore`**（内容 `*`），
/// 并额外维护仓库根的 `.gitignore` 条目作为第二道保险。
///
/// 另外两个代价：多 worktree 时每个 worktree 一份独立会话（`-c` 行为会变），
/// 以及会话体积会跟着 `git status` 的 stat 成本走。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StorageScope {
    /// `<data_root>/sessions/<key>/`（默认）。
    #[default]
    Home,
    /// `<git-root>/.wyj-code/local/sessions/`。
    Project,
}

impl StorageScope {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "home" | "global" | "" => Some(Self::Home),
            "project" | "local" => Some(Self::Project),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Home => "home",
            Self::Project => "project",
        }
    }
}

/// 项目内运行状态目录：`<git-root>/.wyj-code/local/`。
///
/// 只在 [`StorageScope::Project`] 下使用。这个目录必须整体被 git 忽略，
/// 见 [`ensure_project_local_gitignored`]。
pub fn project_local_dir(cwd: &Path) -> PathBuf {
    crate::project_config_dir(cwd).join("local")
}

/// 保证 `<git-root>/.wyj-code/local/` 不会被提交。
///
/// 两道保险，缺一不可：
/// 1. 目录内自带 `.gitignore`（内容 `*`）——只保护这个目录，不污染用户的
///    仓库根 `.gitignore`，且 `git add -A` 也不会带进去。
/// 2. 仓库根 `.gitignore` 追加 `.wyj-code/local/`——防的是「用户把 local/
///    整个删掉后又重建」时第 1 条还没来得及写。
///
/// 幂等：已存在/已有该条目就什么都不做。写失败只 warn——忽略规则写不上
/// 不该让会话保存失败，但必须让用户看见。
pub fn ensure_project_local_gitignored(cwd: &Path) {
    let local_dir = project_local_dir(cwd);
    if let Err(error) = std::fs::create_dir_all(&local_dir) {
        tracing::warn!("创建项目内状态目录失败 {}: {error}", local_dir.display());
        return;
    }
    let inner = local_dir.join(".gitignore");
    if !inner.exists() {
        if let Err(error) = std::fs::write(&inner, "*\n") {
            tracing::warn!("写入 {} 失败: {error}", inner.display());
        }
    }
    // 仓库根 .gitignore 的第二道保险
    let root_ignore = crate::project_root(cwd).join(".gitignore");
    let entry = ".wyj-code/local/";
    let existing = std::fs::read_to_string(&root_ignore).unwrap_or_default();
    if !existing.lines().any(|line| line.trim() == entry) {
        let mut text = existing;
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(entry);
        text.push('\n');
        if let Err(error) = std::fs::write(&root_ignore, text) {
            tracing::warn!("追加 {} 失败: {error}", root_ignore.display());
        }
    }
}

/// 项目级状态根，**不含** `<project_key>` 那一层。
///
/// 只收「跟着工作副本走有意义」的三类：
///
/// * `memory_projects` —— 模型学到的项目事实；
/// * `evolution` —— 演化情景与候选；
/// * `indexes` —— 代码索引（派生缓存，重建无副作用，放项目内只是省一次全量重建）。
///
/// **刻意不收**的两类：`workflows/` 里是本机某进程正在跑的 workflow 实时快照
/// （含 pid），跨机器复制无意义，与 `workspaces/` 的 worktree 同性质；
/// `memory-v3/global` 是跨项目共用的单文件，跟着某个项目走等于把它私有化。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectStateRoots {
    pub memory_projects: PathBuf,
    pub evolution: PathBuf,
    pub indexes: PathBuf,
}

impl ProjectStateRoots {
    /// 按 scope 给出三组根目录，**不创建目录、不写 gitignore**。
    pub fn for_scope(scope: StorageScope, config_base: &Path, cwd: &Path) -> Self {
        match scope {
            StorageScope::Home => Self {
                memory_projects: memory_v3_root(config_base).join("projects"),
                evolution: evolution_root(config_base),
                indexes: indexes_root(config_base),
            },
            StorageScope::Project => {
                // 项目内保持与 home 相同的子目录名，只是把根换成 `local/`。
                // 记忆那层仍留 `memory-v3/projects`：`clear_all` 用相对路径
                // 记录被搬走的文件，形状一致能少一处特判。
                let local = project_local_dir(cwd);
                Self {
                    memory_projects: local.join("memory-v3").join("projects"),
                    evolution: local.join("evolution"),
                    indexes: local.join("indexes"),
                }
            }
        }
    }
}

/// 解析 `storage.scope`：未知值**降级为 home 并告警**，绝不让启动失败。
fn parse_scope_or_warn(scope_value: &str) -> StorageScope {
    match StorageScope::parse(scope_value) {
        Some(scope) => scope,
        None => {
            tracing::warn!(
                "storage.scope = {scope_value:?} 无法识别（只支持 \"home\" / \"project\"），按 \"home\" 处理"
            );
            StorageScope::Home
        }
    }
}

/// 解析 `storage.scope` 并给出 `(sessions_root, history_root)`。
///
/// 把这段判断集中在这里，是因为它有两处非显然的副作用：`project` 分支会
/// 创建目录并写 gitignore，而未知值必须**降级为 home 并告警**而不是让启动失败。
/// CLI 只负责调用，不再自己拼路径——否则测试只能靠「启动一个真进程」覆盖。
///
/// 返回 `(sessions_root, history_root, 实际生效的 scope)`。
pub fn resolve_runtime_roots(
    scope_value: &str,
    config_base: &Path,
    cwd: &Path,
) -> (PathBuf, PathBuf, StorageScope) {
    let scope = parse_scope_or_warn(scope_value);
    match scope {
        StorageScope::Home => (sessions_root(config_base), history_root(config_base), scope),
        StorageScope::Project => {
            ensure_project_local_gitignored(cwd);
            let local = project_local_dir(cwd);
            (local.join("sessions"), local.join("history"), scope)
        }
    }
}

/// 解析 `storage.scope` 并给出 [`ProjectStateRoots`]（含实际生效的 scope）。
///
/// 与 [`resolve_runtime_roots`] 同样在 `project` 分支确保 `local/` 被 git 忽略：
/// `memory` / `evolve` / `workflow` 这些子命令不会经过启动期的
/// `resolve_runtime_roots`，只调本函数也必须拿到同样的保护。
pub fn resolve_project_state_roots(
    scope_value: &str,
    config_base: &Path,
    cwd: &Path,
) -> (ProjectStateRoots, StorageScope) {
    let scope = parse_scope_or_warn(scope_value);
    if scope == StorageScope::Project {
        ensure_project_local_gitignored(cwd);
    }
    (ProjectStateRoots::for_scope(scope, config_base, cwd), scope)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_root_prefers_env_override() {
        let home = Path::new("/home/someone");
        // 用 `set_var` 会在并行测试间互相污染，这里只断言回退分支，
        // 覆盖分支由 `data_root_in` 的纯函数性质保证。
        if std::env::var_os(HOME_ENV).is_none() {
            assert_eq!(data_root_in(home), home.join(".wyj-code"));
        }
    }

    #[test]
    fn project_scoped_paths_nest_under_their_kind() {
        let root = Path::new("/data");
        assert_eq!(
            project_sessions_dir(root, "wyj-code-c69afe02"),
            Path::new("/data/sessions/wyj-code-c69afe02")
        );
        assert_eq!(project_history_dir(root, "k"), Path::new("/data/history/k"));
        assert_eq!(project_trust_dir(root, "k"), Path::new("/data/projects/k"));
        assert_eq!(
            project_meta_path(root, "k"),
            Path::new("/data/projects/k/meta.json")
        );
    }

    #[test]
    fn subdir_helpers_are_relative_to_root() {
        let root = Path::new("/data");
        assert_eq!(sessions_root(root), root.join("sessions"));
        assert_eq!(memory_v3_root(root), root.join("memory-v3"));
        assert_eq!(cas_root(root), root.join("cas"));
    }

    /// `data_root_for_home` 必须**只在传真实主目录时**接受 `WYJ_CODE_HOME`。
    ///
    /// 这条钉的是测试隔离：`global_config_dir_in(tempdir)` 是测试的注入点，
    /// 开发机上只要设了 `WYJ_CODE_HOME`，无条件优先就会让 `cargo test`
    /// 写进真实数据目录，测试之间互相踩。
    #[test]
    fn env_override_only_applies_to_the_real_home() {
        let temp = Path::new("/tmp/definitely-not-the-real-home");
        // 传 tempdir → 无论环境变量如何都必须留在 tempdir 下
        assert_eq!(data_root_for_home(temp), temp.join(DEFAULT_DIR_NAME));

        // 传真实主目录 → 与 `data_root_in` 的判定一致（设了变量就用变量）
        let real_home = crate::home_dir().unwrap();
        assert_eq!(data_root_for_home(&real_home), data_root_in(&real_home));
    }

    #[test]
    fn storage_scope_parses_known_values_and_rejects_typos() {
        assert_eq!(StorageScope::parse("home"), Some(StorageScope::Home));
        assert_eq!(StorageScope::parse(""), Some(StorageScope::Home));
        assert_eq!(StorageScope::parse("PROJECT"), Some(StorageScope::Project));
        assert_eq!(StorageScope::parse(" local "), Some(StorageScope::Project));
        // 拼错必须是 `None`（调用方据此告警并降级），不能静默当成 home
        assert_eq!(StorageScope::parse("projcet"), None);
        assert_eq!(StorageScope::default(), StorageScope::Home);
    }

    /// 选 `project` 落盘时，`local/` 必须自带 gitignore **且**仓库根也有一条
    /// ——少任何一道，一次 `git add -A` 就会把 transcript 提交上去。
    #[test]
    fn project_scope_creates_both_gitignore_layers_idempotently() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".git")).unwrap();

        ensure_project_local_gitignored(repo.path());
        let local = project_local_dir(repo.path());
        assert!(local.is_dir());
        assert_eq!(
            std::fs::read_to_string(local.join(".gitignore")).unwrap(),
            "*\n"
        );
        let root_ignore = repo.path().join(".gitignore");
        assert!(std::fs::read_to_string(&root_ignore)
            .unwrap()
            .contains(".wyj-code/local/"));

        // 幂等：再跑一次不该重复追加
        ensure_project_local_gitignored(repo.path());
        let text = std::fs::read_to_string(&root_ignore).unwrap();
        assert_eq!(text.matches(".wyj-code/local/").count(), 1);
    }

    /// 已有内容的 `.gitignore` 不能被整体覆盖，只在末尾追加。
    #[test]
    fn project_scope_appends_to_existing_gitignore_without_clobbering() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".git")).unwrap();
        let root_ignore = repo.path().join(".gitignore");
        std::fs::write(&root_ignore, "target/\nnode_modules/").unwrap();

        ensure_project_local_gitignored(repo.path());
        let text = std::fs::read_to_string(&root_ignore).unwrap();
        assert!(
            text.starts_with("target/\nnode_modules/\n"),
            "原有内容必须保留"
        );
        assert!(text.contains(".wyj-code/local/"));
    }

    /// `resolve_runtime_roots` 是 CLI 唯一的路径决策点，两条分支都要对。
    #[test]
    fn resolve_runtime_roots_switches_layout_by_scope() {
        let repo = tempfile::tempdir().unwrap();
        let config_base = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".git")).unwrap();

        let (sessions, history, scope) =
            resolve_runtime_roots("home", config_base.path(), repo.path());
        assert_eq!(scope, StorageScope::Home);
        assert_eq!(sessions, config_base.path().join("sessions"));
        assert_eq!(history, config_base.path().join("history"));

        let (sessions, history, scope) =
            resolve_runtime_roots("project", config_base.path(), repo.path());
        assert_eq!(scope, StorageScope::Project);
        // `project_config_dir` 内部走 `project_root`，后者会 canonicalize——
        // macOS 上 `/var` 是 `/private/var` 的符号链接，必须同样规范化后再比。
        let local = repo
            .path()
            .canonicalize()
            .unwrap()
            .join(".wyj-code")
            .join("local");
        assert_eq!(sessions, local.join("sessions"));
        assert_eq!(history, local.join("history"));
        // 选 project 必须顺带把 gitignore 写好
        assert!(local.join(".gitignore").is_file());

        // 拼错降级为 home，而不是 panic 或返回 project 路径
        let (sessions, _, scope) =
            resolve_runtime_roots("projcet", config_base.path(), repo.path());
        assert_eq!(scope, StorageScope::Home);
        assert_eq!(sessions, config_base.path().join("sessions"));
    }

    /// 三组项目状态根同样随 scope 切换，且 `project` 分支自动带 gitignore。
    ///
    /// 这条钉的是「子命令也要跟着走」：`memory` / `evolve` 不经过启动期的
    /// `resolve_runtime_roots`，只调本函数。若它不写 gitignore，从 CLI 子命令
    /// 首次创建的 `local/` 目录就会缺保护。
    #[test]
    fn resolve_project_state_roots_follows_scope() {
        let repo = tempfile::tempdir().unwrap();
        let config_base = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".git")).unwrap();

        let (home, scope) = resolve_project_state_roots("home", config_base.path(), repo.path());
        assert_eq!(scope, StorageScope::Home);
        assert_eq!(
            home.memory_projects,
            config_base.path().join("memory-v3/projects")
        );
        assert_eq!(home.evolution, config_base.path().join("evolution"));
        assert_eq!(home.indexes, config_base.path().join("indexes"));

        let (project, scope) =
            resolve_project_state_roots("project", config_base.path(), repo.path());
        assert_eq!(scope, StorageScope::Project);
        let local = repo
            .path()
            .canonicalize()
            .unwrap()
            .join(".wyj-code")
            .join("local");
        assert_eq!(
            project.memory_projects,
            local.join("memory-v3").join("projects")
        );
        assert_eq!(project.evolution, local.join("evolution"));
        assert_eq!(project.indexes, local.join("indexes"));
        // 只解析路径 + 写 gitignore，不该顺手把业务目录建出来
        assert!(local.join(".gitignore").is_file());
        assert!(!project.evolution.exists());

        // 拼错同样降级为 home
        let (fallback, scope) =
            resolve_project_state_roots("projcet", config_base.path(), repo.path());
        assert_eq!(scope, StorageScope::Home);
        assert_eq!(fallback.evolution, config_base.path().join("evolution"));
    }
}
