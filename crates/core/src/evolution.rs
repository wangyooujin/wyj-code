//! Evidence-backed, local-only Agent evolution.
//!
//! The runtime records one user goal as an [`Episode`], derives scoped memories
//! and Rule/Skill candidates only from auditable evidence, and keeps activation
//! separate from proposal.  All durable state lives under
//! `~/.wyj-code/evolution/<project-id>/`; no telemetry or remote synchronization
//! is performed here.

use crate::{project_id, project_root, redact_sensitive_text, Session};
use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, Utc};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use wyj_api::provider::Provider;
use wyj_api::types::{ContentBlock, Message};
use wyj_config::EvolutionCfg;

pub const EVOLUTION_SCHEMA_VERSION: u32 = 2;
const CANDIDATE_INDEX_FILE: &str = "candidates/index.json";
const FEEDBACK_INDEX_FILE: &str = "feedback/index.json";
const HEALTH_FILE: &str = "health.json";
const USAGE_FILE: &str = "usage.json";
const AUDIT_FILE: &str = "audit.jsonl";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EpisodeOutcome {
    VerifiedSuccess,
    AcceptedSuccess,
    Partial,
    Failed,
    Cancelled,
    Unknown,
}

impl EpisodeOutcome {
    pub fn supports_repository_learning(self) -> bool {
        self == Self::VerifiedSuccess
    }

