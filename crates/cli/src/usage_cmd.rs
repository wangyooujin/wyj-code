//! `wyj-code usage` —— 跨会话 token 消耗汇总。
//!
//! 为什么需要它：v1.5.19 之前 token 计量只存在于 `/cost` 的单会话视图里，且
//! `cache_read_tokens` / `api_calls` 根本不落盘（`/resume` 后归零）。于是
//! 「这个月一共烧了多少 token、每次模型调用平均带多少上下文」这类问题在
//! 磁盘上无解。本命令直接扫 `~/.wyj-code/sessions/*.json` 做聚合，
//! 是 token 治理的**基线来源**——所有优化都要拿它做前后对比。
//!
//! 纯读：不打开任何会话、不写盘、不碰运行中状态。

use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;
use wyj_core::SessionFile;

/// 聚合后的 token 总量 + 派生比率。
#[derive(Debug, Clone, Default, Serialize)]
pub struct UsageTotals {
    pub sessions: usize,
    pub turns: usize,
    /// 模型推理次数。旧会话文件没有这个字段，计 0——**比率计算须把它算进分母
    /// 校正**，否则旧数据会把「每次调用平均 input」系统性高估。
    pub api_calls: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    /// 发生过自动压缩 / context editing 清理的会话数。
    pub sessions_compacted: usize,
    /// `context_edit_freed_tokens > 0` 的会话数。
    pub sessions_context_edited: usize,
    /// 曾因端点拒绝 cache_control 而降级的会话数。
    pub sessions_cache_downgraded: usize,
}

/// 派生比率。`api_calls == 0` 时所有 per-call 值为 `None` 而不是 0——
/// 「没有数据」与「真的是 0」必须区分开，否则会把"未记录"误读成"零消耗"。
#[derive(Debug, Clone, Serialize)]
pub struct UsageRates {
    pub input_per_turn: f64,
    pub output_per_turn: f64,
    pub input_per_api_call: Option<f64>,
    pub output_per_api_call: Option<f64>,
    /// cache_read / (input + cache_read)，即前缀缓存命中率。
    pub cache_hit_ratio: Option<f64>,
    pub output_share: f64,
}

impl UsageTotals {
    pub fn add(&mut self, f: &SessionFile) {
        self.sessions += 1;
        self.turns = self.turns.saturating_add(f.turns);
        self.api_calls = self.api_calls.saturating_add(f.api_calls as usize);
        self.input_tokens += f.input_tokens as u64;
        self.output_tokens += f.output_tokens as u64;
        self.cache_read_tokens += f.cache_read_tokens as u64;
        self.cache_write_tokens += f.cache_write_tokens as u64;
        if f.compact_count > 0 {
            self.sessions_compacted += 1;
        }
        if f.context_edit_freed_tokens > 0 {
            self.sessions_context_edited += 1;
        }
        if f.prompt_cache_state == wyj_core::prompt_cache_state::DOWNGRADED {
            self.sessions_cache_downgraded += 1;
        }
    }

    pub fn rates(&self) -> UsageRates {
        let denominator = self.input_tokens + self.cache_read_tokens;
        UsageRates {
            input_per_turn: ratio(self.input_tokens as f64, self.turns as f64),
            output_per_turn: ratio(self.output_tokens as f64, self.turns as f64),
            input_per_api_call: per_call(self.input_tokens, self.api_calls),
            output_per_api_call: per_call(self.output_tokens, self.api_calls),
            cache_hit_ratio: (self.cache_read_tokens > 0)
                .then(|| self.cache_read_tokens as f64 / denominator as f64),
            output_share: ratio(self.output_tokens as f64, denominator as f64),
        }
    }
}

fn ratio(numerator: f64, denominator: f64) -> f64 {
    if denominator > 0.0 {
        numerator / denominator
    } else {
        0.0
    }
}

fn per_call(tokens: u64, calls: usize) -> Option<f64> {
    (calls > 0).then(|| tokens as f64 / calls as f64)
}

