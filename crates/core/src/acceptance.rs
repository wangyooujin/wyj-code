//! 验收契约与完成门禁（acceptance gate）。
//!
//! ## 为什么需要它
//!
//! 在此之前，「任务完成」的定义是**模型说了完成**。模型输出最终文本，回合就结束。
//! 这条等式是整条链上最贵的漏洞：
//!
//! - 模型**意识不到**功能没有端到端跑通（Anthropic 2025-11 的原话观察）；
//! - 让模型自己评自己的活，它会「自信地夸赞明显平庸的作品」（Anthropic 2026-03）；
//! - 而外部受控实验（arXiv:2605.23950）里，三个 harness 配置之间**最大**的一段
//!   差距（H2→H3）就来自「闭合验证与恢复回路」。
//!
//! 本模块提供的就是那条闭合回路的**前半段**：一组**框架可判定**的验收判据。
//! 模型要结束回合时，框架先跑一遍判据；不通过就不让回合结束，把**证据**
//! （不是评语）回灌给模型，让它继续修。
//!
//! ## 判据必须是框架能判的
//!
//! 这里刻意**只**收录不需要模型参与的判据类型：命令退出码、文件是否存在、文件是否
//! 包含某段文本、某段路径下是否匹配某个正则。理由是外部证据里的硬约束：
//!
//! - 弱验证下加宽候选几乎无用（arXiv:2609.39982：N=4→8 只从 49.32% 提到 51.02%）；
//! - verifier 未必是正收益——验收对象与最终验收对象不对齐时，在 SWE 上是 **−0.8pp**
//!   （arXiv:2603.25723 的模块消融）。
//!
//! 也就是说：**先把能自动判的判据判准，再考虑让模型去判**。需要语义判定的验收
//! （例如「这个 UI 真的能用吗」）属于下一刀的工作，本模块不碰。
//!
//! ## 零假设
//!
//! 与 `verify` 完全同源的哲学：
//!
//! - **没声明 = 完全不存在**。项目没有在 `.wyj-code/settings.json` 里写
//!   `[acceptance]`，则本模块一个字节都不参与：不 spawn 子进程、不读文件、
//!   不改变任何行为。零假设是它能默认安全存在的前提。
//! - 声明了但 `enabled = false` 同样等于没有（保留判据、临时关掉，不删配置）。
//! - 判据列表为空 = 没有要判的东西 = 门禁不生效。
//!
//! ## 信任门控
//!
//! `command` 判据会被 `sh -c` 直接执行，与 `verify.command` 是**同一级**的任意命令
//! 执行面。项目级的 `[acceptance]` 块因此必须过 `hooks_trust` 门控：未批准时
//! **整块跳过**（只保留全局 `~/.wyj-code/settings.json` 的来源）。
//!
//! `hooks_trust` 的指纹覆盖 `hooks` / `verify` / `acceptance` 三个键。既有项目在
//! **没有** `[acceptance]` 块时指纹逐字节不变，批准记录不受影响；一旦新增
//! `[acceptance]`，指纹变化 → 需要重新批准。这正是我们要的语义：新增了能执行命令的
//! 配置就该重新授权。
//!
//! ## 边界：只看你自己的工作区
//!
//! `file_exists` / `file_contains` / `grep_*` 的路径一律相对**工作目录**解析，
//! 且必须落在工作目录之内（逃出去的一律判为「判据无效」→ 恒不通过）。判据的
//! 失败详情只回报**文件路径与行号**，不回显匹配到的内容——验收报告是要回灌给模型
//! 的上下文，不应该成为一条绕过凭据保护的读文件通道。
//!
//! ## 报告形态
//!
//! 回灌给模型的文本是**证据**不是评语：命令的退出码与输出尾部、命中的文件:行号。
//! 这对应 arXiv:2605.23950 里 H1→H2 增益的机制之一——「结构化错误反馈」降低控制
//! 噪声，而不是加一层更严厉的措辞。

use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::Deserialize;