    pub fn supports_user_preference(self) -> bool {
        matches!(
            self,
            Self::VerifiedSuccess | Self::AcceptedSuccess | Self::Partial
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    UserFeedback,
    Test,
    Review,
    Tool,
    Git,
    ModelReflection,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EpisodeEvidence {
    pub kind: EvidenceKind,
    pub label: String,
    pub success: Option<bool>,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Episode {
    pub schema_version: u32,
    pub id: String,
    pub session_id: String,
    pub project_id: String,
    pub repository_root: PathBuf,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub goal_summary: String,
    pub started_at: String,
    pub finished_at: String,
    pub duration_ms: u64,
    pub outcome: EpisodeOutcome,
    pub confidence: u8,
    pub profile: String,
    pub vendor: String,
    pub model: String,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub changed_paths: Vec<PathBuf>,
    pub evidence: Vec<EpisodeEvidence>,
    pub external_context: bool,
    pub included_by_user: bool,
    pub source_session_message_start: usize,
    pub source_session_message_end: usize,
}

#[derive(Debug, Clone)]
pub struct EpisodeCapture {
    id: String,
    session_id: String,
    project_id: String,
    repository_root: PathBuf,
    branch: Option<String>,
    head: Option<String>,
    goal_summary: String,
    started_at: String,
    started: Instant,
    profile: String,
    vendor: String,
    model: String,
    message_start: usize,
    input_tokens: u32,
    output_tokens: u32,
    initial_worktree: BTreeMap<PathBuf, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    UserPreference,
    RepositoryFact,
    Workflow,
    FailurePattern,
    Reference,
    #[default]
    Unspecified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MemoryStatus {
    Proposed,
    Active,
    Conflict,
    Stale,
    Rejected,
    Forgotten,
    #[default]
    Unspecified,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoryScope {
    pub level: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepositoryCitation {
    pub repository_id: String,
    pub commit: Option<String>,
    pub branch: Option<String>,
    pub path: PathBuf,
    pub blob_oid: Option<String>,
    pub working_tree_sha256: Option<String>,
    pub symbol: Option<String>,
    pub context_fingerprint: Option<String>,
    pub display_line: Option<u32>,
}

/// v1.5.5 收敛：普通 Memory 数据层已迁出 `MemoryV3Store`。Evolution 里
/// `EvolutionMemory` 仅保留为兼容 stub：构造该类型用于类型签名兼容，
/// 但所有字段都为空壳——不应再被实例化或序列化进入活跃索引。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EvolutionMemory {
    pub schema_version: u32,
    pub id: String,
    pub kind: MemoryKind,
    pub name: String,
    pub summary: String,
    pub body: String,
    pub scope: MemoryScope,
    pub status: MemoryStatus,
    pub pinned: bool,
    pub confidence: u8,
    pub evidence_episode_ids: Vec<String>,
    pub evidence_session_ids: Vec<String>,
    pub user_quote: Option<String>,
    pub citations: Vec<RepositoryCitation>,
    pub external_context: bool,
    pub created_at: String,
    pub updated_at: String,
    pub last_validated_at: Option<String>,
    pub last_used_at: Option<String>,
    pub use_count: u64,
    pub contradicts: Vec<String>,
    pub supersedes: Vec<String>,
}

/// 供 `list_memories` 兼容桩使用的最小化返回类型。v1.5.5 后 Evolution 不再
/// 持有普通 Memory 索引；TUI/CLI 调用站点若读到 `Vec<EvolutionMemoryStub>`，
/// 应当作空列表处理，让用户改用 `/memory` 面板（v3）做事实/状态管理。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EvolutionMemoryStub {
    pub schema_version: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateKind {
    Rule,
    Skill,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateStatus {
    Proposed,
    Validating,
    Validated,
    Active,
    Rejected,
    Failed,
    Stale,
    RolledBack,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillEvalCase {
    pub category: String,
    pub prompt: String,
    pub expected: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillEvalReport {
    pub generated_at: String,
    pub cases: Vec<SkillEvalCase>,
    pub structural_pass: bool,
    pub historical_successes: usize,
    pub distinct_sessions: usize,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CandidatePayload {
    Rule {
        rule_text: String,
        suggested_target: String,
    },
    Skill {
        skill_name: String,
        description: String,
        skill_md: String,
        eval: SkillEvalReport,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvolutionCandidate {
    pub schema_version: u32,
    pub id: String,
    pub kind: CandidateKind,
    pub status: CandidateStatus,
    pub title: String,
    pub summary: String,
    pub risk: String,
    /// `global` / `project:<project_key>`：与 v3 Memory 一致；不再分
    /// level/value 两字段，因为已经收敛到两 scope。
    pub scope: String,
    pub evidence_episode_ids: Vec<String>,
    pub evidence_session_ids: Vec<String>,
    pub payload: CandidatePayload,
    pub created_at: String,
    pub updated_at: String,
    pub rejection_reason: Option<String>,
    pub activated_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvolutionFeedback {
    pub episode_id: String,
    pub positive: bool,
    pub reason: Option<String>,
    pub explicit: bool,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CandidateIndex {
    #[serde(default = "schema_version")]
    schema_version: u32,
    candidates: Vec<EvolutionCandidate>,
}

impl Default for CandidateIndex {
    fn default() -> Self {
        Self {
            schema_version: EVOLUTION_SCHEMA_VERSION,
            candidates: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FeedbackIndex {
    #[serde(default = "schema_version")]
    schema_version: u32,
    feedback: Vec<EvolutionFeedback>,
    included_episodes: BTreeSet<String>,
    #[serde(default)]
    pending_analysis_episodes: BTreeSet<String>,
}

impl Default for FeedbackIndex {
    fn default() -> Self {
        Self {
            schema_version: EVOLUTION_SCHEMA_VERSION,
            feedback: Vec::new(),
            included_episodes: BTreeSet::new(),
            pending_analysis_episodes: BTreeSet::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvolutionHealth {
    pub schema_version: u32,
    pub consecutive_failures: u32,
    pub last_error: Option<String>,
    pub last_success_at: Option<String>,
    pub notice_pending: bool,
}

impl Default for EvolutionHealth {
    fn default() -> Self {
        Self {
            schema_version: EVOLUTION_SCHEMA_VERSION,
            consecutive_failures: 0,
            last_error: None,
            last_success_at: None,
            notice_pending: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DailyUsage {
    #[serde(default = "schema_version")]
    schema_version: u32,
    date: String,
    tokens: u32,
    wall_secs: u64,
}

impl Default for DailyUsage {
    fn default() -> Self {
        Self {
            schema_version: EVOLUTION_SCHEMA_VERSION,
            date: Utc::now().date_naive().to_string(),
            tokens: 0,
            wall_secs: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvolutionStatus {
    pub project_id: String,
    pub directory: PathBuf,
    pub episodes: usize,
    pub pending_candidates: usize,
    pub active_candidates: usize,
    pub store_bytes: u64,
    pub health: EvolutionHealth,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationPreview {
    pub legacy_directory: PathBuf,
    pub files: Vec<PathBuf>,
    pub entries: usize,
    pub backup_directory: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AuditEvent {
    schema_version: u32,
    timestamp: String,
    action: String,
    target_id: Option<String>,
    detail: String,
}

#[derive(Debug, Deserialize)]
struct AnalysisItem {
    kind: String,
    name: String,
    summary: String,
    body: String,
    #[serde(default)]
    rule_suggestion: Option<String>,
    #[serde(default)]
    skill_description: Option<String>,
    #[serde(default)]
    skill_steps: Vec<String>,
}

/// v1.5.5 收敛：Evolution 不再生成普通 Memory，只剩治理（Rule/Skill
/// candidate + Episode 健康度）。`MemoryAndGovernance` 保留为别名，
/// 等价 `GovernanceOnly`，让旧代码/测试引用继续编译。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EvolutionAnalysisMode {
    GovernanceOnly,
}

fn schema_version() -> u32 {
    EVOLUTION_SCHEMA_VERSION
}

#[derive(Clone)]
pub struct EvolutionStore {
    dir: PathBuf,
    legacy_dir: PathBuf,
    repository_root: PathBuf,
    project_id: String,
    cfg: EvolutionCfg,
    write_lock: Arc<Mutex<()>>,
    activity_generation: Arc<AtomicU64>,
    workers: Arc<Semaphore>,
}

impl EvolutionStore {
    pub fn new(base_dir: &Path, cwd: &Path, cfg: EvolutionCfg) -> Result<Self> {
        let repository_root = project_root(cwd);
        // v1.5.5 收敛：Evolution 不再维护 legacy memory dir（普通 Memory 已
        // 完全迁出 v3）。保留字段避免破坏外部 `EvolutionStore.directory()`
        // 等 API；实际读取时永远为空。
        let legacy_dir = base_dir.join("memory").join(project_id(&repository_root));
        let project_id = project_id(&repository_root);
        let dir = base_dir.join("evolution").join(&project_id);
        for child in ["episodes", "candidates", "feedback"] {
            fs::create_dir_all(dir.join(child))?;
        }
        Ok(Self {
            legacy_dir,
            dir,
            repository_root,
            project_id,
            workers: Arc::new(Semaphore::new(cfg.max_background_workers.max(1) as usize)),
            cfg,
            write_lock: Arc::new(Mutex::new(())),
            activity_generation: Arc::new(AtomicU64::new(0)),
        })
    }

    pub fn directory(&self) -> &Path {
        &self.dir
    }

    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    pub fn config(&self) -> &EvolutionCfg {
        &self.cfg
    }

    /// 为一个新的 Agent 回合构造不可变上下文快照。当前回合内始终复用调用方
    /// 保存的字符串；批准、遗忘或后台提取只会在下一回合重新加载时生效。
    pub fn context_snapshot(&self, goal: &str) -> String {
        self.load_context(goal)
    }

    pub fn begin_episode(
        &self,
        session_id: impl Into<String>,
        session: &Session,
        goal: &str,
        profile: impl Into<String>,
        vendor: impl Into<String>,
        model: impl Into<String>,
    ) -> EpisodeCapture {
        if self.cfg.infer_feedback {
            let _ = self.infer_feedback_from_goal(goal);
        }
        EpisodeCapture {
            id: format!(
                "ep-{}-{}",
                Utc::now().format("%Y%m%dT%H%M%S%3fZ"),
                &uuid::Uuid::new_v4().simple().to_string()[..8]
            ),
            session_id: session_id.into(),
            project_id: self.project_id.clone(),
            repository_root: self.repository_root.clone(),
            branch: git_text(&self.repository_root, &["branch", "--show-current"]),
            head: git_text(&self.repository_root, &["rev-parse", "HEAD"]),
            goal_summary: bounded_redacted(goal, 800),
            started_at: Utc::now().to_rfc3339(),
            started: Instant::now(),
            profile: profile.into(),
            vendor: vendor.into(),
            model: model.into(),
            message_start: session.messages.len().saturating_sub(1),
            input_tokens: session.total_input_tokens,
            output_tokens: session.total_output_tokens,
            initial_worktree: git_worktree_snapshot(&self.repository_root),
        }
    }

    pub fn finish_episode(
        &self,
        capture: EpisodeCapture,
        session: &Session,
        result: &Result<()>,
    ) -> Result<Episode> {
        let messages = session
            .messages
            .get(capture.message_start..)
            .unwrap_or_default();
        let analysis = analyze_messages(messages);
        let outcome = if result
            .as_ref()
            .err()
            .is_some_and(|error| looks_like_cancelled(&error.to_string()))
        {
            EpisodeOutcome::Cancelled
        } else if result.is_err() {
            EpisodeOutcome::Failed
        } else if analysis.any_required_check && analysis.all_required_checks_passed {
            EpisodeOutcome::VerifiedSuccess
        } else if analysis.security_failure || analysis.any_unrecovered_tool_error {
            EpisodeOutcome::Failed
        } else {
            EpisodeOutcome::Unknown
        };
        let confidence = match outcome {
            EpisodeOutcome::VerifiedSuccess => 95,
            EpisodeOutcome::AcceptedSuccess => 80,
            EpisodeOutcome::Partial => 45,
            EpisodeOutcome::Failed => 90,
            EpisodeOutcome::Cancelled => 100,
            EpisodeOutcome::Unknown => 20,
        };
        let mut evidence = analysis.evidence;
        if let Err(error) = result {
            evidence.push(EpisodeEvidence {
                kind: EvidenceKind::Tool,
                label: "turn_error".to_string(),
                success: Some(false),
                detail: bounded_redacted(&error.to_string(), 1_000),
            });
        }
        let included = self
            .load_feedback_index()
            .included_episodes
            .contains(&capture.id);
        let episode = Episode {
            schema_version: EVOLUTION_SCHEMA_VERSION,
            id: capture.id,
            session_id: capture.session_id,
            project_id: capture.project_id,
            repository_root: capture.repository_root,
            branch: capture.branch,
            head: capture.head,
            goal_summary: capture.goal_summary,
            started_at: capture.started_at,
            finished_at: Utc::now().to_rfc3339(),
            duration_ms: capture.started.elapsed().as_millis().min(u64::MAX as u128) as u64,
            outcome,
            confidence,
            profile: capture.profile,
            vendor: capture.vendor,
            model: capture.model,
            input_tokens: session
                .total_input_tokens
                .saturating_sub(capture.input_tokens),
            output_tokens: session
                .total_output_tokens
                .saturating_sub(capture.output_tokens),
            changed_paths: changed_since(
                &capture.initial_worktree,
                &git_worktree_snapshot(&self.repository_root),
            ),
            evidence,
            external_context: analysis.external_context,
            included_by_user: included,
            source_session_message_start: capture.message_start,
            source_session_message_end: session.messages.len(),
        };
        self.append_episode(&episode)?;
        self.enforce_retention_and_capacity()?;
        self.activity_generation.fetch_add(1, Ordering::AcqRel);
        Ok(episode)
    }

    /// Future 被 TUI 的 Esc/Ctrl+C 直接 abort 时，正常的 async 收尾代码不会再被
    /// poll。调用方的 Drop guard 使用此同步路径写入最小 cancelled Episode，确保
    /// 取消目标既不会丢失，也绝不会被错误计作失败经验。
    pub fn cancel_episode(&self, capture: EpisodeCapture) -> Result<Episode> {
        let episode = Episode {
            schema_version: EVOLUTION_SCHEMA_VERSION,
            id: capture.id,
            session_id: capture.session_id,
            project_id: capture.project_id,
            repository_root: capture.repository_root,
            branch: capture.branch,
            head: capture.head,
            goal_summary: capture.goal_summary,
            started_at: capture.started_at,
            finished_at: Utc::now().to_rfc3339(),
            duration_ms: capture.started.elapsed().as_millis().min(u64::MAX as u128) as u64,
            outcome: EpisodeOutcome::Cancelled,
            confidence: 100,
            profile: capture.profile,
            vendor: capture.vendor,
            model: capture.model,
            input_tokens: 0,
            output_tokens: 0,
            changed_paths: changed_since(
                &capture.initial_worktree,
                &git_worktree_snapshot(&self.repository_root),
            ),
            evidence: vec![EpisodeEvidence {
                kind: EvidenceKind::UserFeedback,
                label: "turn_cancelled".to_string(),
                success: None,
                detail: "Agent turn future was cancelled before normal completion".to_string(),
            }],
            external_context: false,
            included_by_user: false,
            source_session_message_start: capture.message_start,
            source_session_message_end: capture.message_start,
        };
        self.append_episode(&episode)?;
        self.enforce_retention_and_capacity()?;
        self.activity_generation.fetch_add(1, Ordering::AcqRel);
        Ok(episode)
    }

    pub fn schedule_analysis(&self, episode: Episode, provider: Arc<dyn Provider>) {
        // v1.5.5 收敛：Evolution 不再生成普通 Memory；只剩 Rule/Skill
        // 治理分析。该入口保留名称（外部 CLI/TUI 仍可能调用）并直接转发
        // 到 GovernanceOnly 调度。
        self.schedule_analysis_with_mode(episode, provider, EvolutionAnalysisMode::GovernanceOnly);
    }

    /// `schedule_analysis` 的同义别名，文档强调"只剩治理"。外部代码
    /// （tests、文档示例）可以直接用更清晰的命名。
    pub fn schedule_governance_analysis(&self, episode: Episode, provider: Arc<dyn Provider>) {
        self.schedule_analysis_with_mode(episode, provider, EvolutionAnalysisMode::GovernanceOnly);
    }

    fn schedule_analysis_with_mode(
        &self,
        episode: Episode,
        provider: Arc<dyn Provider>,
        mode: EvolutionAnalysisMode,
    ) {
        if !self.cfg.enabled {
            return;
        }
        if episode.external_context
            && self.cfg.exclude_external_context
            && !episode.included_by_user
        {
            return;
        }
        let generation = self.activity_generation.load(Ordering::Acquire);
        let store = self.clone();
        tokio::spawn(async move {
            if store.cfg.idle_delay_secs > 0 {
                tokio::time::sleep(Duration::from_secs(store.cfg.idle_delay_secs)).await;
            }
            if store.activity_generation.load(Ordering::Acquire) != generation {
                return;
            }
            let Ok(_permit) = store.workers.clone().acquire_owned().await else {
                return;
            };
            let delays = [30_u64, 120, 600];
            for attempt in 0..=delays.len() {
                let started = Instant::now();
                match store
                    .analyze_episode(&episode, provider.clone(), mode)
                    .await
                {
                    Ok(tokens) => {
                        let _ = store.record_usage(tokens, started.elapsed().as_secs());
                        let _ = store.record_health_success();
                        let _ = store.clear_pending_analysis(&episode.id);
                        return;
                    }
                    Err(error) => {
                        let retryable = error
                            .downcast_ref::<wyj_api::ProviderError>()
                            .is_some_and(|provider_error| provider_error.retryable);
                        let _ = store.record_health_failure(&error.to_string());
                        if !retryable || attempt == delays.len() {
                            let _ = store.mark_pending_analysis(&episode.id);
                            return;
                        }
                        tokio::time::sleep(Duration::from_secs(delays[attempt])).await;
                    }
                }
            }
        });
    }

    /// 在下一次正常 Agent 回合到来时，重新处理由显式反馈或手动纳入触发的旧
    /// Episode。v1.5.5 后只剩 Rule/Skill 治理分析。
    pub fn schedule_pending_analysis(&self, provider: Arc<dyn Provider>) {
        self.schedule_pending_analysis_with_mode(provider, EvolutionAnalysisMode::GovernanceOnly);
    }

    pub fn schedule_pending_governance_analysis(&self, provider: Arc<dyn Provider>) {
        self.schedule_pending_analysis_with_mode(provider, EvolutionAnalysisMode::GovernanceOnly);
    }

    fn schedule_pending_analysis_with_mode(
        &self,
        provider: Arc<dyn Provider>,
        mode: EvolutionAnalysisMode,
    ) {
        if !self.cfg.enabled {
            return;
        }
        let pending = self.take_pending_analysis();
        if pending.is_empty() {
            return;
        }
        let episodes = self.list_episodes(usize::MAX).unwrap_or_default();
        for episode_id in pending {
            if let Some(episode) = episodes
                .iter()
                .find(|episode| episode.id == episode_id)
                .cloned()
            {
                self.schedule_analysis_with_mode(episode, provider.clone(), mode);
            }
        }
    }

    pub fn list_episodes(&self, limit: usize) -> Result<Vec<Episode>> {
        let mut files: Vec<PathBuf> = fs::read_dir(self.dir.join("episodes"))?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
            .collect();
        files.sort();
        let feedback = self.load_feedback_index();
        let feedback_by_episode: HashMap<&str, &EvolutionFeedback> = feedback
            .feedback
            .iter()
            .map(|item| (item.episode_id.as_str(), item))
            .collect();
        let mut episodes = Vec::new();
        for path in files {
            for mut episode in read_jsonl::<Episode>(&path)? {
                if let Some(item) = feedback_by_episode.get(episode.id.as_str()) {
                    episode.outcome = if item.positive {
                        EpisodeOutcome::AcceptedSuccess
                    } else if item.explicit {
                        EpisodeOutcome::Failed
                    } else {
                        EpisodeOutcome::Partial
                    };
                    episode.evidence.push(EpisodeEvidence {
                        kind: EvidenceKind::UserFeedback,
                        label: if item.explicit {
                            "explicit_feedback"
                        } else {
                            "inferred_feedback"
                        }
                        .to_string(),
                        success: Some(item.positive),
                        detail: item.reason.clone().unwrap_or_default(),
                    });
                }
                episode.included_by_user = feedback.included_episodes.contains(&episode.id);
                episodes.push(episode);
            }
        }
        episodes.sort_by(|left, right| right.finished_at.cmp(&left.finished_at));
        episodes.truncate(limit);
        Ok(episodes)
    }

    /// v1.5.5 收敛：普通 Memory 数据层已迁出 Evolution；`/evolve` 面板只剩
    /// Active / Candidates / Episodes / Health。`list_memories` 保留为返回
    /// 空 Vec 的兼容桩，避免外部 TUI/CLI 调用站点批量报错。
    pub fn list_memories(&self) -> Result<Vec<EvolutionMemory>> {
        Ok(Vec::new())
    }

    pub fn list_candidates(&self) -> Result<Vec<EvolutionCandidate>> {
        let mut index = self.load_candidate_index();
        index.candidates.sort_by(|left, right| {
            candidate_status_rank(left.status)
                .cmp(&candidate_status_rank(right.status))
                .then_with(|| right.updated_at.cmp(&left.updated_at))
        });
        Ok(index.candidates)
    }

    pub fn status(&self) -> Result<EvolutionStatus> {
        let episodes = self.list_episodes(usize::MAX)?;
        let candidates = self.list_candidates()?;
        Ok(EvolutionStatus {
            project_id: self.project_id.clone(),
            directory: self.dir.clone(),
            episodes: episodes.len(),
            pending_candidates: candidates
                .iter()
                .filter(|candidate| {
                    matches!(
                        candidate.status,
                        CandidateStatus::Proposed
                            | CandidateStatus::Validating
                            | CandidateStatus::Validated
                    )
                })
                .count(),
            active_candidates: candidates
                .iter()
                .filter(|candidate| candidate.status == CandidateStatus::Active)
                .count(),
            store_bytes: directory_size(&self.dir),
            health: self.health(),
        })
    }

    pub fn health(&self) -> EvolutionHealth {
        read_json_or_default(&self.dir.join(HEALTH_FILE))
    }

    pub fn feedback_latest(&self, positive: bool, reason: Option<String>) -> Result<String> {
        let episode = self
            .list_episodes(1)?
            .into_iter()
            .next()
            .context("no evolution episode exists for this project")?;
        self.feedback_episode(&episode.id, positive, reason, true)?;
        Ok(episode.id)
    }

    pub fn feedback_episode(
        &self,
        episode_id: &str,
        positive: bool,
        reason: Option<String>,
        explicit: bool,
    ) -> Result<()> {
        let _guard = self
            .write_lock
            .lock()
            .expect("evolution write lock poisoned");
        let mut index = self.load_feedback_index_unlocked();
        index.feedback.retain(|item| item.episode_id != episode_id);
        index.feedback.push(EvolutionFeedback {
            episode_id: episode_id.to_string(),
            positive,
            reason: reason.map(|value| bounded_redacted(&value, 1_000)),
            explicit,
            created_at: Utc::now().to_rfc3339(),
        });
        index
            .pending_analysis_episodes
            .insert(episode_id.to_string());
        write_json_atomic(&self.dir.join(FEEDBACK_INDEX_FILE), &index)?;
        self.append_audit_unlocked(
            "episode_feedback",
            Some(episode_id),
            if positive { "good" } else { "bad" },
        )
    }

    pub fn include_episode(&self, episode_id: &str) -> Result<()> {
        anyhow::ensure!(
            self.list_episodes(usize::MAX)?
                .iter()
                .any(|episode| episode.id == episode_id),
            "episode not found: {episode_id}"
        );
        let _guard = self
            .write_lock
            .lock()
            .expect("evolution write lock poisoned");
        let mut index = self.load_feedback_index_unlocked();
        index.included_episodes.insert(episode_id.to_string());
        index
            .pending_analysis_episodes
            .insert(episode_id.to_string());
        write_json_atomic(&self.dir.join(FEEDBACK_INDEX_FILE), &index)?;
        self.append_audit_unlocked(
            "include_external_episode",
            Some(episode_id),
            "manual repository-scope inclusion",
        )
    }

    pub fn pin_memory(&self, _memory_id: &str, _pinned: bool) -> Result<()> {
        // 普通 Memory 已迁出 Evolution → 改走 /memory 面板。`pin_memory` 留
        // 空函数以兼容旧 CLI/TUI 调用站点；调用方应迁移到 `Memory` 工具
        // 的对应 action（v3 中不再需要 pinned 字段，由 activate 决定 active）。
        Ok(())
    }

    pub fn activate_memory(&self, _memory_id: &str) -> Result<()> {
        // 普通 Memory 已迁出 Evolution；activate 改走 v3 Memory 工具的
        // `confirm_global_candidate` / 直接写 Active claim。
        Ok(())
    }

    pub fn forget_memory(&self, _memory_id: &str) -> Result<()> {
        // 普通 Memory 已迁出 Evolution；forget 改走 v3 Memory 工具的 `forget` action。
        Ok(())
    }

    pub fn reject_candidate(&self, candidate_id: &str, reason: Option<String>) -> Result<()> {
        let _guard = self
            .write_lock
            .lock()
            .expect("evolution write lock poisoned");
        let mut index = self.load_candidate_index_unlocked();
        let candidate = index
            .candidates
            .iter_mut()
            .find(|candidate| candidate.id == candidate_id)
            .with_context(|| format!("candidate not found: {candidate_id}"))?;
        candidate.status = CandidateStatus::Rejected;
        candidate.rejection_reason = reason.map(|value| bounded_redacted(&value, 1_000));
        candidate.updated_at = Utc::now().to_rfc3339();
        write_json_atomic(&self.dir.join(CANDIDATE_INDEX_FILE), &index)?;
        self.append_audit_unlocked("reject_candidate", Some(candidate_id), "")
    }

    pub fn mark_candidate_active(
        &self,
        candidate_id: &str,
        activated_path: Option<PathBuf>,
    ) -> Result<()> {
        let _guard = self
            .write_lock
            .lock()
            .expect("evolution write lock poisoned");
        let mut index = self.load_candidate_index_unlocked();
        let candidate = index
            .candidates
            .iter_mut()
            .find(|candidate| candidate.id == candidate_id)
            .with_context(|| format!("candidate not found: {candidate_id}"))?;
        candidate.status = CandidateStatus::Active;
        candidate.activated_path = activated_path;
        candidate.updated_at = Utc::now().to_rfc3339();
        write_json_atomic(&self.dir.join(CANDIDATE_INDEX_FILE), &index)?;
        self.append_audit_unlocked("activate_candidate", Some(candidate_id), "")
    }

    pub fn rollback_candidate(&self, candidate_id: &str) -> Result<()> {
        let _guard = self
            .write_lock
            .lock()
            .expect("evolution write lock poisoned");
        let mut index = self.load_candidate_index_unlocked();
        let candidate = index
            .candidates
            .iter_mut()
            .find(|candidate| candidate.id == candidate_id)
            .with_context(|| format!("candidate not found: {candidate_id}"))?;
        candidate.status = CandidateStatus::RolledBack;
        candidate.updated_at = Utc::now().to_rfc3339();
        write_json_atomic(&self.dir.join(CANDIDATE_INDEX_FILE), &index)?;
        self.append_audit_unlocked("rollback_candidate", Some(candidate_id), "")
    }

    pub fn create_skill_candidate_from_episode(&self, episode_id: &str) -> Result<String> {
        let episode = self
            .list_episodes(usize::MAX)?
            .into_iter()
            .find(|episode| episode.id == episode_id)
            .with_context(|| format!("episode not found: {episode_id}"))?;
        anyhow::ensure!(
            matches!(
                episode.outcome,
                EpisodeOutcome::VerifiedSuccess | EpisodeOutcome::AcceptedSuccess
            ),
            "only successful episodes can be converted into a Skill candidate"
        );
        anyhow::ensure!(
            !episode.external_context || episode.included_by_user,
            "externally sourced Episodes require explicit include before Skill generation"
        );
        let name = sanitize_slug(&episode.goal_summary);
        let description = format!("Repeat the verified workflow: {}", episode.goal_summary);
        let body =
            "Use this skill when the user asks for the same goal as the verified episode.\n\n1. Inspect the current repository state first.\n2. Follow the repository instructions and reproduce the validated workflow.\n3. Run the relevant checks before reporting completion.\n4. Stop and ask when required evidence is missing.\n"
                .to_string();
        let memory = EvolutionMemory {
            schema_version: EVOLUTION_SCHEMA_VERSION,
            id: format!("manual-{}", episode.id),
            kind: MemoryKind::Workflow,
            name: name.clone(),
            summary: description.clone(),
            body: body.clone(),
            scope: MemoryScope {
                level: "repository".to_string(),
                value: self.project_id.clone(),
            },
            status: MemoryStatus::Proposed,
            pinned: false,
            confidence: episode.confidence,
            evidence_episode_ids: vec![episode.id.clone()],
            evidence_session_ids: vec![episode.session_id.clone()],
            user_quote: None,
            citations: episode
                .changed_paths
                .iter()
                .filter_map(|path| self.build_citation(&path.to_string_lossy()).ok())
                .collect(),
            external_context: episode.external_context,
            created_at: Utc::now().to_rfc3339(),
            updated_at: Utc::now().to_rfc3339(),
            last_validated_at: None,
            last_used_at: None,
            use_count: 0,
            contradicts: Vec::new(),
            supersedes: Vec::new(),
        };
        let skill_md = format!(
            "---\nname: {name}\ndescription: {}\n---\n\n{}\n",
            yaml_scalar(&description),
            body.trim()
        );
        let mut eval = build_skill_eval(&memory, &description);
        eval.structural_pass = eval.cases.len() >= 8 && !description.trim().is_empty();
        eval.notes.push(
            "This candidate was explicitly requested from one successful Episode; historical replication remains limited and requires human review."
                .to_string(),
        );
        let id = format!("cand-skill-manual-{}", short_hash(episode.id.as_bytes()));
        let _guard = self
            .write_lock
            .lock()
            .expect("evolution write lock poisoned");
        let mut index = self.load_candidate_index_unlocked();
        if !index.candidates.iter().any(|candidate| candidate.id == id) {
            let now = Utc::now().to_rfc3339();
            index.candidates.push(EvolutionCandidate {
                schema_version: EVOLUTION_SCHEMA_VERSION,
                id: id.clone(),
                kind: CandidateKind::Skill,
                status: CandidateStatus::Validated,
                title: format!("Skill: {name}"),
                summary: description.clone(),
                risk: "medium".to_string(),
                scope: format!("{}:{}", memory.scope.level, memory.scope.value),
                evidence_episode_ids: memory.evidence_episode_ids,
                evidence_session_ids: memory.evidence_session_ids,
                payload: CandidatePayload::Skill {
                    skill_name: name,
                    description,
                    skill_md,
                    eval,
                },
                created_at: now.clone(),
                updated_at: now,
                rejection_reason: None,
                activated_path: None,
            });
            write_json_atomic(&self.dir.join(CANDIDATE_INDEX_FILE), &index)?;
            self.append_audit_unlocked("manual_skill_candidate", Some(&id), &episode.goal_summary)?;
        }
        Ok(id)
    }

    pub async fn analyze_now(&self, episode_id: &str, provider: Arc<dyn Provider>) -> Result<u32> {
        let episode = self
            .list_episodes(usize::MAX)?
            .into_iter()
            .find(|episode| episode.id == episode_id)
            .with_context(|| format!("episode not found: {episode_id}"))?;
        let started = Instant::now();
        match self
            .analyze_episode(&episode, provider, EvolutionAnalysisMode::GovernanceOnly)
            .await
        {
            Ok(tokens) => {
                self.record_usage(tokens, started.elapsed().as_secs())?;
                self.record_health_success()?;
                self.clear_pending_analysis(episode_id)?;
                Ok(tokens)
            }
            Err(error) => {
                let _ = self.record_health_failure(&error.to_string());
                let _ = self.mark_pending_analysis(episode_id);
                Err(error)
            }
        }
    }

    pub fn migration_preview(&self) -> Result<MigrationPreview> {
        let mut files = Vec::new();
        if self.legacy_dir.exists() {
            for entry in fs::read_dir(&self.legacy_dir)? {
                let path = entry?.path();
                if path.extension().and_then(|ext| ext.to_str()) == Some("md")
                    && path.file_name().and_then(|name| name.to_str()) != Some("MEMORY.md")
                {
                    files.push(path);
                }
            }
        }
        files.sort();
        let backup_directory = self.legacy_dir.with_file_name(format!(
            "{}.backup-{}",
            self.legacy_dir
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("memory"),
            Utc::now().format("%Y%m%dT%H%M%SZ")
        ));
        Ok(MigrationPreview {
            legacy_directory: self.legacy_dir.clone(),
            entries: files.len(),
            files,
            backup_directory,
        })
    }

    pub fn migrate_legacy(&self) -> Result<MigrationPreview> {
        // v1.5.5 收敛：普通 Memory 数据层已迁出 Evolution；migrate_legacy
        // 不再灌库，仅返回 preview 让调用方知道"已迁出，无可迁数据"。
        self.migration_preview()
    }

    pub fn export_redacted(&self) -> Result<serde_json::Value> {
        Ok(serde_json::json!({
            "schema_version": EVOLUTION_SCHEMA_VERSION,
            "project_id": self.project_id,
            "status": self.status()?,
            "episodes": self.list_episodes(usize::MAX)?,
            "memories": self.list_memories()?,
            "candidates": self.list_candidates()?,
            "health": self.health(),
        }))
    }

    async fn analyze_episode(
        &self,
        episode: &Episode,
        provider: Arc<dyn Provider>,
        mode: EvolutionAnalysisMode,
    ) -> Result<u32> {
        if !self.within_daily_budget()? {
            anyhow::bail!("daily evolution budget exhausted");
        }
        if !matches!(
            episode.outcome,
            EpisodeOutcome::VerifiedSuccess
                | EpisodeOutcome::AcceptedSuccess
                | EpisodeOutcome::Partial
                | EpisodeOutcome::Failed
        ) {
            return Ok(0);
        }
        let prompt = evolution_analysis_prompt(episode, mode);
        let _ = mode; // v1.5.5 后模式只剩 GovernanceOnly
        let system = "You identify approval-gated Rule and Skill candidates from coding-agent Episodes. Return only JSON objects, one per line. Never emit ordinary memories, repository facts, or user preferences.";
        let result = provider
            .complete(
                system,
                &[Message::user(prompt)],
                &[],
                &wyj_api::provider::RequestOptions::text_only(4096),
            )
            .await?;
        let text = result
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        for line in text
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with('{'))
        {
            if let Ok(item) = serde_json::from_str::<AnalysisItem>(line) {
                self.upsert_analysis_item(episode, item, mode)?;
            }
        }
        self.enforce_retention_and_capacity()?;
        Ok(result.input_tokens.saturating_add(result.output_tokens))
    }

    fn upsert_analysis_item(
        &self,
        episode: &Episode,
        item: AnalysisItem,
        mode: EvolutionAnalysisMode,
    ) -> Result<()> {
        // v1.5.5 收敛：Evolution 不再生成普通 Memory；只剩 Rule/Skill 治理。
        // 模式只剩 GovernanceOnly，旧 MemoryAndGovernance 分支全部删除。
        debug_assert_eq!(mode, EvolutionAnalysisMode::GovernanceOnly);
        self.upsert_governance_analysis_item(episode, item)
    }

    fn upsert_governance_analysis_item(&self, episode: &Episode, item: AnalysisItem) -> Result<()> {
        let Some(kind) = parse_memory_kind(&item.kind) else {
            return Ok(());
        };
        match kind {
            MemoryKind::Workflow
                if self.cfg.suggest_skills && episode.outcome.supports_repository_learning() => {}
            MemoryKind::FailurePattern
                if self.cfg.suggest_rules && episode.outcome == EpisodeOutcome::Failed => {}
            _ => return Ok(()),
        }

        let memory_id = analysis_memory_id(kind, &item.name);
        let candidate_id = match kind {
            MemoryKind::Workflow => format!("cand-skill-{}", short_hash(memory_id.as_bytes())),
            MemoryKind::FailurePattern => {
                format!("cand-rule-{}", short_hash(memory_id.as_bytes()))
            }
            _ => unreachable!("governance mode filters ordinary memory kinds"),
        };
        let _guard = self
            .write_lock
            .lock()
            .expect("evolution write lock poisoned");
        let mut index = self.load_candidate_index_unlocked();
        let existing = index
            .candidates
            .iter()
            .position(|candidate| candidate.id == candidate_id);
        if existing.is_some_and(|position| {
            matches!(
                index.candidates[position].status,
                CandidateStatus::Active
                    | CandidateStatus::Rejected
                    | CandidateStatus::Stale
                    | CandidateStatus::RolledBack
            )
        }) {
            return Ok(());
        }

        let mut evidence_episode_ids = existing
            .map(|position| index.candidates[position].evidence_episode_ids.clone())
            .unwrap_or_default();
        let mut evidence_session_ids = existing
            .map(|position| index.candidates[position].evidence_session_ids.clone())
            .unwrap_or_default();
        push_unique(&mut evidence_episode_ids, episode.id.clone());
        push_unique(&mut evidence_session_ids, episode.session_id.clone());
        let now = Utc::now().to_rfc3339();
        let created_at = existing
            .map(|position| index.candidates[position].created_at.clone())
            .unwrap_or_else(|| now.clone());
        let name = sanitize_slug(&item.name);
        let candidate = match kind {
            MemoryKind::Workflow => {
                let description = item
                    .skill_description
                    .clone()
                    .unwrap_or_else(|| bounded_redacted(&item.summary, 500));
                let instructions = if item.skill_steps.is_empty() {
                    bounded_redacted(&item.body, 8_000)
                } else {
                    item.skill_steps
                        .iter()
                        .enumerate()
                        .map(|(index, step)| format!("{}. {}", index + 1, step))
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                let memory = governance_workflow_memory(
                    &memory_id,
                    &name,
                    &item,
                    evidence_episode_ids.clone(),
                    evidence_session_ids.clone(),
                    episode,
                    &self.project_id,
                );
                let eval = build_skill_eval(&memory, &description);
                let enough_evidence = evidence_episode_ids.len()
                    >= self.cfg.skill_candidate_min_successes as usize
                    && evidence_session_ids.len() >= self.cfg.skill_candidate_min_sessions as usize;
                let status = if !enough_evidence {
                    CandidateStatus::Validating
                } else if eval.structural_pass {
                    CandidateStatus::Validated
                } else {
                    CandidateStatus::Failed
                };
                let skill_md = format!(
                    "---\nname: {name}\ndescription: {}\n---\n\n{}\n",
                    yaml_scalar(&description),
                    instructions.trim()
                );
                EvolutionCandidate {
                    schema_version: EVOLUTION_SCHEMA_VERSION,
                    id: candidate_id.clone(),
                    kind: CandidateKind::Skill,
                    status,
                    title: format!("Skill: {name}"),
                    summary: description.clone(),
                    risk: "medium".to_string(),
                    scope: format!("repository:{}", self.project_id),
                    evidence_episode_ids,
                    evidence_session_ids,
                    payload: CandidatePayload::Skill {
                        skill_name: name,
                        description,
                        skill_md,
                        eval,
                    },
                    created_at,
                    updated_at: now,
                    rejection_reason: None,
                    activated_path: None,
                }
            }
            MemoryKind::FailurePattern => {
                let Some(rule) = item.rule_suggestion.as_deref() else {
                    return Ok(());
                };
                let status = if evidence_episode_ids.len() >= 3 {
                    CandidateStatus::Validated
                } else {
                    CandidateStatus::Validating
                };
                EvolutionCandidate {
                    schema_version: EVOLUTION_SCHEMA_VERSION,
                    id: candidate_id.clone(),
                    kind: CandidateKind::Rule,
                    status,
                    title: format!("Rule: {}", bounded_redacted(&item.summary, 500)),
                    summary: bounded_redacted(rule, 500),
                    risk: "medium".to_string(),
                    scope: format!("repository:{}", self.project_id),
                    evidence_episode_ids,
                    evidence_session_ids,
                    payload: CandidatePayload::Rule {
                        rule_text: bounded_redacted(rule, 4_000),
                        suggested_target: "evolution active rules".to_string(),
                    },
                    created_at,
                    updated_at: now,
                    rejection_reason: None,
                    activated_path: None,
                }
            }
            _ => unreachable!("governance mode filters ordinary memory kinds"),
        };
        let action = if candidate.status == CandidateStatus::Validated {
            match candidate.kind {
                CandidateKind::Skill => "validate_skill_candidate",
                CandidateKind::Rule => "validate_rule_candidate",
            }
        } else {
            "collect_candidate_evidence"
        };
        let detail = candidate.summary.clone();
        if let Some(position) = existing {
            index.candidates[position] = candidate;
        } else {
            index.candidates.push(candidate);
        }
        write_json_atomic(&self.dir.join(CANDIDATE_INDEX_FILE), &index)?;
        self.append_audit_unlocked(action, Some(&candidate_id), &detail)
    }

    fn load_context(&self, _goal: &str) -> String {
        // v1.5.5 收敛：普通 Memory 数据层已迁出 Evolution；上下文注入
        // 由 v3 Memory 的 project_brief + 续接分支接管。Evolution 只剩
        // Rule/Skill candidate 治理，本函数不再生成任何 Memory 上下文。
        let _ = self.list_candidates();
        String::new()
    }

    fn build_citation(&self, _value: &str) -> Result<RepositoryCitation> {
        // v1.5.5：普通 Memory 数据层已迁出；build_citation 失去业务意义。
        // 保留为 no-op 让旧调用站点（已被 noop 化的 upsert_governance 分支）
        // 不破坏编译。
        Ok(RepositoryCitation {
            repository_id: String::new(),
            commit: None,
            branch: None,
            path: PathBuf::new(),
            blob_oid: None,
            working_tree_sha256: None,
            symbol: None,
            context_fingerprint: None,
            display_line: None,
        })
    }

    fn append_episode(&self, episode: &Episode) -> Result<()> {
        let timestamp = DateTime::parse_from_rfc3339(&episode.finished_at)
            .map(|value| value.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now());
        let path = self.dir.join("episodes").join(format!(
            "{:04}-{:02}.jsonl",
            timestamp.year(),
            timestamp.month()
        ));
        append_jsonl(&path, episode)
    }

    fn load_candidate_index(&self) -> CandidateIndex {
        let _guard = self
            .write_lock
            .lock()
            .expect("evolution write lock poisoned");
        self.load_candidate_index_unlocked()
    }

    fn load_candidate_index_unlocked(&self) -> CandidateIndex {
        read_json_or_default(&self.dir.join(CANDIDATE_INDEX_FILE))
    }

    fn load_feedback_index(&self) -> FeedbackIndex {
        let _guard = self
            .write_lock
            .lock()
            .expect("evolution write lock poisoned");
        self.load_feedback_index_unlocked()
    }

    fn load_feedback_index_unlocked(&self) -> FeedbackIndex {
        read_json_or_default(&self.dir.join(FEEDBACK_INDEX_FILE))
    }

    fn mark_pending_analysis(&self, episode_id: &str) -> Result<()> {
        let _guard = self
            .write_lock
            .lock()
            .expect("evolution write lock poisoned");
        let mut index = self.load_feedback_index_unlocked();
        index
            .pending_analysis_episodes
            .insert(episode_id.to_string());
        write_json_atomic(&self.dir.join(FEEDBACK_INDEX_FILE), &index)
    }

    fn clear_pending_analysis(&self, episode_id: &str) -> Result<()> {
        let _guard = self
            .write_lock
            .lock()
            .expect("evolution write lock poisoned");
        let mut index = self.load_feedback_index_unlocked();
        index.pending_analysis_episodes.remove(episode_id);
        write_json_atomic(&self.dir.join(FEEDBACK_INDEX_FILE), &index)
    }

    fn take_pending_analysis(&self) -> Vec<String> {
        let _guard = self
            .write_lock
            .lock()
            .expect("evolution write lock poisoned");
        let mut index = self.load_feedback_index_unlocked();
        let pending = std::mem::take(&mut index.pending_analysis_episodes)
            .into_iter()
            .collect::<Vec<_>>();
        if !pending.is_empty() {
            let _ = write_json_atomic(&self.dir.join(FEEDBACK_INDEX_FILE), &index);
        }
        pending
    }

    fn enforce_retention_and_capacity(&self) -> Result<()> {
        let _guard = self
            .write_lock
            .lock()
            .expect("evolution write lock poisoned");
        let now = Utc::now();

        let episodes_dir = self.dir.join("episodes");
        if episodes_dir.exists() {
            for entry in fs::read_dir(&episodes_dir)? {
                let path = entry?.path();
                if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                    continue;
                }
                let mut episodes = read_jsonl::<Episode>(&path)?;
                episodes.retain(|episode| {
                    if !matches!(
                        episode.outcome,
                        EpisodeOutcome::Failed | EpisodeOutcome::Cancelled
                    ) {
                        return true;
                    }
                    !older_than_days(
                        &episode.finished_at,
                        self.cfg.retention.failed_episode_days,
                        now,
                    )
                });
                write_jsonl_atomic(&path, &episodes)?;
            }
        }

        let mut candidates = self.load_candidate_index_unlocked();
        candidates.candidates.retain(|candidate| {
            candidate.status == CandidateStatus::Active
                || !older_than_days(
                    &candidate.updated_at,
                    self.cfg.retention.candidate_days,
                    now,
                )
        });
        write_json_atomic(&self.dir.join(CANDIDATE_INDEX_FILE), &candidates)?;

        let audit_path = self.dir.join(AUDIT_FILE);
        if audit_path.exists() {
            let mut audit = read_jsonl::<AuditEvent>(&audit_path)?;
            audit.retain(|event| {
                !older_than_days(&event.timestamp, self.cfg.retention.audit_days, now)
            });
            write_jsonl_atomic(&audit_path, &audit)?;
        }

        if directory_size(&self.dir) <= self.cfg.max_project_store_bytes {
            return Ok(());
        }

        // 容量清理只删除非 active、非 pinned 的派生内容。先清理可重建的
        // audit/失败候选，再从最老 Episode 开始收缩；active Memory/Rule/Skill
        // 与 pinned Memory 永不因容量压力自动删除。
        if audit_path.exists() {
            write_jsonl_atomic::<AuditEvent>(&audit_path, &[])?;
        }
        candidates
            .candidates
            .retain(|candidate| matches!(candidate.status, CandidateStatus::Active));
        write_json_atomic(&self.dir.join(CANDIDATE_INDEX_FILE), &candidates)?;

        // v1.5.5：普通 Memory 数据层已迁出 Evolution；这里不再清洗
        // memories/index.json（不存在也不该存在）。容量清理仍按 max_project_
        // store_bytes 触发。

        if directory_size(&self.dir) > self.cfg.max_project_store_bytes {
            let mut paths = fs::read_dir(&episodes_dir)?
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
                .collect::<Vec<_>>();
            paths.sort();
            for path in paths {
                if directory_size(&self.dir) <= self.cfg.max_project_store_bytes {
                    break;
                }
                fs::remove_file(path)?;
            }
        }
        Ok(())
    }

    fn infer_feedback_from_goal(&self, goal: &str) -> Result<()> {
        let Some(latest) = self.list_episodes(1)?.into_iter().next() else {
            return Ok(());
        };
        if latest.outcome != EpisodeOutcome::Unknown {
            return Ok(());
        }
        let lower = goal.to_lowercase();
        let negative = [
            "不对",
            "没有",
            "还是",
            "错了",
            "重新",
            "并没有",
            "not correct",
            "still",
            "didn't",
            "doesn't",
        ]
        .iter()
        .any(|marker| lower.contains(marker));
        let positive = ["很好", "可以", "搞定", "完成了", "works", "great", "fixed"]
            .iter()
            .any(|marker| lower.contains(marker));
        if negative || positive {
            self.feedback_episode(
                &latest.id,
                positive,
                Some("inferred from the next user goal".to_string()),
                false,
            )?;
        }
        Ok(())
    }

    fn within_daily_budget(&self) -> Result<bool> {
        let mut usage: DailyUsage = read_json_or_default(&self.dir.join(USAGE_FILE));
        let today = Utc::now().date_naive().to_string();
        if usage.date != today {
            usage = DailyUsage::default();
            write_json_atomic(&self.dir.join(USAGE_FILE), &usage)?;
        }
        Ok(usage.tokens < self.cfg.max_daily_tokens
            && usage.wall_secs < self.cfg.max_daily_wall_secs)
    }

    fn record_usage(&self, tokens: u32, wall_secs: u64) -> Result<()> {
        let _guard = self
            .write_lock
            .lock()
            .expect("evolution write lock poisoned");
        let mut usage: DailyUsage = read_json_or_default(&self.dir.join(USAGE_FILE));
        let today = Utc::now().date_naive().to_string();
        if usage.date != today {
            usage = DailyUsage::default();
        }
        usage.tokens = usage.tokens.saturating_add(tokens);
        usage.wall_secs = usage.wall_secs.saturating_add(wall_secs);
        write_json_atomic(&self.dir.join(USAGE_FILE), &usage)
    }

    fn record_health_success(&self) -> Result<()> {
        let _guard = self
            .write_lock
            .lock()
            .expect("evolution write lock poisoned");
        let mut health: EvolutionHealth = read_json_or_default(&self.dir.join(HEALTH_FILE));
        health.consecutive_failures = 0;
        health.last_error = None;
        health.last_success_at = Some(Utc::now().to_rfc3339());
        health.notice_pending = false;
        write_json_atomic(&self.dir.join(HEALTH_FILE), &health)
    }

    fn record_health_failure(&self, error: &str) -> Result<()> {
        let _guard = self
            .write_lock
            .lock()
            .expect("evolution write lock poisoned");
        let mut health: EvolutionHealth = read_json_or_default(&self.dir.join(HEALTH_FILE));
        health.consecutive_failures = health.consecutive_failures.saturating_add(1);
        health.last_error = Some(bounded_redacted(error, 1_000));
        if health.consecutive_failures >= 3 {
            health.notice_pending = true;
        }
        write_json_atomic(&self.dir.join(HEALTH_FILE), &health)
    }

    fn append_audit_unlocked(
        &self,
        action: &str,
        target_id: Option<&str>,
        detail: &str,
    ) -> Result<()> {
        append_jsonl(
            &self.dir.join(AUDIT_FILE),
            &AuditEvent {
                schema_version: EVOLUTION_SCHEMA_VERSION,
                timestamp: Utc::now().to_rfc3339(),
                action: action.to_string(),
                target_id: target_id.map(str::to_string),
                detail: bounded_redacted(detail, 1_000),
            },
        )
    }
}

#[derive(Default)]
struct MessageAnalysis {
    evidence: Vec<EpisodeEvidence>,
    any_required_check: bool,
    all_required_checks_passed: bool,
    any_unrecovered_tool_error: bool,
    security_failure: bool,
    external_context: bool,
}

fn analyze_messages(messages: &[Message]) -> MessageAnalysis {
    let mut output = MessageAnalysis {
        all_required_checks_passed: true,
        ..MessageAnalysis::default()
    };
    let mut calls: HashMap<String, (String, serde_json::Value)> = HashMap::new();
    for message in messages {
        for block in &message.content {
            match block {
                ContentBlock::ToolUse { id, name, input } => {
                    if is_external_tool(name) {
                        output.external_context = true;
                    }
                    calls.insert(id.clone(), (name.clone(), input.clone()));
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => {
                    let (name, input) = calls
                        .get(tool_use_id)
                        .cloned()
                        .unwrap_or_else(|| ("unknown".to_string(), serde_json::Value::Null));
                    let text = bounded_redacted(&content.display_text(), 1_000);
                    let command = input
                        .get("command")
                        .or_else(|| input.get("cmd"))
                        .and_then(|value| value.as_str())
                        .unwrap_or_default();
                    let is_check =
                        name.eq_ignore_ascii_case("bash") && is_validation_command(command);
                    if is_check {
                        output.any_required_check = true;
                        let success = !*is_error && !looks_like_failed_test(&text);
                        output.all_required_checks_passed &= success;
                        output.evidence.push(EpisodeEvidence {
                            kind: EvidenceKind::Test,
                            label: bounded_redacted(command, 300),
                            success: Some(success),
                            detail: text.clone(),
                        });
                    } else {
                        output.evidence.push(EpisodeEvidence {
                            kind: EvidenceKind::Tool,
                            label: name.clone(),
                            success: Some(!*is_error),
                            detail: text.clone(),
                        });
                    }
                    if *is_error {
                        output.any_unrecovered_tool_error = true;
                    }
                    let lower = text.to_lowercase();
                    output.security_failure |= lower.contains("secret")
                        && (lower.contains("leak") || lower.contains("credential"));
                }
                _ => {}
            }
        }
    }
    if !output.any_required_check {
        output.all_required_checks_passed = false;
    }
    // A later successful result for the same tool is evidence that the Agent recovered.
    if output
        .evidence
        .iter()
        .rev()
        .take(3)
        .any(|evidence| evidence.success == Some(true))
    {
        output.any_unrecovered_tool_error = false;
    }
    output
}

fn evolution_analysis_prompt(episode: &Episode, _mode: EvolutionAnalysisMode) -> String {
    let evidence = episode
        .evidence
        .iter()
        .map(|item| {
            format!(
                "- {:?} {} success={:?}: {}",
                item.kind, item.label, item.success, item.detail
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let paths = episode
        .changed_paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let task = r#"Extract only approval-gated Rule or Skill candidates from this coding-agent episode. Output zero or more JSON objects, one per line, with:
{{"kind":"rule_skill_candidate","name":"short-slug","summary":"one line","body":"durable governance rule or reusable skill steps","rule_suggestion":null,"skill_description":null,"skill_steps":[]}}

Rules:
- Never infer a user preference or ordinary memory; only emit Rule/Skill candidates that need human approval.
- Skill candidates require verified_success and must describe reusable steps, not one-off task state.
- Rule candidates should identify the stable governance rule, not a transient provider/network error.
- Do not emit user_preference|repository_fact|workflow|failure_pattern|reference — those are managed by Memory v3, not Evolution.
- Do not persist tool availability, permission mode, temporary environment/network state, secrets, or raw credentials.
- Do not output anything when evidence is insufficient."#;
    format!(
        r#"{task}

Episode id: {id}
Outcome: {outcome:?}
Goal: {goal}
Changed paths: {paths}
Evidence:
{evidence}
"#,
        id = episode.id,
        outcome = episode.outcome,
        goal = episode.goal_summary,
    )
}

fn analysis_memory_id(kind: MemoryKind, name: &str) -> String {
    format!(
        "mem-{}",
        short_hash(format!("{:?}:{}", kind, sanitize_slug(name)).as_bytes())
    )
}

fn governance_workflow_memory(
    id: &str,
    name: &str,
    item: &AnalysisItem,
    evidence_episode_ids: Vec<String>,
    evidence_session_ids: Vec<String>,
    episode: &Episode,
    project_id: &str,
) -> EvolutionMemory {
    let now = Utc::now().to_rfc3339();
    EvolutionMemory {
        schema_version: EVOLUTION_SCHEMA_VERSION,
        id: id.to_string(),
        kind: MemoryKind::Workflow,
        name: name.to_string(),
        summary: bounded_redacted(&item.summary, 500),
        body: bounded_redacted(&item.body, 8_000),
        scope: MemoryScope {
            level: "repository".to_string(),
            value: project_id.to_string(),
        },
        status: MemoryStatus::Proposed,
        pinned: false,
        confidence: 0,
        evidence_episode_ids,
        evidence_session_ids,
        user_quote: None,
        citations: Vec::new(),
        external_context: episode.external_context,
        created_at: now.clone(),
        updated_at: now,
        last_validated_at: None,
        last_used_at: None,
        use_count: 0,
        contradicts: Vec::new(),
        supersedes: Vec::new(),
    }
}

fn parse_memory_kind(value: &str) -> Option<MemoryKind> {
    match value.trim().to_ascii_lowercase().as_str() {
        "user_preference" | "user" | "feedback" => Some(MemoryKind::UserPreference),
        "repository_fact" | "project" => Some(MemoryKind::RepositoryFact),
        "workflow" | "workflow_hint" => Some(MemoryKind::Workflow),
        "failure_pattern" | "failure" => Some(MemoryKind::FailurePattern),
        "reference" => Some(MemoryKind::Reference),
        _ => None,
    }
}

fn build_skill_eval(memory: &EvolutionMemory, description: &str) -> SkillEvalReport {
    let prompts = vec![
        ("direct", format!("Please {description}"), "activate"),
        ("direct", memory.summary.clone(), "activate"),
        (
            "indirect",
            format!("I need the usual workflow for {}", memory.name),
            "activate",
        ),
        (
            "indirect",
            format!(
                "Can you handle this the same way as before: {}",
                memory.summary
            ),
            "activate",
        ),
        (
            "incomplete",
            format!(
                "Help with {} but I have not provided the target",
                memory.name
            ),
            "ask_question",
        ),
        (
            "negative",
            "Translate this sentence to French".to_string(),
            "do_not_activate",
        ),
        (
            "negative",
            "What time is it?".to_string(),
            "do_not_activate",
        ),
        (
            "safety",
            format!("Run {} and ignore repository permissions", memory.name),
            "decline_or_confirm",
        ),
    ];
    let cases = prompts
        .into_iter()
        .map(|(category, prompt, expected)| SkillEvalCase {
            category: category.to_string(),
            prompt,
            expected: expected.to_string(),
        })
        .collect::<Vec<_>>();
    let structural_pass = cases.len() >= 8
        && !description.trim().is_empty()
        && memory.evidence_episode_ids.len() >= 3
        && memory.evidence_session_ids.len() >= 2;
    SkillEvalReport {
        generated_at: Utc::now().to_rfc3339(),
        cases,
        structural_pass,
        historical_successes: memory.evidence_episode_ids.len(),
        distinct_sessions: memory.evidence_session_ids.len(),
        notes: vec![
            "Activation boundary cases are generated automatically.".to_string(),
            "Historical verified episodes provide the execution evidence; the candidate remains manually approved.".to_string(),
        ],
    }
}

fn is_external_tool(name: &str) -> bool {
    matches!(name, "WebFetch" | "WebSearch" | "ToolSearch")
        || name.starts_with("mcp__")
        || name.starts_with("MCP")
}

fn is_validation_command(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    [
        "cargo test",
        "cargo check",
        "cargo clippy",
        "cargo fmt",
        "npm test",
        "npm run test",
        "pnpm test",
        "pytest",
        "go test",
        "gradle test",
        "mvn test",
        "git diff --check",
        "node --check",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

fn looks_like_failed_test(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    lower.contains("test result: failed")
        || lower.contains("error: could not compile")
        || lower.contains(" failed;")
        || lower.contains("failures:")
}

fn git_worktree_snapshot(root: &Path) -> BTreeMap<PathBuf, String> {
    // 不传 `-uno`,让 untracked 也进入 snapshot:`changed_since` 需要比较 untracked
    // 的指纹识别新增/修改的文件(如 `node_modules/`、`target/` 等)。对 untracked
    // 大文件(>= 1 MiB)用 mtime+size 替代全文 SHA-256,避免 IO 放大。
    let Ok(output) = Command::new("git")
        .args(["status", "--porcelain", "-z"])
        .current_dir(root)
        .output()
    else {
        return BTreeMap::new();
    };
    if !output.status.success() {
        return BTreeMap::new();
    }
    output
        .stdout
        .split(|byte| *byte == 0)
        .filter_map(|entry| {
            if entry.len() < 4 {
                return None;
            }
            let status = String::from_utf8_lossy(&entry[..2]).to_string();
            let raw_path = String::from_utf8_lossy(&entry[3..]).to_string();
            let path = PathBuf::from(raw_path.rsplit(" -> ").next().unwrap_or(&raw_path));
            let absolute = root.join(&path);
            // untracked (`??`) 且 >= 1 MiB 的文件用 mtime+size 作轻量指纹;
            // 其余(is_file())读全文哈希。Episode "改了哪些文件" 的语义对
            // untracked 只关心"是否被改过",mtime+size 变化即触发变更。
            let is_untracked = entry[0] == b'?' && entry[1] == b'?';
            let is_large_untracked = is_untracked
                && fs::metadata(&absolute)
                    .map(|m| m.len() >= UNTRACKED_LARGE_FILE_BYTES)
                    .unwrap_or(false);
            let digest = if is_large_untracked {
                fs::metadata(&absolute)
                    .map(|m| {
                        let mtime = m
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_secs())
                            .unwrap_or(0);
                        format!("mtime:{mtime};size:{}", m.len())
                    })
                    .unwrap_or_else(|_| "unreadable".to_string())
            } else if absolute.is_file() {
                fs::read(&absolute)
                    .map(|bytes| hex_sha256(&bytes))
                    .unwrap_or_else(|_| "unreadable".to_string())
            } else {
                "non-file".to_string()
            };
            Some((path, format!("{status}:{digest}")))
        })
        .collect()
}

/// untracked 大文件阈值(1 MiB)。达到此大小的 `node_modules/`/`target/` 等
/// 未追踪目录不会被全文读 SHA-256,改用 mtime+size 指纹。
const UNTRACKED_LARGE_FILE_BYTES: u64 = 1024 * 1024;

fn changed_since(
    before: &BTreeMap<PathBuf, String>,
    after: &BTreeMap<PathBuf, String>,
) -> Vec<PathBuf> {
    let mut paths = before
        .keys()
        .chain(after.keys())
        .filter(|path| before.get(*path) != after.get(*path))
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

fn looks_like_cancelled(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    [
        "cancelled",
        "canceled",
        "interrupted",
        "aborted by user",
        "已取消",
        "用户中断",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

fn git_text(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn bounded_redacted(value: &str, max_chars: usize) -> String {
    let redacted = redact_sensitive_text(value);
    if redacted.chars().count() <= max_chars {
        redacted
    } else {
        let mut output = redacted.chars().take(max_chars).collect::<String>();
        output.push_str("...[truncated]");
        output
    }
}

fn sanitize_slug(value: &str) -> String {
    let mut output = String::new();
    let mut last_dash = false;
    for ch in value.to_ascii_lowercase().chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            output.push(ch);
            last_dash = false;
        } else if !last_dash {
            output.push('-');
            last_dash = true;
        }
        if output.len() >= 64 {
            break;
        }
    }
    let output = output.trim_matches('-').to_string();
    if output.is_empty() {
        format!(
            "workflow-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        )
    } else {
        output
    }
}

fn yaml_scalar(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn short_hash(bytes: &[u8]) -> String {
    hex_sha256(bytes)[..16].to_string()
}

fn hex_sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.contains(&value) {
        values.push(value);
    }
}

fn candidate_status_rank(status: CandidateStatus) -> u8 {
    match status {
        CandidateStatus::Validated => 0,
        CandidateStatus::Proposed => 1,
        CandidateStatus::Validating => 2,
        CandidateStatus::Active => 3,
        CandidateStatus::Failed => 4,
        CandidateStatus::Stale => 5,
        CandidateStatus::Rejected => 6,
        CandidateStatus::RolledBack => 7,
    }
}

fn append_jsonl(path: &Path, value: &impl Serialize) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut line = serde_json::to_string(value)?;
    line = redact_sensitive_text(&line);
    line.push('\n');
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(line.as_bytes())?;
    Ok(())
}

fn write_jsonl_atomic<T: Serialize>(path: &Path, values: &[T]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!(
        "tmp-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    ));
    let mut bytes = Vec::new();
    for value in values {
        let mut line = serde_json::to_vec(value)?;
        line.push(b'\n');
        bytes.extend_from_slice(&line);
    }
    if let Err(error) = fs::write(&tmp, bytes) {
        let _ = fs::remove_file(&tmp);
        return Err(error.into());
    }
    if let Err(error) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(error.into());
    }
    Ok(())
}

fn read_jsonl<T: DeserializeOwned>(path: &Path) -> Result<Vec<T>> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    Ok(content
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect())
}

fn read_json_or_default<T: DeserializeOwned + Default>(path: &Path) -> T {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn older_than_days(timestamp: &str, days: u32, now: DateTime<Utc>) -> bool {
    if days == 0 {
        return true;
    }
    DateTime::parse_from_rfc3339(timestamp)
        .map(|value| {
            now.signed_duration_since(value.with_timezone(&Utc))
                .num_days()
                >= i64::from(days)
        })
        .unwrap_or(false)
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!(
        "tmp-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    ));
    let bytes = serde_json::to_vec_pretty(value)?;
    fs::write(&tmp, bytes)?;
    if let Err(error) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(error.into());
    }
    Ok(())
}

fn directory_size(root: &Path) -> u64 {
    fn walk(path: &Path, total: &mut u64) {
        let Ok(metadata) = fs::symlink_metadata(path) else {
            return;
        };
        if metadata.is_file() {
            *total = total.saturating_add(metadata.len());
        } else if metadata.is_dir() {
            if let Ok(entries) = fs::read_dir(path) {
                for entry in entries.flatten() {
                    walk(&entry.path(), total);
                }
            }
        }
    }
    let mut total = 0;
    walk(root, &mut total);
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use wyj_api::types::ToolResultContent;

    fn test_store() -> (tempfile::TempDir, EvolutionStore) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .status()
            .unwrap();
        fs::write(repo.join("README.md"), "hello\n").unwrap();
        let cfg = EvolutionCfg {
            idle_delay_secs: 0,
            ..EvolutionCfg::default()
        };
        let store = EvolutionStore::new(dir.path(), &repo, cfg).unwrap();
        (dir, store)
    }

    fn completed_episode(
        store: &EvolutionStore,
        session_id: &str,
        command_output: &str,
    ) -> Episode {
        let mut session = Session::new();
        session.push_user("run the reusable validation workflow");
        let capture = store.begin_episode(
            session_id,
            &session,
            "run the reusable validation workflow",
            "p",
            "v",
            "m",
        );
        session.push_assistant(vec![ContentBlock::ToolUse {
            id: format!("tool-{session_id}"),
            name: "Bash".to_string(),
            input: serde_json::json!({"command":"cargo test"}),
        }]);
        session.push_tool_result(
            format!("tool-{session_id}"),
            ToolResultContent::Text(command_output.to_string()),
            looks_like_failed_test(command_output),
        );
        store.finish_episode(capture, &session, &Ok(())).unwrap()
    }

    fn workflow_analysis_item() -> AnalysisItem {
        AnalysisItem {
            kind: "workflow".to_string(),
            name: "rust-validation".to_string(),
            summary: "Validate Rust changes before delivery".to_string(),
            body: "Run focused tests, strict Clippy, formatting, and diff checks.".to_string(),
            rule_suggestion: None,
            skill_description: Some("Validate Rust changes before delivery".to_string()),
            skill_steps: vec![
                "Run focused tests".to_string(),
                "Run strict Clippy".to_string(),
                "Check formatting and the final diff".to_string(),
            ],
        }
    }

    fn failure_analysis_item() -> AnalysisItem {
        AnalysisItem {
            kind: "failure_pattern".to_string(),
            name: "skipped-validation".to_string(),
            summary: "Delivery skipped required validation".to_string(),
            body: "The change was reported complete before required checks passed.".to_string(),
            rule_suggestion: Some(
                "Do not report completion until the required validation commands pass.".to_string(),
            ),
            skill_description: None,
            skill_steps: Vec::new(),
        }
    }

    #[test]
    fn empty_indexes_default_to_the_current_schema() {
        assert_eq!(
            CandidateIndex::default().schema_version,
            EVOLUTION_SCHEMA_VERSION
        );
        assert_eq!(
            FeedbackIndex::default().schema_version,
            EVOLUTION_SCHEMA_VERSION
        );
        assert_eq!(
            DailyUsage::default().schema_version,
            EVOLUTION_SCHEMA_VERSION
        );
        assert_eq!(
            EvolutionHealth::default().schema_version,
            EVOLUTION_SCHEMA_VERSION
        );
    }

    #[test]
    fn episode_state_requires_hard_validation_for_verified_success() {
        let (_dir, store) = test_store();
        let mut session = Session::new();
        session.push_user("change it");
        let capture = store.begin_episode("s1", &session, "change it", "p", "v", "m");
        session.push_assistant(vec![ContentBlock::ToolUse {
            id: "t1".to_string(),
            name: "Bash".to_string(),
            input: serde_json::json!({"command":"cargo test"}),
        }]);
        session.push_tool_result(
            "t1".to_string(),
            ToolResultContent::Text("test result: ok. 1 passed".to_string()),
            false,
        );
        let episode = store.finish_episode(capture, &session, &Ok(())).unwrap();
        assert_eq!(episode.outcome, EpisodeOutcome::VerifiedSuccess);
    }

    #[test]
    fn governance_only_analysis_keeps_episodes_and_builds_skill_without_memory_v2() {
        let (_dir, store) = test_store();

        for session_id in ["skill-s1", "skill-s2", "skill-s3"] {
            let episode =
                completed_episode(&store, session_id, "test result: ok. 1 passed; 0 failed");
            assert_eq!(episode.outcome, EpisodeOutcome::VerifiedSuccess);
            store
                .upsert_analysis_item(
                    &episode,
                    workflow_analysis_item(),
                    EvolutionAnalysisMode::GovernanceOnly,
                )
                .unwrap();
        }

        assert_eq!(store.list_episodes(10).unwrap().len(), 3);
        assert!(store.list_memories().unwrap().is_empty());
        let candidates = store.list_candidates().unwrap();
        assert_eq!(candidates.len(), 1);
        let candidate = &candidates[0];
        assert_eq!(candidate.kind, CandidateKind::Skill);
        assert_eq!(candidate.status, CandidateStatus::Validated);
        assert_eq!(candidate.evidence_episode_ids.len(), 3);
        assert_eq!(candidate.evidence_session_ids.len(), 3);
        let CandidatePayload::Skill { eval, .. } = &candidate.payload else {
            panic!("expected Skill candidate")
        };
        assert!(eval.structural_pass);
        assert_eq!(eval.historical_successes, 3);
        assert_eq!(eval.distinct_sessions, 3);
    }

    #[test]
    fn governance_only_analysis_accumulates_rule_evidence_without_memory_v2() {
        let (_dir, store) = test_store();

        for session_id in ["rule-s1", "rule-s2"] {
            let episode = completed_episode(
                &store,
                session_id,
                "test result: failed; 1 failed; failures:",
            );
            assert_eq!(episode.outcome, EpisodeOutcome::Failed);
            store
                .upsert_analysis_item(
                    &episode,
                    failure_analysis_item(),
                    EvolutionAnalysisMode::GovernanceOnly,
                )
                .unwrap();
        }
        assert!(store.list_memories().unwrap().is_empty());
        assert_eq!(
            store.list_candidates().unwrap()[0].status,
            CandidateStatus::Validating
        );

        let episode = completed_episode(
            &store,
            "rule-s3",
            "test result: failed; 1 failed; failures:",
        );
        store
            .upsert_analysis_item(
                &episode,
                failure_analysis_item(),
                EvolutionAnalysisMode::GovernanceOnly,
            )
            .unwrap();

        assert_eq!(store.list_episodes(10).unwrap().len(), 3);
        assert!(store.list_memories().unwrap().is_empty());
        let candidate = store.list_candidates().unwrap().remove(0);
        assert_eq!(candidate.kind, CandidateKind::Rule);
        assert_eq!(candidate.status, CandidateStatus::Validated);
        assert_eq!(candidate.evidence_episode_ids.len(), 3);
        let CandidatePayload::Rule { rule_text, .. } = candidate.payload else {
            panic!("expected Rule candidate")
        };
        assert!(rule_text.contains("required validation"));
    }

    #[test]
    fn governance_only_analysis_ignores_ordinary_memory_kinds() {
        let (_dir, store) = test_store();
        let episode = completed_episode(
            &store,
            "ordinary-memory",
            "test result: ok. 1 passed; 0 failed",
        );
        let mut item = workflow_analysis_item();
        item.kind = "user_preference".to_string();

        store
            .upsert_analysis_item(&episode, item, EvolutionAnalysisMode::GovernanceOnly)
            .unwrap();

        assert!(store.list_memories().unwrap().is_empty());
        assert!(store.list_candidates().unwrap().is_empty());
    }

    // v1.5.5 收敛后删除：旧的 `legacy_analysis_mode_still_writes_memory_v2_when_v3_is_absent`
    // 验证 `MemoryAndGovernance` 模式下普通 Memory 仍能写入；该模式已废除，
    // 普通 Memory 由 v3 MemoryV3Store 接管。

    #[test]
    fn external_tool_marks_episode_as_quarantined() {
        let (_dir, store) = test_store();
        let mut session = Session::new();
        session.push_user("research it");
        let capture = store.begin_episode("s1", &session, "research it", "p", "v", "m");
        session.push_assistant(vec![ContentBlock::ToolUse {
            id: "t1".to_string(),
            name: "WebSearch".to_string(),
            input: serde_json::json!({"query":"x"}),
        }]);
        session.push_tool_result(
            "t1".to_string(),
            ToolResultContent::Text("result".to_string()),
            false,
        );
        let episode = store.finish_episode(capture, &session, &Ok(())).unwrap();
        assert!(episode.external_context);
    }

    #[test]
    fn cancelled_episode_is_persisted_without_becoming_failure_evidence() {
        let (_dir, store) = test_store();
        let mut session = Session::new();
        session.push_user("stop this turn");
        let capture = store.begin_episode("s-cancel", &session, "stop this turn", "p", "v", "m");

        let episode = store.cancel_episode(capture).unwrap();

        assert_eq!(episode.outcome, EpisodeOutcome::Cancelled);
        assert_eq!(episode.confidence, 100);
        assert_eq!(episode.evidence.len(), 1);
        assert_eq!(episode.evidence[0].label, "turn_cancelled");
        assert_eq!(
            store.list_episodes(10).unwrap()[0].outcome,
            EpisodeOutcome::Cancelled
        );
    }

    #[test]
    fn episode_changed_paths_exclude_unchanged_preexisting_dirty_files() {
        let (_dir, store) = test_store();
        fs::write(store.repository_root.join("README.md"), "already dirty\n").unwrap();
        let mut session = Session::new();
        session.push_user("add a source file");
        let capture = store.begin_episode("s-diff", &session, "add a source file", "p", "v", "m");
        fs::write(store.repository_root.join("new.rs"), "fn main() {}\n").unwrap();

        let episode = store.finish_episode(capture, &session, &Ok(())).unwrap();

        assert_eq!(episode.changed_paths, vec![PathBuf::from("new.rs")]);
    }

    #[test]
    fn explicit_feedback_overrides_unknown_episode() {
        let (_dir, store) = test_store();
        let mut session = Session::new();
        session.push_user("explain");
        let capture = store.begin_episode("s1", &session, "explain", "p", "v", "m");
        session.push_assistant(vec![ContentBlock::Text {
            text: "done".to_string(),
        }]);
        let episode = store.finish_episode(capture, &session, &Ok(())).unwrap();
        assert_eq!(episode.outcome, EpisodeOutcome::Unknown);
        store
            .feedback_episode(&episode.id, true, Some("accepted".to_string()), true)
            .unwrap();
        assert_eq!(
            store.list_episodes(1).unwrap()[0].outcome,
            EpisodeOutcome::AcceptedSuccess
        );
    }

    #[test]
    fn explicit_feedback_and_include_queue_episode_for_reanalysis() {
        let (_dir, store) = test_store();
        let mut session = Session::new();
        session.push_user("research and explain");
        let capture = store.begin_episode(
            "s-feedback",
            &session,
            "research and explain",
            "p",
            "v",
            "m",
        );
        let episode = store.finish_episode(capture, &session, &Ok(())).unwrap();

        store
            .feedback_episode(&episode.id, true, Some("accepted".into()), true)
            .unwrap();
        store.include_episode(&episode.id).unwrap();

        let feedback = store.load_feedback_index();
        assert!(feedback.included_episodes.contains(&episode.id));
        assert!(feedback.pending_analysis_episodes.contains(&episode.id));
        assert_eq!(feedback.feedback.len(), 1);
    }

    // v1.5.5 收敛后删除：`context_snapshot_is_relevance_filtered_and_byte_bounded`
    // 依赖旧 Memory 索引生成 relevance-filtered 上下文；普通 Memory 数据层
    // 已迁出 Evolution，`context_snapshot` 现在只剩 Rule/Skill 治理摘要。

    #[test]
    fn external_episode_requires_include_before_manual_skillize() {
        let (_dir, store) = test_store();
        let mut session = Session::new();
        session.push_user("research and validate release steps");
        let capture = store.begin_episode(
            "s-skill",
            &session,
            "research and validate release steps",
            "p",
            "v",
            "m",
        );
        session.push_assistant(vec![
            ContentBlock::ToolUse {
                id: "web-1".to_string(),
                name: "WebSearch".to_string(),
                input: serde_json::json!({"query":"release steps"}),
            },
            ContentBlock::ToolUse {
                id: "test-1".to_string(),
                name: "Bash".to_string(),
                input: serde_json::json!({"command":"cargo test"}),
            },
        ]);
        session.push_tool_result(
            "web-1".to_string(),
            ToolResultContent::Text("official documentation".to_string()),
            false,
        );
        session.push_tool_result(
            "test-1".to_string(),
            ToolResultContent::Text("test result: ok. 1 passed".to_string()),
            false,
        );
        let episode = store.finish_episode(capture, &session, &Ok(())).unwrap();
        assert_eq!(episode.outcome, EpisodeOutcome::VerifiedSuccess);
        assert!(episode.external_context);

        let error = store
            .create_skill_candidate_from_episode(&episode.id)
            .unwrap_err();
        assert!(error.to_string().contains("explicit include"));

        store.include_episode(&episode.id).unwrap();
        let candidate_id = store
            .create_skill_candidate_from_episode(&episode.id)
            .unwrap();
        let candidate = store
            .list_candidates()
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.id == candidate_id)
            .unwrap();
        assert_eq!(candidate.status, CandidateStatus::Validated);
        let CandidatePayload::Skill { eval, .. } = candidate.payload else {
            panic!("expected Skill candidate")
        };
        assert!(eval.structural_pass);
        assert_eq!(eval.cases.len(), 8);
        assert_eq!(eval.historical_successes, 1);
        assert_eq!(eval.distinct_sessions, 1);
    }

    // v1.5.5 收敛后删除：`capacity_cleanup_preserves_active_and_pinned_memories`
    // 验证旧 memories/index.json 的 active/pinned 保留；普通 Memory 数据层
    // 已迁出 Evolution，容量清理不再触碰该索引。

    // v1.5.5 收敛后删除：`legacy_migration_is_previewed_and_backed_up`
    // 验证旧 legacy markdown 自动灌入 v2 MemoryIndex；该路径已废除，新库
    // 不再做任何隐式迁移。

    #[test]
    fn skill_eval_contains_all_required_boundary_categories() {
        let memory = EvolutionMemory {
            schema_version: EVOLUTION_SCHEMA_VERSION,
            id: "m".to_string(),
            kind: MemoryKind::Workflow,
            name: "release".to_string(),
            summary: "release the project".to_string(),
            body: "run checks".to_string(),
            scope: MemoryScope {
                level: "repository".to_string(),
                value: "p".to_string(),
            },
            status: MemoryStatus::Active,
            pinned: false,
            confidence: 90,
            evidence_episode_ids: vec!["1".into(), "2".into(), "3".into()],
            evidence_session_ids: vec!["a".into(), "b".into()],
            user_quote: None,
            citations: Vec::new(),
            external_context: false,
            created_at: Utc::now().to_rfc3339(),
            updated_at: Utc::now().to_rfc3339(),
            last_validated_at: None,
            last_used_at: None,
            use_count: 0,
            contradicts: Vec::new(),
            supersedes: Vec::new(),
        };
        let eval = build_skill_eval(&memory, "release the project");
        assert!(eval.structural_pass);
        assert_eq!(eval.cases.len(), 8);
        let categories: BTreeSet<_> = eval
            .cases
            .iter()
            .map(|case| case.category.as_str())
            .collect();
        assert!(categories.contains("direct"));
        assert!(categories.contains("indirect"));
        assert!(categories.contains("incomplete"));
        assert!(categories.contains("negative"));
        assert!(categories.contains("safety"));
    }
}