#[derive(Debug, Serialize)]
pub struct UsageReport {
    total: UsageTotals,
    rates: UsageRates,
    by_month: BTreeMap<String, UsageTotals>,
    by_project: BTreeMap<String, UsageTotals>,
    /// 旧会话文件没有 `api_calls` 字段，这类会话的 token 无法归因到「每次调用」。
    sessions_without_api_calls: usize,
}

/// 汇总 `sessions_dir` 下所有会话。支持按日期与项目过滤。
///
/// 过滤按**文件内 `timestamp` 字段**判断，而不是文件 mtime：mtime 会被
/// `/resume`、备份恢复、手动拷贝改写，用它做历史统计会静默串档。
pub fn collect(
    sessions_dir: &Path,
    since: Option<&str>,
    project: Option<&str>,
) -> Result<UsageReport> {
    let mut total = UsageTotals::default();
    let mut by_month: BTreeMap<String, UsageTotals> = BTreeMap::new();
    let mut by_project: BTreeMap<String, UsageTotals> = BTreeMap::new();
    let mut sessions_without_api_calls = 0usize;

    let entries = std::fs::read_dir(sessions_dir)
        .with_context(|| format!("读取会话目录失败：{}", sessions_dir.display()))?;

    for entry in entries.flatten() {
        let path = entry.path();
        // 子 Agent trace 与 checkpoint 是目录/其他后缀，一并跳过。
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        // 单个坏文件不能让整条命令失败：跳过并继续统计其余会话。
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(f) = serde_json::from_str::<SessionFile>(&raw) else {
            continue;
        };

        let ts = &f.timestamp;
        if let Some(since) = since {
            // ISO-8601 前缀比较即可：都是 `YYYY-MM-DD` 打头的 UTC 串，
            // 字典序 == 时间序，跨年份同样成立。
            if ts.as_str() < since {
                continue;
            }
        }
        if let Some(project) = project {
            if f.cwd.as_str() != project {
                continue;
            }
        }

        if f.api_calls == 0 {
            sessions_without_api_calls += 1;
        }
        total.add(&f);
        by_month
            .entry(ts.get(..7).unwrap_or("unknown").to_string())
            .or_default()
            .add(&f);
        by_project.entry(f.cwd.clone()).or_default().add(&f);
    }

    Ok(UsageReport {
        rates: total.rates(),
        total,
        by_month,
        by_project,
        sessions_without_api_calls,
    })
}

pub fn run(since: Option<String>, project: Option<String>, json: bool) -> Result<()> {
    let sessions_dir = wyj_config::config_dir()?.join("sessions");
    let report = collect(&sessions_dir, since.as_deref(), project.as_deref())?;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    print_human(&report);
    Ok(())
}