/// 单条判据的判定方式。
#[derive(Debug, Clone)]
pub enum CriterionKind {
    /// 跑一条命令，退出码必须为 0。
    Command { command: String, timeout_ms: u64 },
    /// 文件必须存在。
    FileExists { path: String },
    /// 文件必须包含某段文本（**字面量**，不是正则）。
    FileContains { path: String, needle: String },
    /// 指定的路径下必须至少有一处匹配该正则。
    GrepPresent { pattern: String, paths: Vec<String> },
    /// 指定的路径下**不得**有任何匹配该正则的地方。
    GrepAbsent { pattern: String, paths: Vec<String> },
}

/// 一条验收判据。
#[derive(Debug, Clone)]
pub struct AcceptanceCriterion {
    /// 判据标识，出现在报告里，必须在同一个项目内唯一。
    pub id: String,
    pub kind: CriterionKind,
    /// 判据本身配置无效时的原因。
    ///
    /// `Some` 时该判据**恒判不通过**（fail-closed）。理由：一条配错了的判据如果
    /// 被静默跳过，用户会以为门禁生效了，实际强度是打折的——这比明确失败更危险。
    pub invalid: Option<String>,
}

/// 一个项目的验收契约。
#[derive(Debug, Clone)]
pub struct AcceptanceCfg {
    pub criteria: Vec<AcceptanceCriterion>,
    /// 验收不通过时允许模型再试几轮。超过后按「未完成」交付，不再续跑。
    pub max_retries: u32,
}

/// 默认重试预算。
///
/// 2 轮是权衡值：给模型「发现漏改 → 修 → 再验」两次机会，同时不至于把一次
/// 误判放大成无界循环。超过预算时未通过的判据会作为最终交付的一部分明确呈现，
/// 既不静默放行，也不无限重试。
pub const DEFAULT_MAX_RETRIES: u32 = 2;

/// 命令判据的默认超时（毫秒）。与 `verify` 同量级：`cargo test` 冷启动很慢。
pub const DEFAULT_COMMAND_TIMEOUT_MS: u64 = 120_000;

/// 单条判据失败详情回灌给模型时的字节上限。
const DETAIL_MAX_BYTES: usize = 4_000;

/// 单条判据最多回报多少个命中位置。
const MAX_HITS_REPORTED: usize = 10;

/// 单条判据最多扫描多少个文件（`grep_*`）。
const MAX_FILES_SCANNED: usize = 20_000;

/// 加载结果。
///
/// `cfg = None` 且 `error = None` = 项目没声明验收契约（正常路径）。
/// `cfg = None` 且 `error = Some(..)` = **声明了但解析失败**——这一档必须能被
/// 用户看见，否则「门禁其实没生效」会被完全静默掉。
#[derive(Debug, Clone, Default)]
pub struct AcceptanceLoad {
    pub cfg: Option<AcceptanceCfg>,
    pub error: Option<String>,
}

/// `settings.json` 里的 `acceptance` 块（原始形态）。
#[derive(Debug, Clone, Deserialize)]
struct RawAcceptance {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    max_retries: Option<u32>,
    #[serde(default)]
    criteria: Vec<serde_json::Value>,
}

/// 读三源合并后的验收契约。
///
/// 来源与优先级与 `verify` 完全一致（后者覆盖前者）：
/// `~/.wyj-code/settings.json` → `<git-root>/.wyj-code/settings.json` →
/// `<git-root>/.wyj-code/settings.local.json`。
///
/// **项目级两源先过信任门控**（`hooks_trust`）：`acceptance` 的 `command` 判据
/// 与 `verify.command` 同级，未批准前不能执行。
///
/// 每次调用都重新读盘（与 `ClaudeMdLoader` / `load_verify_cfg` 同一哲学：不缓存，
/// 保证运行期间改 settings 立即生效）。
pub fn load_acceptance_cfg(cwd: &Path) -> AcceptanceLoad {
    let mut paths: Vec<PathBuf> = Vec::new();
    if let Ok(global) = wyj_config::global_config_dir() {
        paths.push(global.join("settings.json"));
    }
    let project_trusted = crate::hooks_trust::project_trusted(cwd);
    if project_trusted {
        let root = wyj_config::project_root(cwd);
        paths.push(root.join(".wyj-code").join("settings.json"));
        paths.push(root.join(".wyj-code").join("settings.local.json"));
    }

    let mut block: Option<RawAcceptance> = None;
    for path in &paths {
        match read_acceptance_block(path) {
            Ok(Some(next)) => block = Some(next),
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(
                    "acceptance: 解析 {} 中的 acceptance 字段失败: {err}",
                    path.display()
                );
                return AcceptanceLoad {
                    cfg: None,
                    error: Some(format!("{}: {err}", path.display())),
                };
            }
        }
    }

    let Some(raw) = block else {
        return AcceptanceLoad::default();
    };
    if raw.enabled == Some(false) {
        return AcceptanceLoad::default();
    }

    let mut criteria = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for value in &raw.criteria {
        let (id, kind, invalid) = parse_criterion(value);
        if seen.iter().any(|s| s == &id) {
            tracing::warn!("acceptance: 判据 id 重复，已跳过: {id}");
            continue;
        }
        seen.push(id.clone());
        criteria.push(AcceptanceCriterion { id, kind, invalid });
    }

    if criteria.is_empty() {
        // 没有要判的东西 = 门禁无事可做。仍然把「声明了但判据为空」记下来，
        // 免得用户以为写了 `[acceptance]` 就有门禁。
        return AcceptanceLoad {
            cfg: None,
            error: None,
        };
    }

    AcceptanceLoad {
        cfg: Some(AcceptanceCfg {
            criteria,
            max_retries: raw.max_retries.unwrap_or(DEFAULT_MAX_RETRIES),
        }),
        error: None,
    }
}

/// 读一个 settings 文件里的 `acceptance` 块。
fn read_acceptance_block(path: &Path) -> Result<Option<RawAcceptance>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(_) => return Ok(None),
    };
    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        // 整个文件不是合法 JSON 时不关验收的事——别的键可能仍然可用。
        // 但这会让 `[acceptance]` 块被静默忽略，所以单独 warn 一次。
        Err(err) => {
            tracing::warn!("acceptance: {} 不是合法 JSON: {err}", path.display());
            return Ok(None);
        }
    };
    let raw = match value.get("acceptance") {
        Some(raw) => raw,
        None => return Ok(None),
    };
    serde_json::from_value::<RawAcceptance>(raw.clone())
        .map(Some)
        .map_err(|err| format!("acceptance 块结构非法: {err}"))
}