fn print_human(report: &UsageReport) {
    use wyj_i18n::{tr, tr_fmt};

    let t = &report.total;
    println!("{}", tr("usage.header"));
    if t.sessions == 0 {
        println!("{}", tr("usage.no_sessions"));
        return;
    }
    println!(
        "{}",
        tr_fmt(
            "usage.totals",
            &[
                ("sessions", &t.sessions.to_string()),
                ("turns", &t.turns.to_string()),
                ("api_calls", &t.api_calls.to_string()),
            ]
        )
    );
    println!(
        "{}",
        tr_fmt(
            "usage.tokens",
            &[
                ("input", &t.input_tokens.to_string()),
                ("output", &t.output_tokens.to_string()),
                ("cache_read", &t.cache_read_tokens.to_string()),
                ("cache_write", &t.cache_write_tokens.to_string()),
            ]
        )
    );
    println!(
        "{}",
        tr_fmt(
            "usage.rates",
            &[
                ("input_per_turn", &fmt_int(report.rates.input_per_turn)),
                ("output_per_turn", &fmt_int(report.rates.output_per_turn)),
                (
                    "input_per_call",
                    &report
                        .rates
                        .input_per_api_call
                        .map(fmt_int)
                        .unwrap_or_else(|| "-".into())
                ),
                (
                    "output_per_call",
                    &report
                        .rates
                        .output_per_api_call
                        .map(fmt_int)
                        .unwrap_or_else(|| "-".into())
                ),
                (
                    "cache_hit",
                    &report
                        .rates
                        .cache_hit_ratio
                        .map(fmt_pct)
                        .unwrap_or_else(|| "-".into())
                ),
                ("output_share", &fmt_pct(report.rates.output_share)),
            ]
        )
    );

    if report.sessions_without_api_calls > 0 {
        // 必须提示：这些会话的 `api_calls` 是 0（v1.5.19 之前未落盘），
        // per-call 比率的样本是**有偏的**，且随时间推移越晚的会话越可信。
        println!(
            "{}",
            tr_fmt(
                "usage.legacy_no_api_calls",
                &[("count", &report.sessions_without_api_calls.to_string())]
            )
        );
    }
    if t.sessions_cache_downgraded > 0 {
        println!(
            "{}",
            tr_fmt(
                "usage.cache_downgraded",
                &[("count", &t.sessions_cache_downgraded.to_string())]
            )
        );
    }
    if t.sessions_context_edited == 0 && t.sessions_compacted == 0 {
        println!("{}", tr("usage.never_trimmed"));
    }

    if !report.by_month.is_empty() {
        println!("\n{}", tr("usage.by_month"));
        for (month, m) in &report.by_month {
            println!(
                "  {month}: {}",
                tr_fmt(
                    "usage.month_row",
                    &[
                        ("input", &m.input_tokens.to_string()),
                        ("output", &m.output_tokens.to_string()),
                        ("turns", &m.turns.to_string()),
                        ("input_per_turn", &fmt_int(m.rates().input_per_turn)),
                    ]
                )
            );
        }
    }
    if !report.by_project.is_empty() {
        println!("\n{}", tr("usage.by_project"));
        for (project, p) in &report.by_project {
            println!(
                "  {project}: {}",
                tr_fmt(
                    "usage.project_row",
                    &[
                        ("input", &p.input_tokens.to_string()),
                        ("output", &p.output_tokens.to_string()),
                        ("sessions", &p.sessions.to_string()),
                    ]
                )
            );
        }
    }
}

fn fmt_int(v: f64) -> String {
    format!("{v:.0}")
}