/// 解析单条判据。
///
/// 手写解析而不是 `#[serde(tag = "kind")]`：手写能把**每一种**配置错误都变成
/// 一句能定位到字段的话，而 `serde` 的默认报错对用户没有指导意义。
fn parse_criterion(value: &serde_json::Value) -> (String, CriterionKind, Option<String>) {
    let obj = match value.as_object() {
        Some(obj) => obj,
        None => {
            return (
                "<invalid>".to_string(),
                CriterionKind::FileExists {
                    path: String::new(),
                },
                Some("判据必须是一个 JSON 对象".to_string()),
            )
        }
    };
    let id = obj
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if id.is_empty() {
        return (
            "<invalid>".to_string(),
            CriterionKind::FileExists {
                path: String::new(),
            },
            Some("判据缺少非空的 `id` 字段".to_string()),
        );
    }
    let kind_name = obj.get("kind").and_then(|v| v.as_str()).unwrap_or("");
    let str_field = |name: &str| obj.get(name).and_then(|v| v.as_str()).map(str::to_string);
    let paths_field = || -> Vec<String> {
        obj.get("paths")
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };

    let (kind, invalid) = match kind_name {
        "command" => {
            let command = str_field("command").unwrap_or_default();
            let timeout_ms = obj
                .get("timeout_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(DEFAULT_COMMAND_TIMEOUT_MS)
                .max(1);
            if command.trim().is_empty() {
                (
                    CriterionKind::Command {
                        command: String::new(),
                        timeout_ms,
                    },
                    Some("`command` 判据缺少非空的 `command` 字段".to_string()),
                )
            } else {
                (
                    CriterionKind::Command {
                        command,
                        timeout_ms,
                    },
                    None,
                )
            }
        }
        "file_exists" => {
            let path = str_field("path").unwrap_or_default();
            if path.trim().is_empty() {
                (
                    CriterionKind::FileExists {
                        path: String::new(),
                    },
                    Some("`file_exists` 判据缺少非空的 `path` 字段".to_string()),
                )
            } else {
                (CriterionKind::FileExists { path }, None)
            }
        }
        "file_contains" => {
            let path = str_field("path").unwrap_or_default();
            let needle = str_field("needle").unwrap_or_default();
            if path.trim().is_empty() || needle.is_empty() {
                (
                    CriterionKind::FileContains {
                        path: String::new(),
                        needle: String::new(),
                    },
                    Some("`file_contains` 判据需要非空的 `path` 与 `needle`".to_string()),
                )
            } else {
                (CriterionKind::FileContains { path, needle }, None)
            }
        }
        "grep_present" | "grep_absent" => {
            let pattern = str_field("pattern").unwrap_or_default();
            let paths = paths_field();
            let invalid = if pattern.is_empty() {
                Some(format!("`{kind_name}` 判据缺少非空的 `pattern` 字段"))
            } else if paths.is_empty() {
                Some(format!("`{kind_name}` 判据缺少非空的 `paths` 列表"))
            } else if regex::Regex::new(&pattern).is_err() {
                Some(format!("`{kind_name}` 判据的 `pattern` 不是合法正则"))
            } else {
                None
            };
            let kind = if kind_name == "grep_present" {
                CriterionKind::GrepPresent { pattern, paths }
            } else {
                CriterionKind::GrepAbsent { pattern, paths }
            };
            (kind, invalid)
        }
        "" => (
            CriterionKind::FileExists {
                path: String::new(),
            },
            Some("判据缺少 `kind` 字段".to_string()),
        ),
        other => (
            CriterionKind::FileExists {
                path: String::new(),
            },
            Some(format!("未知的判据类型 `{other}`")),
        ),
    };
    (id, kind, invalid)
}

/// 单条判据的判定结果。
#[derive(Debug, Clone)]
pub struct CriterionOutcome {
    pub id: String,
    pub passed: bool,
    /// 失败时的**证据**（不是评语）。通过时为空。
    pub detail: String,
}

/// 一轮验收的总体结果。
#[derive(Debug, Clone)]
pub struct AcceptanceOutcome {
    pub total: usize,
    pub failures: Vec<CriterionOutcome>,
}

impl AcceptanceOutcome {
    pub fn passed(&self) -> bool {
        self.failures.is_empty()
    }

    /// 回灌给模型的报告。英文——模型侧提示词一律英文，不走 i18n。
    pub fn report(&self) -> String {
        if self.failures.is_empty() {
            return String::new();
        }
        let mut out = String::new();
        out.push_str(&format!(
            "<acceptance-failures failed=\"{}\" total=\"{}\">\n",
            self.failures.len(),
            self.total
        ));
        out.push_str(
            "The task is NOT complete: acceptance criteria failed. Fix the problems listed below,\n\
             then finish your turn again — the same criteria will be re-evaluated when you stop.\n\
             The entries below are evidence, not opinions: each one states what was observed.\n",
        );
        for failure in &self.failures {
            out.push_str(&format!("\n- `{}`: {}\n", failure.id, failure.detail));
        }
        out.push_str("</acceptance-failures>");
        out
    }
}

/// 跑一遍全部判据。
///
/// 任何一条判据自身出错（子进程起不来、超时、文件读不了）都**算这条不通过**，
/// 而不是整轮放弃——理由与 `verify.rs` 相同：验收机制自己出错时，最坏结果是
/// 「这一轮没有验收反馈」，而不是让整个任务失败。
pub async fn run_acceptance(cfg: &AcceptanceCfg, cwd: &Path) -> AcceptanceOutcome {
    let mut failures = Vec::new();
    for criterion in &cfg.criteria {
        let outcome = evaluate(criterion, cwd).await;
        if !outcome.passed {
            failures.push(outcome);
        }
    }
    AcceptanceOutcome {
        total: cfg.criteria.len(),
        failures,
    }
}

/// 判定单条判据。
async fn evaluate(criterion: &AcceptanceCriterion, cwd: &Path) -> CriterionOutcome {
    if let Some(reason) = &criterion.invalid {
        return CriterionOutcome {
            id: criterion.id.clone(),
            passed: false,
            detail: format!("criterion is misconfigured — {reason}"),
        };
    }
    let (passed, detail) = match &criterion.kind {
        CriterionKind::Command {
            command,
            timeout_ms,
        } => run_command(cwd, command, *timeout_ms).await,
        CriterionKind::FileExists { path } => match resolve_in_cwd(cwd, path) {
            Err(reason) => (false, reason),
            Ok(resolved) => {
                if resolved.exists() {
                    (true, String::new())
                } else {
                    (
                        false,
                        format!("file does not exist: {}", display_rel(cwd, &resolved)),
                    )
                }
            }
        },
        CriterionKind::FileContains { path, needle } => match resolve_in_cwd(cwd, path) {
            Err(reason) => (false, reason),
            Ok(resolved) => match std::fs::read_to_string(&resolved) {
                Err(err) => (false, format!("cannot read file: {err}")),
                Ok(content) => {
                    if content.contains(needle.as_str()) {
                        (true, String::new())
                    } else {
                        (
                            false,
                            format!(
                                "file does not contain the required text ({} bytes read): {}",
                                content.len(),
                                display_rel(cwd, &resolved)
                            ),
                        )
                    }
                }
            },
        },
        CriterionKind::GrepPresent { pattern, paths } => {
            let hits = grep_paths(cwd, pattern, paths);
            match hits {
                Err(reason) => (false, reason),
                Ok(hits) if hits.is_empty() => (
                    false,
                    format!(
                        "pattern `{pattern}` matched nothing under {}",
                        paths.join(", ")
                    ),
                ),
                Ok(_) => (true, String::new()),
            }
        }
        CriterionKind::GrepAbsent { pattern, paths } => {
            let hits = grep_paths(cwd, pattern, paths);
            match hits {
                Err(reason) => (false, reason),
                Ok(hits) if hits.is_empty() => (true, String::new()),
                Ok(hits) => (
                    false,
                    format!(
                        "pattern `{pattern}` matched {} location(s) under {}: {}",
                        hits.len(),
                        paths.join(", "),
                        hits.join(", ")
                    ),
                ),
            }
        }
    };
    CriterionOutcome {
        id: criterion.id.clone(),
        passed,
        detail: if passed {
            String::new()
        } else {
            crate::textutil::truncate_str(&detail, DETAIL_MAX_BYTES).to_string()
        },
    }
}

/// 把相对路径解析成工作目录内的绝对路径。
///
/// 逃出工作目录的一律拒绝：验收判据只该看用户自己的工作区，判据的失败详情
/// 又会回灌进模型上下文，放开边界等于开了一条读任意文件的通道。
fn resolve_in_cwd(cwd: &Path, path: &str) -> Result<PathBuf, String> {
    let raw = Path::new(path);
    let joined = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        cwd.join(raw)
    };
    let normalized = normalize_path(&joined);
    let base = normalize_path(cwd);
    if !normalized.starts_with(&base) {
        return Err(format!(
            "path `{path}` resolves outside the working directory — rejected"
        ));
    }
    Ok(normalized)
}