fn fmt_pct(v: f64) -> String {
    format!("{:.1}%", v * 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::path::PathBuf;

    struct Fixture {
        dir: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("wyj-usage-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self { dir }
        }

        fn write(&self, id: &str, body: serde_json::Value) {
            std::fs::write(
                self.dir.join(format!("{id}.json")),
                serde_json::to_string_pretty(&body).unwrap(),
            )
            .unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn session_json(timestamp: &str, cwd: &str, input: u64, output: u64, calls: u64) -> Value {
        serde_json::json!({
            "session_id": "s",
            "title": "t",
            "last_preview": "p",
            "cwd": cwd,
            "timestamp": timestamp,
            "turns": 2,
            "input_tokens": input,
            "output_tokens": output,
            "api_calls": calls,
            "messages": [],
        })
    }

    #[test]
    fn aggregates_totals_and_derived_rates() {
        let fx = Fixture::new("totals");
        fx.write(
            "a",
            session_json("2026-09-10T00:00:00Z", "/p1", 1000, 200, 4),
        );
        fx.write(
            "b",
            session_json("2026-10-02T00:00:00Z", "/p2", 3000, 600, 6),
        );
        let r = collect(&fx.dir, None, None).unwrap();
        assert_eq!(r.total.sessions, 2);
        assert_eq!(r.total.turns, 4);
        assert_eq!(r.total.api_calls, 10);
        assert_eq!(r.total.input_tokens, 4000);
        assert_eq!(r.total.output_tokens, 800);
        assert_eq!(r.rates.input_per_turn, 1000.0);
        assert_eq!(r.rates.input_per_api_call, Some(400.0));
        assert_eq!(r.by_month.len(), 2);
        assert_eq!(r.by_project.len(), 2);
    }

    #[test]
    fn since_filters_on_timestamp_not_mtime() {
        let fx = Fixture::new("since");
        fx.write(
            "old",
            session_json("2026-09-10T00:00:00Z", "/p", 100, 10, 1),
        );
        fx.write(
            "new",
            session_json("2026-10-02T00:00:00Z", "/p", 999, 10, 1),
        );
        let r = collect(&fx.dir, Some("2026-10-01"), None).unwrap();
        assert_eq!(r.total.sessions, 1);
        assert_eq!(r.total.input_tokens, 999);
    }

    #[test]
    fn project_filter_matches_cwd_exactly() {
        let fx = Fixture::new("project");
        fx.write(
            "a",
            session_json("2026-10-02T00:00:00Z", "/code/app", 10, 1, 1),
        );
        fx.write(
            "b",
            session_json("2026-10-02T00:00:00Z", "/code/other", 777, 1, 1),
        );
        let r = collect(&fx.dir, None, Some("/code/app")).unwrap();
        assert_eq!(r.total.input_tokens, 10);
        assert_eq!(r.by_project.len(), 1);
    }

    /// v1.5.19 之前落盘的会话没有 `api_calls`。per-call 必须给 None 而不是
    /// 除零/0，否则历史数据会被读成「每次调用零消耗」。
    #[test]
    fn legacy_sessions_without_api_calls_report_none_not_zero() {
        let fx = Fixture::new("legacy");
        fx.write(
            "old",
            session_json("2026-09-01T00:00:00Z", "/p", 500, 50, 0),
        );
        let r = collect(&fx.dir, None, None).unwrap();
        assert_eq!(r.rates.input_per_api_call, None);
        assert_eq!(r.sessions_without_api_calls, 1);
        // 回合级比率仍然可用——它不依赖 api_calls。
        assert_eq!(r.rates.input_per_turn, 250.0);
    }

    #[test]
    fn empty_directory_reports_zero_without_erroring() {
        let fx = Fixture::new("empty");
        let r = collect(&fx.dir, None, None).unwrap();
        assert_eq!(r.total.sessions, 0);
        assert_eq!(r.rates.input_per_api_call, None);
        assert_eq!(r.rates.input_per_turn, 0.0);
    }

    /// 坏文件不能让整条命令失败——扫的是用户目录，里面可能有被写坏的 JSON。
    #[test]
    fn corrupt_file_is_skipped_not_fatal() {
        let fx = Fixture::new("corrupt");
        std::fs::write(fx.dir.join("broken.json"), "{not json").unwrap();
        fx.write("good", session_json("2026-10-02T00:00:00Z", "/p", 42, 4, 1));
        let r = collect(&fx.dir, None, None).unwrap();
        assert_eq!(r.total.sessions, 1);
        assert_eq!(r.total.input_tokens, 42);
    }

    /// 子 Agent trace / checkpoint 目录不能被当成会话计入。
    #[test]
    fn non_json_entries_are_ignored() {
        let fx = Fixture::new("nonjson");
        std::fs::create_dir_all(fx.dir.join("sess-1.checkpoints")).unwrap();
        std::fs::write(fx.dir.join("notes.txt"), "x").unwrap();
        fx.write("a", session_json("2026-10-02T00:00:00Z", "/p", 7, 1, 1));
        let r = collect(&fx.dir, None, None).unwrap();
        assert_eq!(r.total.sessions, 1);
    }

    #[test]
    fn cache_hit_ratio_uses_cached_plus_fresh_input() {
        let mut t = UsageTotals {
            input_tokens: 300,
            cache_read_tokens: 700,
            ..Default::default()
        };
        t.turns = 1;
        assert_eq!(t.rates().cache_hit_ratio, Some(0.7));
    }

    #[test]
    fn downgrade_flag_is_counted() {
        let fx = Fixture::new("downgrade");
        let mut body = session_json("2026-10-02T00:00:00Z", "/p", 10, 1, 1);
        body["prompt_cache_state"] = serde_json::json!(wyj_core::prompt_cache_state::DOWNGRADED);
        fx.write("a", body);
        let r = collect(&fx.dir, None, None).unwrap();
        assert_eq!(r.total.sessions_cache_downgraded, 1);
    }
}