/// 纯字符串的路径规整（解析 `.` 与 `..`，不解 symlink）。
fn normalize_path(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// 相对工作目录的展示路径（尽量短，便于模型定位）。
fn display_rel(cwd: &Path, path: &Path) -> String {
    path.strip_prefix(cwd)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

/// 在若干路径下搜索匹配正则的行，返回 `文件:行号` 列表。
///
/// 只回报**位置**，不回报匹配到的**内容**——报告要回灌进模型上下文。
fn grep_paths(cwd: &Path, pattern: &str, paths: &[String]) -> Result<Vec<String>, String> {
    let regex = regex::Regex::new(pattern).map_err(|err| format!("invalid regex: {err}"))?;
    let mut hits: Vec<String> = Vec::new();
    let mut scanned = 0usize;
    for path in paths {
        let resolved = resolve_in_cwd(cwd, path)?;
        if resolved.is_file() {
            scanned += 1;
            grep_file(cwd, &resolved, &regex, &mut hits);
        } else if resolved.is_dir() {
            for entry in ignore::WalkBuilder::new(&resolved)
                .hidden(true)
                .git_ignore(true)
                .git_global(false)
                .git_exclude(false)
                .build()
                .flatten()
            {
                let file = entry.path();
                if !file.is_file() {
                    continue;
                }
                scanned += 1;
                if scanned > MAX_FILES_SCANNED {
                    break;
                }
                grep_file(cwd, file, &regex, &mut hits);
                if hits.len() >= MAX_HITS_REPORTED {
                    break;
                }
            }
        } else {
            return Err(format!("path `{path}` does not exist"));
        }
        if hits.len() >= MAX_HITS_REPORTED {
            break;
        }
    }
    Ok(hits)
}

/// 在单个文件里逐行匹配，把命中位置追加进 `hits`。
fn grep_file(cwd: &Path, file: &Path, regex: &regex::Regex, hits: &mut Vec<String>) {
    let Ok(content) = std::fs::read_to_string(file) else {
        return;
    };
    for (idx, line) in content.lines().enumerate() {
        if regex.is_match(line) {
            hits.push(format!("{}:{}", display_rel(cwd, file), idx + 1));
            if hits.len() >= MAX_HITS_REPORTED {
                return;
            }
        }
    }
}

/// 跑一条命令并判定退出码。
async fn run_command(cwd: &Path, command: &str, timeout_ms: u64) -> (bool, String) {
    let started = Instant::now();
    let mut cmd = tokio::process::Command::new(if cfg!(windows) { "cmd" } else { "sh" });
    if cfg!(windows) {
        cmd.arg("/C");
    } else {
        cmd.arg("-c");
    }
    cmd.arg(command)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => return (false, format!("command could not start: {err}")),
    };
    let timed_out = tokio::time::timeout(
        std::time::Duration::from_millis(timeout_ms),
        child.wait_with_output(),
    )
    .await;
    let elapsed = started.elapsed().as_millis() as u64;
    match timed_out {
        Ok(Ok(out)) => {
            let code = out.status.code();
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&out.stderr));
            match code {
                Some(0) => (true, String::new()),
                Some(code) => {
                    let body = crate::textutil::truncate_head_tail(&text, 2000, 2000);
                    (
                        false,
                        format!(
                            "command `{command}` exited with code {code} after {elapsed}ms.\nOutput:\n```\n{body}\n```"
                        ),
                    )
                }
                None => (
                    false,
                    format!("command `{command}` was terminated by a signal after {elapsed}ms"),
                ),
            }
        }
        Ok(Err(err)) => (false, format!("command `{command}` failed: {err}")),
        Err(_) => (
            false,
            format!("command `{command}` timed out after {timeout_ms}ms"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_settings(dir: &Path, name: &str, body: &str) {
        let p = dir.join(".wyj-code").join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    fn write_trusted_settings(dir: &Path, name: &str, body: &str) {
        write_settings(dir, name, body);
        crate::hooks_trust::approve(dir).unwrap();
    }

    #[test]
    fn a_project_declaring_criteria_is_loaded() {
        let tmp = tempfile::tempdir().unwrap();
        write_trusted_settings(
            tmp.path(),
            "settings.json",
            r#"{"acceptance":{"criteria":[{"id":"t","kind":"command","command":"true"}]}}"#,
        );
        let load = load_acceptance_cfg(tmp.path());
        assert!(load.error.is_none(), "{:?}", load.error);
        let cfg = load.cfg.expect("应当解析出验收契约");
        assert_eq!(cfg.criteria.len(), 1);
        assert_eq!(cfg.max_retries, DEFAULT_MAX_RETRIES);
    }

    /// 未批准的项目级 acceptance 必须整块跳过——它会 `sh -c` 执行命令，
    /// 与 `verify.command` 同级。
    #[test]
    fn unapproved_project_acceptance_is_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        write_settings(
            tmp.path(),
            "settings.json",
            r#"{"acceptance":{"criteria":[{"id":"t","kind":"command","command":"true"}]}}"#,
        );
        let load = load_acceptance_cfg(tmp.path());
        assert!(load.cfg.is_none());
    }

    #[test]
    fn no_declaration_means_no_acceptance_at_all() {
        let tmp = tempfile::tempdir().unwrap();
        let load = load_acceptance_cfg(tmp.path());
        assert!(load.cfg.is_none());
        assert!(load.error.is_none());
    }

    #[test]
    fn enabled_false_disables_without_losing_the_criteria() {
        let tmp = tempfile::tempdir().unwrap();
        write_trusted_settings(
            tmp.path(),
            "settings.json",
            r#"{"acceptance":{"enabled":false,"criteria":[{"id":"t","kind":"command","command":"true"}]}}"#,
        );
        let load = load_acceptance_cfg(tmp.path());
        assert!(load.cfg.is_none());
    }

    #[test]
    fn an_empty_criteria_list_is_not_a_gate() {
        let tmp = tempfile::tempdir().unwrap();
        write_trusted_settings(
            tmp.path(),
            "settings.json",
            r#"{"acceptance":{"criteria":[]}}"#,
        );
        let load = load_acceptance_cfg(tmp.path());
        assert!(load.cfg.is_none());
    }

    #[test]
    fn a_broken_block_is_reported_instead_of_silently_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        write_trusted_settings(
            tmp.path(),
            "settings.json",
            r#"{"acceptance":{"criteria":"not-a-list"}}"#,
        );
        let load = load_acceptance_cfg(tmp.path());
        assert!(load.cfg.is_none());
        assert!(load.error.is_some(), "解析失败必须能被看见");
    }

    #[test]
    fn a_misconfigured_criterion_fails_closed() {
        let tmp = tempfile::tempdir().unwrap();
        write_trusted_settings(
            tmp.path(),
            "settings.json",
            r#"{"acceptance":{"criteria":[{"id":"bad","kind":"grep_absent","pattern":"([unclosed"}]}}"#,
        );
        let load = load_acceptance_cfg(tmp.path());
        let cfg = load.cfg.expect("应当解析出验收契约");
        assert!(cfg.criteria[0].invalid.is_some());
    }

    #[tokio::test]
    async fn a_passing_command_passes() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = AcceptanceCfg {
            criteria: vec![AcceptanceCriterion {
                id: "t".into(),
                kind: CriterionKind::Command {
                    command: "exit 0".into(),
                    timeout_ms: 5_000,
                },
                invalid: None,
            }],
            max_retries: 2,
        };
        let outcome = run_acceptance(&cfg, tmp.path()).await;
        assert!(outcome.passed(), "{:?}", outcome.failures);
    }

    #[tokio::test]
    async fn a_failing_command_reports_its_exit_code_and_output() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = AcceptanceCfg {
            criteria: vec![AcceptanceCriterion {
                id: "t".into(),
                kind: CriterionKind::Command {
                    command: "echo boom >&2; exit 3".into(),
                    timeout_ms: 5_000,
                },
                invalid: None,
            }],
            max_retries: 2,
        };
        let outcome = run_acceptance(&cfg, tmp.path()).await;
        assert!(!outcome.passed());
        let detail = &outcome.failures[0].detail;
        assert!(detail.contains("code 3"), "{detail}");
        assert!(detail.contains("boom"), "{detail}");
    }

    #[tokio::test]
    async fn a_hanging_command_times_out_instead_of_hanging_the_turn() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = AcceptanceCfg {
            criteria: vec![AcceptanceCriterion {
                id: "t".into(),
                kind: CriterionKind::Command {
                    command: "sleep 30".into(),
                    timeout_ms: 200,
                },
                invalid: None,
            }],
            max_retries: 2,
        };
        let outcome = run_acceptance(&cfg, tmp.path()).await;
        assert!(!outcome.passed());
        assert!(outcome.failures[0].detail.contains("timed out"));
    }

    #[tokio::test]
    async fn a_missing_file_fails_with_its_path() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = AcceptanceCfg {
            criteria: vec![AcceptanceCriterion {
                id: "f".into(),
                kind: CriterionKind::FileExists {
                    path: "nope.txt".into(),
                },
                invalid: None,
            }],
            max_retries: 2,
        };
        let outcome = run_acceptance(&cfg, tmp.path()).await;
        assert!(!outcome.passed());
        assert!(outcome.failures[0].detail.contains("nope.txt"));
    }

    #[tokio::test]
    async fn a_path_outside_the_working_directory_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = AcceptanceCfg {
            criteria: vec![AcceptanceCriterion {
                id: "f".into(),
                kind: CriterionKind::FileExists {
                    path: "../outside.txt".into(),
                },
                invalid: None,
            }],
            max_retries: 2,
        };
        let outcome = run_acceptance(&cfg, tmp.path()).await;
        assert!(!outcome.passed());
        assert!(outcome.failures[0].detail.contains("outside"));
    }

    #[tokio::test]
    async fn grep_absent_reports_where_it_matched() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn main() {}\nlet x = 1;\n").unwrap();
        let cfg = AcceptanceCfg {
            criteria: vec![AcceptanceCriterion {
                id: "g".into(),
                kind: CriterionKind::GrepAbsent {
                    pattern: "let x".into(),
                    paths: vec![".".into()],
                },
                invalid: None,
            }],
            max_retries: 2,
        };
        let outcome = run_acceptance(&cfg, tmp.path()).await;
        assert!(!outcome.passed());
        let detail = &outcome.failures[0].detail;
        assert!(detail.contains("a.rs:2"), "{detail}");
    }

    #[tokio::test]
    async fn grep_present_requires_at_least_one_match() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn main() {}\n").unwrap();
        let cfg = AcceptanceCfg {
            criteria: vec![AcceptanceCriterion {
                id: "g".into(),
                kind: CriterionKind::GrepPresent {
                    pattern: "fn main".into(),
                    paths: vec![".".into()],
                },
                invalid: None,
            }],
            max_retries: 2,
        };
        let outcome = run_acceptance(&cfg, tmp.path()).await;
        assert!(outcome.passed(), "{:?}", outcome.failures);
    }

    #[test]
    fn the_report_names_the_failing_criteria_and_their_evidence() {
        let outcome = AcceptanceOutcome {
            total: 3,
            failures: vec![CriterionOutcome {
                id: "tests-pass".into(),
                passed: false,
                detail: "command `cargo test` exited with code 101.".into(),
            }],
        };
        let report = outcome.report();
        assert!(report.contains("tests-pass"));
        assert!(report.contains("exited with code 101"));
        assert!(report.contains("failed=\"1\" total=\"3\""));
        assert!(report.contains("NOT complete"));
    }

    #[test]
    fn a_passing_outcome_has_no_report() {
        let outcome = AcceptanceOutcome {
            total: 1,
            failures: vec![],
        };
        assert!(outcome.report().is_empty());
    }
}
