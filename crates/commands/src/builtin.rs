//! 内置 Slash 命令

use crate::registry::{
    Command, CommandContext, CommandRegistry, CommandResult, SubAgentControlAction,
};
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use wyj_i18n::{tr, tr_fmt};

// ── /help ─────────────────────────────────────────────────────────────────────

pub struct HelpCmd;

#[async_trait]
impl Command for HelpCmd {
    fn runs_while_busy(&self) -> bool {
        true
    }
    fn name(&self) -> &str {
        "help"
    }
    fn description(&self) -> String {
        tr("help.desc")
    }
    fn usage(&self) -> String {
        "/help".to_string()
    }
    async fn run(&self, _args: &str, ctx: &CommandContext) -> Result<CommandResult> {
        let version = env!("CARGO_PKG_VERSION");
        let mut text = tr_fmt("help.body", &[("version", version)]);
        // 运行时动态发现的命令（Skill / .claude/commands）因人而异，不进静态
        // help.body 模板，在此追加为独立分组（见 CLAUDE.md「Slash 命令约定」）。
        if !ctx.dynamic_commands.is_empty() {
            text.push_str(&format!("\n\n## {}\n\n", tr("help.custom_commands_header")));
            for (_name, desc, usage) in &ctx.dynamic_commands {
                text.push_str(&format!("- `{usage}` — {desc}\n"));
            }
        }
        Ok(CommandResult::Output(text))
    }
}

// ── /clear ────────────────────────────────────────────────────────────────────

pub struct ClearCmd;

#[async_trait]
impl Command for ClearCmd {
    fn name(&self) -> &str {
        "clear"
    }
    fn description(&self) -> String {
        tr("clear.desc")
    }
    fn usage(&self) -> String {
        "/clear".to_string()
    }
    async fn run(&self, _args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        Ok(CommandResult::ClearHistory)
    }
}

// ── /compact ──────────────────────────────────────────────────────────────────

pub struct CompactCmd;

#[async_trait]
impl Command for CompactCmd {
    fn name(&self) -> &str {
        "compact"
    }
    fn description(&self) -> String {
        tr("compact.desc")
    }
    fn usage(&self) -> String {
        tr("compact.usage")
    }
    async fn run(&self, _args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        Ok(CommandResult::CompactHistory)
    }
}

// ── /context ─────────────────────────────────────────────────────────────────
// 上下文占用分解面板。状态栏自 v0.2.26 起不再显示占用百分比（压缩已完全自动化，
// 且旧百分比算的是 messages-only 口径、与压缩阈值对不上号），所以愿意关心上下文
// 的人改用这个命令主动查——这里的数字与自动压缩的触发计算**同源**。
pub struct ContextCmd;

#[async_trait]
impl Command for ContextCmd {
    fn name(&self) -> &str {
        "context"
    }
    fn description(&self) -> String {
        tr("context.desc")
    }
    fn usage(&self) -> String {
        tr("context.usage")
    }
    async fn run(&self, _args: &str, ctx: &CommandContext) -> Result<CommandResult> {
        let mut text = format!("{}{}\n\n", crate::cost_view::INDENT, tr("context.header"));

        let Some(audit) = ctx.context_audit.filter(|a| a.total() > 0) else {
            text.push_str(&tr("context.empty"));
            return Ok(CommandResult::Output(text));
        };
        // 分母必须是**本次判定实际用的窗口**，不是配置里的那个。端点拒绝过
        // 一次之后有效窗口会收紧（见 `Session::effective_context_window`），
        // 此时按配置值算出来的「占用率 / 阈值」是一个永远触发不到的数字——
        // 面板与决策不同源，用户就会照着它判断「离压缩还早得很」。
        let window = audit.effective_window(ctx.context_window);
        if window == 0 {
            text.push_str(&tr("context.empty"));
            return Ok(CommandResult::Output(text));
        }

        let total = audit.total();
        let pct = total as f64 / window as f64 * 100.0;
        // 压缩阈值 = 窗口 - buffer。buffer 必须覆盖输出预留，否则压缩完
        // 再加 max_tokens 还是会撞穿（见 compact_trigger_buffer）。
        let buffer = wyj_core::compact::compact_trigger_buffer(window, audit.output_reserve);
        let threshold = window.saturating_sub(buffer);

        // 列宽按**显示宽度**算，不能用 `{label:<18}`——那是按字符个数补的，
        // 中文标签每字占 2 列，补出来的列自然歪（"System prompt" 13 字符补到
        // 18 = 18 列，"工具定义" 4 字符补到 18 = 16 列，差 2 列）。
        let rows: Vec<crate::cost_view::Row> = [
            (tr("context.system"), audit.system_tokens),
            (tr("context.tools"), audit.tool_schema_tokens),
            (tr("context.history"), audit.history_tokens),
            (tr("context.output"), audit.output_reserve),
            (tr("context.framing"), audit.framing_tokens),
        ]
        .into_iter()
        .map(|(label, value)| crate::cost_view::Row::new(label, u64::from(value), None))
        .collect();
        let mut rows = rows;
        rows.push(crate::cost_view::Row::total(
            tr("context.total"),
            u64::from(total),
            None,
        ));
        // 数值列不留 `—` 占位：这里每项都有确切值，`cost.unknown` 会让
        // 有值的行也挂上「未知」标记。
        text.push_str(&crate::cost_view::Table::new(rows, "").render(ctx.terminal_width));
        text.push('\n');

        text.push_str(&tr_fmt(
            "context.summary",
            &[
                ("total", &fmt_num(total)),
                ("window", &fmt_num(window)),
                ("pct", &format!("{pct:.0}")),
                ("threshold", &fmt_num(threshold)),
            ],
        ));

        // 可回收的旧工具结果：目前**只统计不删除**（第 3 批的 context editing
        // 才会真的回收），如实告知用户有多少是"已经用不上但仍占着窗口"的。
        if audit.reclaimable_tool_result_tokens > 0 {
            text.push('\n');
            text.push_str(&tr_fmt(
                "context.reclaimable",
                &[
                    ("tokens", &fmt_num(audit.reclaimable_tool_result_tokens)),
                    (
                        "kept",
                        &wyj_core::compact::RECLAIM_KEEP_RECENT_TOOL_CALLS.to_string(),
                    ),
                ],
            ));
        }

        // 已清理量：把「已经省下多少」和「还能省多少」放在一起，否则用户看到
        // 一个持续增长的可回收量却看不到它有没有被处理过。
        if ctx.context_edit.0 > 0 {
            text.push('\n');
            text.push_str(&tr_fmt(
                "context.edited",
                &[
                    ("count", &ctx.context_edit.0.to_string()),
                    ("saved", &fmt_num(ctx.context_edit.1)),
                ],
            ));
        }

        if ctx.compact_count > 0 {
            text.push('\n');
            text.push_str(&tr_fmt(
                "context.compacted",
                &[("times", &ctx.compact_count.to_string())],
            ));
        }

        // 时延画像：TTFT / 净推理 / 重试开销。只在真跑过请求后显示，
        // 否则一行「TTFT —」对还没提问的用户是纯噪音。
        if ctx.last_latency.total_ms > 0 {
            text.push('\n');
            text.push_str(&tr_fmt(
                "context.latency",
                &[
                    (
                        "ttft",
                        &ctx.last_latency
                            .ttft_ms
                            .map(|ms| format!("{:.1}s", ms as f64 / 1000.0))
                            .unwrap_or_else(|| "-".to_string()),
                    ),
                    ("total", &fmt_secs(ctx.last_latency.net_total_ms())),
                ],
            ));
            // 重试单列：把它混在总耗时里，用户会把网络抖动误判成「模型变慢了」。
            if ctx.last_latency.retried {
                text.push('\n');
                text.push_str(&tr_fmt(
                    "context.latency_retry",
                    &[("spent", &fmt_secs(ctx.last_latency.retry_ms))],
                ));
            }
        }

        // prompt 指纹漂移：回答「这次 resume 和上次运行注入的东西一样吗」。
        // 三种状态必须分开显示，否则用户会误读：
        //   None                → 没数据（升级前的老会话 / 还没发请求）
        //   Some(空)            → 确认一致
        //   Some(有 section)    → 变了，并指出是哪一段、增减了什么工具
        match ctx.prompt_drift.as_ref() {
            None => {
                text.push('\n');
                text.push_str(&tr("context.prompt_drift_none"));
            }
            Some(drift) if drift.is_empty() => {
                text.push('\n');
                text.push_str(&tr("context.prompt_drift_same"));
            }
            Some(drift) => {
                text.push('\n');
                text.push_str(&tr("context.prompt_drift_header"));
                text.push('\n');
                text.push_str(&tr_fmt(
                    "context.prompt_drift_previous",
                    &[("at", &drift.previous_recorded_at)],
                ));
                for section in &drift.sections {
                    text.push('\n');
                    let key = match section.name.as_str() {
                        wyj_core::prompt_fingerprint::SECTION_MAIN => "context.prompt_drift_main",
                        _ => "context.prompt_drift_tools",
                    };
                    text.push_str(&tr_fmt(
                        key,
                        &[
                            ("old_bytes", &section.previous_bytes.to_string()),
                            ("new_bytes", &section.current_bytes.to_string()),
                        ],
                    ));
                    // 工具名集合的增减是最有信息量的一条：只说「digest 变了」
                    // 用户还得自己去翻配置。同名工具改描述时两行为空，此时
                    // digest 变化本身就说明是内容改写。
                    if !section.added_tools.is_empty() {
                        text.push('\n');
                        text.push_str(&tr_fmt(
                            "context.prompt_drift_added",
                            &[("names", &section.added_tools.join(", "))],
                        ));
                    }
                    if !section.removed_tools.is_empty() {
                        text.push('\n');
                        text.push_str(&tr_fmt(
                            "context.prompt_drift_removed",
                            &[("names", &section.removed_tools.join(", "))],
                        ));
                    }
                }
            }
        }

        // 表格必须原样输出：走 `Output` 会被 markdown 当段落重新折行，
        // 上面那列对齐会全部作废。
        Ok(CommandResult::OutputPlain(text.trim_end().to_string()))
    }
}

/// 毫秒 → 「1.8s」/「320ms」。低于 1s 用 ms，否则用户看不出差别。
fn fmt_secs(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

// ── /new ────────────────────────────────────────────────────────────────────
// 对齐 Claude Code `/new`：开启新会话，自动保存当前会话历史到磁盘后分配新
// session_id、清空 TUI 内存状态。无二次确认弹窗（用户已经明确输入了命令名）。
pub struct NewCmd;

#[async_trait]
impl Command for NewCmd {
    fn name(&self) -> &str {
        "new"
    }
    fn description(&self) -> String {
        tr("new.desc")
    }
    fn usage(&self) -> String {
        tr("new.usage")
    }
    async fn run(&self, _args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        Ok(CommandResult::StartNewSession)
    }
}

// ── /checkpoint /rewind /branch ──────────────────────────────────────────────

pub struct CheckpointCmd;

#[async_trait]
impl Command for CheckpointCmd {
    fn name(&self) -> &str {
        "checkpoint"
    }
    fn description(&self) -> String {
        "Create or list recoverable conversation/workspace checkpoints".to_string()
    }
    fn usage(&self) -> String {
        "/checkpoint [list|name]".to_string()
    }
    async fn run(&self, args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        let args = args.trim();
        Ok(CommandResult::CreateCheckpoint {
            list: args.eq_ignore_ascii_case("list"),
            name: (!args.is_empty() && !args.eq_ignore_ascii_case("list"))
                .then(|| args.to_string()),
        })
    }
}

pub struct RewindCmd;

#[async_trait]
impl Command for RewindCmd {
    fn name(&self) -> &str {
        "rewind"
    }
    fn description(&self) -> String {
        "Preview or restore conversation/files from a checkpoint".to_string()
    }
    fn usage(&self) -> String {
        "/rewind [checkpoint-id|latest] [conversation|files|both] [--confirm]".to_string()
    }
    async fn run(&self, args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        let mut checkpoint_id = None;
        let mut scope = wyj_core::RewindScope::Both;
        let mut confirmed = false;
        for token in args.split_whitespace() {
            match token.to_ascii_lowercase().as_str() {
                "latest" => checkpoint_id = None,
                "conversation" => scope = wyj_core::RewindScope::Conversation,
                "files" => scope = wyj_core::RewindScope::Files,
                "both" => scope = wyj_core::RewindScope::Both,
                "--confirm" => confirmed = true,
                _ if checkpoint_id.is_none() => checkpoint_id = Some(token.to_string()),
                _ => anyhow::bail!("usage: {}", self.usage()),
            }
        }
        Ok(CommandResult::Rewind {
            checkpoint_id,
            scope,
            confirmed,
        })
    }
}

pub struct BranchCmd;

#[async_trait]
impl Command for BranchCmd {
    fn name(&self) -> &str {
        "branch"
    }
    fn description(&self) -> String {
        "Create a new session from a checkpoint without changing the original".to_string()
    }
    fn usage(&self) -> String {
        "/branch [checkpoint-id|latest] [--restore-files] [--confirm]".to_string()
    }
    async fn run(&self, args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        let mut checkpoint_id = None;
        let mut restore_files = false;
        let mut confirmed = false;
        for token in args.split_whitespace() {
            match token.to_ascii_lowercase().as_str() {
                "latest" => checkpoint_id = None,
                "--restore-files" => restore_files = true,
                "--confirm" => confirmed = true,
                _ if checkpoint_id.is_none() => checkpoint_id = Some(token.to_string()),
                _ => anyhow::bail!("usage: {}", self.usage()),
            }
        }
        Ok(CommandResult::BranchSession {
            checkpoint_id,
            restore_files,
            confirmed,
        })
    }
}

pub struct AgentControlCmd;

#[async_trait]
impl Command for AgentControlCmd {
    fn runs_while_busy(&self) -> bool {
        true
    }
    fn name(&self) -> &str {
        "agent-control"
    }
    fn description(&self) -> String {
        "Send follow-up, interrupt or retry to a running sub-agent".to_string()
    }
    fn usage(&self) -> String {
        "/agent-control <id> <follow-up text|interrupt|retry>".to_string()
    }
    async fn run(&self, args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        let mut parts = args.trim().splitn(3, char::is_whitespace);
        let id = parts
            .next()
            .and_then(|value| value.trim_start_matches('a').parse::<u64>().ok())
            .ok_or_else(|| anyhow::anyhow!("usage: {}", self.usage()))?;
        let action = parts
            .next()
            .ok_or_else(|| anyhow::anyhow!("usage: {}", self.usage()))?;
        let action = match action {
            "interrupt" => SubAgentControlAction::Interrupt,
            "retry" | "retry-last" => SubAgentControlAction::RetryLast,
            "follow-up" | "followup" => {
                let text = parts.next().unwrap_or_default().trim();
                anyhow::ensure!(!text.is_empty(), "follow-up text is required");
                SubAgentControlAction::FollowUp(text.to_string())
            }
            _ => anyhow::bail!("usage: {}", self.usage()),
        };
        Ok(CommandResult::ControlSubAgent { id, action })
    }
}

// ── /cost ─────────────────────────────────────────────────────────────────────

pub struct CostCmd;

// (前缀, input_per_mtok, output_per_mtok, cache_read_ratio) in USD
//
// 硬编码单价，随官方调价可能过时——这是已知的未决项（计划 U3）。查不到单价时
// **必须显式提示**而不是静默降级成"只有 token 数"：用户看到没有金额时无法区分
// 「真的免费」和「我没查到」。
//
// **匹配是子串匹配（`contains`），因此顺序即优先级**：更精确的前缀必须排在
// 更宽的前缀之前，否则 `MiniMax-M3.1-Flash-Preview` 会先命中 `minimax-m3`
// 而被算成 M3 的价（贵一倍）。
static PRICES: &[(&str, f64, f64, f64)] = &[
    ("claude-opus-4", 15.0, 75.0, 0.1),
    ("claude-opus-3", 15.0, 75.0, 0.1),
    ("claude-sonnet-4", 3.0, 15.0, 0.1),
    ("claude-sonnet-3", 3.0, 15.0, 0.1),
    ("claude-haiku-4", 0.8, 4.0, 0.1),
    ("claude-haiku-3", 0.25, 1.25, 0.1),
    // MiniMax 走 Anthropic 兼容端点，单价远低于 Anthropic 同档。
    //
    // 官方价（人民币/百万 tokens，2026-10 核对，按 7.0 折美元）：
    //   M3.1-Flash  输入 1.00 / 输出 4.00 / 缓存读 0.10（= 输入价的 0.1x）
    //   M3          输入 2.10 / 输出 8.40 / 缓存读 0.42（= 输入价的 0.2x，>512k 输入翻倍）
    // M3.1-Flash 的条目**必须排在 `minimax-m3` 之前**（子串匹配，顺序即优先级）。
    // M3 用的是低档价（≤512k 输入）；超长输入按 2 倍计费，本表不区分。
    //
    // 放在列表里是因为第三方端点默认**不**开启缓存
    // （见 `Profile::effective_prompt_cache`），未显式配置时全额按 input 计。
    ("minimax-m3.1-flash", 0.15, 0.60, 0.1),
    ("minimax-m3", 0.30, 1.20, 0.2),
    ("minimax-m2", 0.30, 1.20, 0.1),
];

/// 查到模型单价：`(input_per_mtok, output_per_mtok, cache_read_ratio)`。
fn lookup_price(model: &str) -> Option<(f64, f64, f64)> {
    let model_lower = model.to_lowercase();
    PRICES
        .iter()
        .find(|(prefix, _, _, _)| model_lower.contains(prefix))
        .map(|(_, i, o, cr)| (*i, *o, *cr))
}

#[async_trait]
impl Command for CostCmd {
    fn name(&self) -> &str {
        "cost"
    }
    fn description(&self) -> String {
        tr("cost.desc")
    }
    fn usage(&self) -> String {
        "/cost".to_string()
    }

    /// 重排后的 `/cost`：主表（明细 + 合计线）→ 派生指标 → 上下文占比条
    /// → 子 Agent 分表 → 脚注。
    ///
    /// 相对旧版的三个实质变化：
    /// 1. **列宽在 [`cost_view`] 里算**，不再靠 i18n 模板里的空格凑对齐（旧版
    ///    切到英文就散架）；
    /// 2. **未知值显式渲染**（缓存写入恒 0 时显示"未上报"而非 `~$0.0000`，
    ///    后者会被读成"这项免费"）；
    /// 3. **口径说明全部下沉到脚注**，行内只留数字——旧版每行都拖着一句括号
    ///    解释，是"凌乱"的主因。
    async fn run(&self, _args: &str, ctx: &CommandContext) -> Result<CommandResult> {
        use crate::cost_view::{context_bar, display_width, fmt_int, fmt_pct, Row, Table, INDENT};

        // 归一化一次，之后所有子部件都吃同一个 `width`，各自不再夹。
        let width = crate::cost_view::usable_width(ctx.terminal_width);
        let indent_w = display_width(INDENT);
        let prices = lookup_price(&ctx.model);
        let unknown = tr("cost.unknown");

        // ── 金额换算 ──────────────────────────────────────────────────────
        // 缓存读取按**该模型自己的**倍率折算（Anthropic 与 MiniMax M3.1-Flash 是
        // 输入价的 0.1x，MiniMax M3 是 0.2x），缓存写入按 1.25x 输入价。
        // `cache_write_known` 为 false 时用 None 占位而不是折算成 0 ——
        // 端点不上报 ≠ 不计费。
        let (input_cost, output_cost, cache_read_cost, cache_write_cost, sub_costs) = match prices {
            Some((ip, op, cache_read_ratio)) => {
                let sub = (
                    ctx.sub_input_tokens as f64 / 1_000_000.0 * ip,
                    ctx.sub_output_tokens as f64 / 1_000_000.0 * op,
                    ctx.sub_cache_read_tokens as f64 / 1_000_000.0 * ip * cache_read_ratio,
                    ctx.sub_cache_write_tokens as f64 / 1_000_000.0 * ip * 1.25,
                );
                (
                    ctx.input_tokens as f64 / 1_000_000.0 * ip,
                    ctx.output_tokens as f64 / 1_000_000.0 * op,
                    ctx.cache_read_tokens as f64 / 1_000_000.0 * ip * cache_read_ratio,
                    ctx.cache_write_tokens as f64 / 1_000_000.0 * ip * 1.25,
                    sub,
                )
            }
            None => (0.0, 0.0, 0.0, 0.0, (0.0, 0.0, 0.0, 0.0)),
        };
        let total_cost = input_cost
            + output_cost
            + cache_read_cost
            + cache_write_cost
            + sub_costs.0
            + sub_costs.1
            + sub_costs.2
            + sub_costs.3;

        // ── 主表 ──────────────────────────────────────────────────────────
        // 缓存命中 > 0 而写入恒 0：端点能缓存却不统计写入量。此时写入行必须
        // 走占位符 —— 折算成 `~$0.0000` 等于告诉用户"这项免费"，而它是**未上报**，
        // 真实费用未知（可能还要按 1.25x 输入价补）。这是本命令最容易出错
        // 的一个数，单独提出来先算好。
        //
        // 排除 DOWNGRADED：那是端点**拒绝**了 cache_control、缓存整个停用，
        // 与"能用但不报写入量"是两回事。此时残留的 cache_read 来自降级之前，
        // 套上"下界"提示只会误导——该提示的是 `cache_downgraded_line`。
        let cache_write_unreported = ctx.cache_read_tokens > 0
            && ctx.cache_write_tokens == 0
            && ctx.prompt_cache_state != wyj_core::prompt_cache_state::DOWNGRADED;
        let total_tokens = u64::from(ctx.input_tokens)
            + u64::from(ctx.output_tokens)
            + u64::from(ctx.cache_read_tokens)
            + u64::from(ctx.cache_write_tokens);
        let mut rows = vec![
            Row::new(
                tr("cost.row_input"),
                u64::from(ctx.input_tokens),
                prices.map(|_| input_cost),
            ),
            Row::new(
                tr("cost.row_output"),
                u64::from(ctx.output_tokens),
                prices.map(|_| output_cost),
            ),
            Row::new(
                tr("cost.row_cache_read"),
                u64::from(ctx.cache_read_tokens),
                prices.map(|_| cache_read_cost),
            ),
            Row::new(
                tr("cost.row_cache_write"),
                u64::from(ctx.cache_write_tokens),
                if cache_write_unreported {
                    None
                } else {
                    prices.map(|_| cache_write_cost)
                },
            ),
            Row::total(
                tr("cost.row_total"),
                total_tokens,
                prices.map(|_| total_cost),
            ),
        ];
        // 子 Agent 有用量时并进同一张表：它已计入上方合计，单独成表反而会
        // 让用户以为有两笔费用。放在合计线之后，用分节标题 + 一层缩进区分。
        let has_sub = ctx.sub_input_tokens > 0
            || ctx.sub_output_tokens > 0
            || ctx.sub_cache_read_tokens > 0
            || ctx.sub_cache_write_tokens > 0;
        if has_sub {
            rows.push(Row::section(tr("cost.subagent_section")));
            rows.push(
                Row::new(
                    tr("cost.subagent_input"),
                    u64::from(ctx.sub_input_tokens),
                    prices.map(|_| sub_costs.0),
                )
                .indented(),
            );
            rows.push(
                Row::new(
                    tr("cost.subagent_output"),
                    u64::from(ctx.sub_output_tokens),
                    prices.map(|_| sub_costs.1),
                )
                .indented(),
            );
            rows.push(
                Row::new(
                    tr("cost.subagent_cache"),
                    u64::from(ctx.sub_cache_read_tokens),
                    prices.map(|_| sub_costs.2),
                )
                .indented(),
            );
        }

        let table = Table::new(rows, unknown.clone()).with_header(
            tr("cost.col_item"),
            tr("cost.col_tokens"),
            tr("cost.col_cost"),
        );

        let mut out = String::new();
        // 标题行按"有值才拼"：headless / ACP 等表面拿不到 model 名，
        // 直接塞进模板会渲染成 `标题 ·  · 0 calls` 这种悬空分隔符。
        let mut title_parts = vec![tr("cost.header")];
        if !ctx.model.is_empty() {
            title_parts.push(ctx.model.clone());
        }
        title_parts.push(tr_fmt(
            "cost.calls_suffix",
            &[("calls", &fmt_int(u64::from(ctx.api_calls)))],
        ));
        // 标题行也带 INDENT：预排版消息在渲染层不再补缩进，整块版式由这里
        // 统一负责，标题与表格才会左边缘对齐。
        out.push_str(INDENT);
        out.push_str(&crate::cost_view::fit_line(
            &title_parts.join(" · "),
            width.saturating_sub(indent_w),
        ));
        out.push('\n');
        out.push_str(&table.render_block(width));
        out.push('\n');

        // ── 派生指标 ──────────────────────────────────────────────────────
        // 这几行文案长度随 locale 与数值变化，必须逐行按可用宽度收口，
        // 否则窄终端下它们会被聊天区换行拆散（实测 44 列时一行冲到 75 列）。
        // 与表格同缩进，面板读起来是**一个**块而不是两截。
        let indent = INDENT.to_string();
        let mut line = |s: String| {
            out.push('\n');
            out.push_str(&indent);
            out.push_str(&crate::cost_view::fit_line(
                &s,
                width.saturating_sub(indent_w),
            ));
        };
        let full_input = u64::from(ctx.input_tokens)
            + u64::from(ctx.cache_read_tokens)
            + u64::from(ctx.cache_write_tokens);
        if full_input > 0 {
            let hit = ctx.cache_read_tokens as f64 / full_input as f64 * 100.0;
            let full_price = (ctx.input_tokens + ctx.cache_read_tokens) as f64;
            let saved_pct = if full_price > 0.0 {
                ctx.cache_read_tokens as f64 * 0.9 / full_price * 100.0
            } else {
                0.0
            };
            line(tr_fmt(
                "cost.cache_stats_line",
                &[
                    ("hit_ratio", &fmt_pct(hit)),
                    ("saved_pct", &fmt_pct(saved_pct)),
                ],
            ));
        }
        if ctx.api_calls > 0 {
            line(tr_fmt(
                "cost.api_calls_line",
                &[(
                    "avg_input",
                    &fmt_int(u64::from(ctx.input_tokens / ctx.api_calls.max(1))),
                )],
            ));
        }

        // ── 上下文占比 ────────────────────────────────────────────────
        // 百分比口径与自动压缩同源：优先用 request 级 audit（含 system +
        // 工具 schema + 输出预留），退回旧的 messages-only 估算只在还没
        // 发起过模型请求时才会发生。
        let ctx_estimated = ctx
            .context_audit
            .map(|audit| audit.total())
            .unwrap_or(ctx.estimated_tokens);
        // 分母同样取**本次判定实际用的窗口**：端点拒绝过一次之后有效窗口会
        // 收紧到远小于配置值，此时按配置值算出的百分比会小到看不出危险。
        let ctx_window = ctx
            .context_audit
            .map(|audit| audit.effective_window(ctx.context_window))
            .unwrap_or(ctx.context_window);
        if ctx_window > 0 {
            // 条 + 数字合并成一行：拆两行会让本就高的面板再高一截，而
            // 「占了多少 / 一共多少」本就该一眼同框。后缀必须**先算出来传
            // 给 context_bar** 参与格数预算——否则条按满格渲染、再把长
            // 后缀拼在后面，窄终端必然超宽。
            let pct = fmt_pct(ctx_estimated as f64 / ctx_window as f64 * 100.0);
            let detail = tr_fmt(
                "cost.context_detail_line",
                &[
                    ("pct", &pct),
                    ("used", &fmt_int(u64::from(ctx_estimated))),
                    ("window", &fmt_int(u64::from(ctx_window))),
                ],
            );
            if let Some(bar) = context_bar(
                ctx_estimated,
                ctx_window,
                width.saturating_sub(indent_w),
                &tr("cost.bar_full"),
                &tr("cost.bar_empty"),
                &detail,
            ) {
                line(bar.trim_start().to_string());
            }
        }

        // ── 脚注 / 告警 ───────────────────────────────────────────────────
        let mut notes: Vec<String> = Vec::new();
        if cache_write_unreported {
            notes.push(tr("cost.note_cache_write_unreported"));
        }
        if ctx.prompt_cache_state == wyj_core::prompt_cache_state::DOWNGRADED {
            notes.push(tr("cost.cache_downgraded_line"));
        }
        if prices.is_none() {
            notes.push(tr_fmt("cost.price_missing", &[("model", &ctx.model)]));
        }
        if ctx.compact_count > 0 {
            notes.push(tr_fmt(
                "cost.note_compacted",
                &[("times", &ctx.compact_count.to_string())],
            ));
        }
        if !notes.is_empty() {
            out.push('\n');
            for note in notes {
                for (i, l) in note.lines().enumerate() {
                    let budget = width.saturating_sub(display_width(INDENT) + 2);
                    let body = if budget > 8 {
                        crate::cost_view::truncate_note(l, budget)
                    } else {
                        l.to_string()
                    };
                    out.push_str(INDENT);
                    out.push_str(if i == 0 { "⚠ " } else { "  " });
                    out.push_str(&body);
                    out.push('\n');
                }
            }
        }

        // 合计金额已在表格的 `Total` 行给出——底部再重复一次纯属冗余。
        //
        // 必须走 `OutputPlain`：表格的列对齐依赖换行原样保留，而 `Output` 会
        // 被 TUI 的 markdown 渲染器当段落重新折行，整张表会被揉成一团糊。
        Ok(CommandResult::OutputPlain(out.trim_end().to_string()))
    }
}

// ── /mcp ──────────────────────────────────────────────────────────────────────

pub struct McpCmd;

#[async_trait]
impl Command for McpCmd {
    fn runs_while_busy(&self) -> bool {
        true
    }
    fn name(&self) -> &str {
        "mcp"
    }
    fn description(&self) -> String {
        tr("mcp.desc")
    }
    fn usage(&self) -> String {
        "/mcp".to_string()
    }
    async fn run(&self, _args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        Ok(CommandResult::OpenMcpDialog)
    }
}

// ── /schedule ─────────────────────────────────────────────────────────────────

pub struct ScheduleCmd;

#[async_trait]
impl Command for ScheduleCmd {
    fn runs_while_busy(&self) -> bool {
        true
    }
    fn name(&self) -> &str {
        "schedule"
    }
    fn description(&self) -> String {
        tr("schedule.desc")
    }
    fn usage(&self) -> String {
        "/schedule".to_string()
    }
    async fn run(&self, _args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        Ok(CommandResult::OpenScheduleDialog)
    }
}

// ── /skills ───────────────────────────────────────────────────────────────────

pub struct SkillsCmd;

#[async_trait]
impl Command for SkillsCmd {
    fn runs_while_busy(&self) -> bool {
        true
    }
    fn name(&self) -> &str {
        "skills"
    }
    fn description(&self) -> String {
        tr("skills.desc")
    }
    fn usage(&self) -> String {
        "/skills".to_string()
    }
    async fn run(&self, _args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        Ok(CommandResult::OpenSkillsDialog)
    }
}

// ── /plugins ──────────────────────────────────────────────────────────────────

pub struct PluginsCmd;

#[async_trait]
impl Command for PluginsCmd {
    fn runs_while_busy(&self) -> bool {
        true
    }
    fn name(&self) -> &str {
        "plugins"
    }
    fn description(&self) -> String {
        tr("plugins.desc")
    }
    fn usage(&self) -> String {
        "/plugins".to_string()
    }
    async fn run(&self, _args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        Ok(CommandResult::OpenPluginsDialog)
    }
}

// ── /extensions ───────────────────────────────────────────────────────────────

pub struct ExtensionsCmd;

#[async_trait]
impl Command for ExtensionsCmd {
    fn runs_while_busy(&self) -> bool {
        true
    }
    fn name(&self) -> &str {
        "extensions"
    }
    fn description(&self) -> String {
        "统一查看和诊断 Skill / MCP / Plugin 资源".to_string()
    }
    fn usage(&self) -> String {
        "/extensions".to_string()
    }
    async fn run(&self, _args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        Ok(CommandResult::OpenExtensionsDialog)
    }
}

// ── /agents ───────────────────────────────────────────────────────────────────

pub struct AgentsCmd;

pub fn format_agents_text(defs: &[wyj_core::AgentDefinition]) -> String {
    let mut out = tr("agents.header");
    for d in defs {
        out.push_str(&format!("\n● {} — {}\n", d.name, d.description));
        if let Some(m) = &d.model {
            out.push_str(&format!(
                "  {}\n",
                tr_fmt("agents.model_line", &[("profile", m)])
            ));
        }
        let tools = d
            .tools
            .as_ref()
            .map(|t| t.join(", "))
            .unwrap_or_else(|| tr("agents.tools_all"));
        out.push_str(&format!(
            "  {}\n",
            tr_fmt("agents.tools_line", &[("tools", &tools)])
        ));
        let source = if d.builtin {
            tr("agents.builtin_tag")
        } else {
            d.source
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        };
        out.push_str(&format!(
            "  {}\n",
            tr_fmt("agents.source_line", &[("source", &source)])
        ));
    }
    out.push_str(&format!("\n{}", tr("agents.reload_note")));
    out
}

#[async_trait]
impl Command for AgentsCmd {
    fn runs_while_busy(&self) -> bool {
        true
    }
    fn name(&self) -> &str {
        "agents"
    }
    fn description(&self) -> String {
        tr("agents.desc")
    }
    fn usage(&self) -> String {
        "/agents".to_string()
    }
    async fn run(&self, _args: &str, ctx: &CommandContext) -> Result<CommandResult> {
        // 实时重读盘，方便验证新写的定义文件；但注册进模型的类型列表在
        // 启动时已固定，新增/修改定义需重启才对模型生效（下方注明）。
        let defs = wyj_core::load_agent_defs(&ctx.cwd, &ctx.plugin_agent_paths);
        let fallback_text = format_agents_text(&defs);
        Ok(CommandResult::OpenAgentsDialog {
            defs,
            fallback_text,
        })
    }
}

/// 打开/定位子 Agent 聚合面板；`args` 可选，形如 `a3` 或 `3`（Hub 分配的 id）。
pub struct SubAgentsCmd;

#[async_trait]
impl Command for SubAgentsCmd {
    fn runs_while_busy(&self) -> bool {
        true
    }
    fn name(&self) -> &str {
        "subagents"
    }
    fn description(&self) -> String {
        tr("subagents.desc")
    }
    fn usage(&self) -> String {
        "/subagents [id]".to_string()
    }
    async fn run(&self, args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        let arg = args.trim();
        if arg.is_empty() {
            return Ok(CommandResult::OpenSubAgentsPanel(None));
        }
        let digits = arg.strip_prefix(['a', 'A']).unwrap_or(arg);
        match digits.parse::<u64>() {
            Ok(id) => Ok(CommandResult::OpenSubAgentsPanel(Some(id))),
            Err(_) => Ok(CommandResult::Output(tr_fmt(
                "subagents.bad_id",
                &[("arg", arg)],
            ))),
        }
    }
}

/// 打开/定位后台 Shell 任务面板；`args` 可选，形如 `bash_1` 或 `1`。
pub struct ShellsCmd;

#[async_trait]
impl Command for ShellsCmd {
    fn runs_while_busy(&self) -> bool {
        true
    }
    fn name(&self) -> &str {
        "shells"
    }
    fn description(&self) -> String {
        tr("shells.desc")
    }
    fn usage(&self) -> String {
        "/shells [id]".to_string()
    }
    async fn run(&self, args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        let arg = args.trim();
        if arg.is_empty() {
            return Ok(CommandResult::OpenShellsPanel(None));
        }
        // 后台任务 id 形如 `bash_1`；同时接受裸数字（1），方便用户少打几个字符
        let digits = arg.strip_prefix("bash_").unwrap_or(arg);
        match digits.parse::<u64>() {
            Ok(n) => Ok(CommandResult::OpenShellsPanel(Some(format!("bash_{n}")))),
            Err(_) => Ok(CommandResult::Output(tr_fmt(
                "shells.bad_id",
                &[("arg", arg)],
            ))),
        }
    }
}

fn fmt_num(n: u32) -> String {
    let s = n.to_string();
    let mut result = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}

// ── /hooks ───────────────────────────────────────────────────────────────────

pub struct HooksCmd;

#[async_trait]
impl Command for HooksCmd {
    fn runs_while_busy(&self) -> bool {
        true
    }
    fn name(&self) -> &str {
        "hooks"
    }
    fn description(&self) -> String {
        tr("hooks.desc")
    }
    fn usage(&self) -> String {
        "/hooks".to_string()
    }
    async fn run(&self, _args: &str, ctx: &CommandContext) -> Result<CommandResult> {
        // 与 `/agents` 同一哲学：实时重新读盘，保证显示的是最新配置文件状态。
        let settings = wyj_core::load_effective_hooks(&ctx.cwd);
        let mut out = String::new();
        if !ctx.hooks_enabled {
            out.push_str(&tr("hooks.disabled_banner"));
            out.push('\n');
        }
        if settings.hooks.is_empty() {
            out.push_str(&tr("hooks.empty_note"));
            return Ok(CommandResult::Output(out));
        }
        out.push_str(&tr("hooks.header"));

        let mut events: Vec<&String> = settings.hooks.keys().collect();
        events.sort();
        for event in events {
            let entries = &settings.hooks[event];
            if entries.iter().all(|e| e.hooks.is_empty()) {
                continue;
            }
            out.push_str(&format!(
                "\n{}",
                tr_fmt("hooks.event_line", &[("event", event)])
            ));
            for entry in entries {
                if entry.hooks.is_empty() {
                    continue;
                }
                let any_matcher = tr("hooks.matcher_any");
                let matcher = entry.matcher.as_deref().unwrap_or(&any_matcher);
                out.push_str(&format!(
                    "\n  {}",
                    tr_fmt("hooks.matcher_line", &[("matcher", matcher)])
                ));
                for cmd in &entry.hooks {
                    out.push_str(&format!(
                        "\n    {}",
                        tr_fmt(
                            "hooks.command_line",
                            &[
                                ("timeout", &cmd.timeout.unwrap_or(60).to_string()),
                                ("command", &cmd.command)
                            ]
                        )
                    ));
                }
            }
        }
        Ok(CommandResult::Output(out))
    }
}

// ── /memory ───────────────────────────────────────────────────────────────────

pub struct MemoryCmd;

#[async_trait]
impl Command for MemoryCmd {
    fn runs_while_busy(&self) -> bool {
        true
    }
    fn name(&self) -> &str {
        "memory"
    }
    fn description(&self) -> String {
        tr("memory.desc")
    }
    fn usage(&self) -> String {
        "/memory [clear-all]".to_string()
    }
    async fn run(&self, args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        // 子命令：`clear-all` → TUI 弹二级确认面板；
        // 其它（含空 args / 任意未知子命令）→ 打开正常 Memory 面板。
        let trimmed = args.trim();
        if trimmed.eq_ignore_ascii_case("clear-all") || trimmed.eq_ignore_ascii_case("clear_all") {
            // 当前 active / superseded 数由 TUI 从 store.status() 取，命令层不依赖具体数。
            return Ok(CommandResult::OpenMemoryClearAllConfirm {
                active_count: 0,
                superseded_count: 0,
            });
        }
        Ok(CommandResult::OpenMemoryDialog)
    }
}

// ── /evolve ──────────────────────────────────────────────────────────────────

pub struct EvolveCmd;

#[async_trait]
impl Command for EvolveCmd {
    fn name(&self) -> &str {
        "evolve"
    }
    fn description(&self) -> String {
        tr("evolve.desc")
    }
    fn usage(&self) -> String {
        "/evolve".to_string()
    }
    async fn run(&self, _args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        Ok(CommandResult::OpenEvolutionDialog)
    }
}

// ── /doctor ───────────────────────────────────────────────────────────────────

pub struct DoctorCmd;

#[async_trait]
impl Command for DoctorCmd {
    fn runs_while_busy(&self) -> bool {
        true
    }
    fn name(&self) -> &str {
        "doctor"
    }
    fn description(&self) -> String {
        tr("doctor.desc")
    }
    fn usage(&self) -> String {
        "/doctor".to_string()
    }
    async fn run(&self, _args: &str, ctx: &CommandContext) -> Result<CommandResult> {
        use wyj_config::Config;
        let cfg = Config::load()?;
        let version = env!("CARGO_PKG_VERSION");

        let mut lines = vec![tr_fmt("doctor.header", &[("version", version)])];

        // API Key
        match cfg.api_key() {
            Ok(_) => lines.push(tr_fmt(
                "status.api_key_ok",
                &[("prefix", &cfg.redacted_api_key().unwrap_or_default())],
            )),
            Err(_) => lines.push(tr("doctor.api_key_missing")),
        }

        // 配置文件
        match wyj_config::config_dir() {
            Ok(dir) => {
                let cfg_file = dir.join("config.toml");
                if cfg_file.exists() {
                    lines.push(tr_fmt(
                        "doctor.config_file_ok",
                        &[("path", &cfg_file.display().to_string())],
                    ));
                } else {
                    lines.push(tr_fmt(
                        "doctor.config_file_missing",
                        &[("path", &cfg_file.display().to_string())],
                    ));
                }
            }
            Err(e) => lines.push(tr_fmt("doctor.config_dir_err", &[("err", &e.to_string())])),
        }

        // Provider + Model
        lines.push(tr_fmt(
            "doctor.provider",
            &[("provider", &cfg.provider().to_string())],
        ));
        lines.push(tr_fmt("doctor.model", &[("model", &ctx.model)]));

        // WebSearch：[search] 节配好了就报后端，没配就说明原因。这一项是为了让
        // 「工具怎么不见了」能在一条命令里得到答案——未配置时它压根不注册，
        // 模型侧完全看不到，用户只能靠诊断输出发现。
        match cfg.search_provider_issue() {
            None => lines.push(tr_fmt(
                "doctor.search_ok",
                &[("provider", cfg.effective_search_provider())],
            )),
            Some(issue) => lines.push(tr_fmt("doctor.search_unavailable", &[("reason", &issue)])),
        }

        // 项目规约文件（全局 + 祖先链，按 AGENTS.md → CLAUDE.md → … 回退）
        let claude_md_files = wyj_core::discover_files(&ctx.cwd);
        let existing: Vec<String> = claude_md_files
            .iter()
            .filter(|f| f.exists)
            .map(|f| f.path.display().to_string())
            .collect();
        if existing.is_empty() {
            lines.push(tr("doctor.claude_md_missing"));
        } else {
            lines.push(tr_fmt(
                "doctor.claude_md_ok",
                &[("paths", &existing.join(", "))],
            ));
        }

        // 记忆目录
        let pid = wyj_core::project_id(&ctx.cwd);
        let mem_dir = wyj_config::global_config_dir_in(&ctx.home_dir)
            .join("memory")
            .join(&pid);
        let index_path = mem_dir.join("MEMORY.md");
        if index_path.exists() {
            let entry_count = std::fs::read_to_string(&index_path)
                .unwrap_or_default()
                .lines()
                .filter(|l| l.starts_with("- ["))
                .count();
            lines.push(tr_fmt(
                "doctor.memory_dir_with_count",
                &[
                    ("path", &mem_dir.display().to_string()),
                    ("count", &entry_count.to_string()),
                ],
            ));
        } else {
            lines.push(tr_fmt(
                "doctor.memory_dir_empty",
                &[("path", &mem_dir.display().to_string())],
            ));
        }

        // MCP servers（合并全局+项目配置、过滤禁用条目后的有效数量）
        let mcp_count = ctx.effective_mcp_count;
        if mcp_count > 0 {
            lines.push(tr_fmt(
                "doctor.mcp_configured",
                &[("count", &mcp_count.to_string())],
            ));
        } else {
            lines.push(tr("doctor.mcp_none"));
        }

        Ok(CommandResult::Output(lines.join("\n")))
    }
}

// ── /computer ─────────────────────────────────────────────────────────────────

pub struct ComputerCmd;

#[async_trait]
impl Command for ComputerCmd {
    fn runs_while_busy(&self) -> bool {
        true
    }
    fn name(&self) -> &str {
        "computer"
    }
    fn description(&self) -> String {
        tr("computer.desc")
    }
    fn usage(&self) -> String {
        "/computer".to_string()
    }
    async fn run(&self, _args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        use wyj_config::{Config, ForegroundFallback, Provider};

        let mut lines = vec![tr("computer.header")];

        let os = std::env::consts::OS;
        if wyj_computer::SUPPORTED {
            lines.push(tr_fmt("computer.platform_supported", &[("os", os)]));
        } else {
            lines.push(tr_fmt("computer.platform_unsupported", &[("os", os)]));
        }

        // 门控逻辑需与 `cli::register_computer_tool_if_enabled` 的真实注册条件
        // 保持一致（平台 → vision → provider），否则诊断结果会跟实际是否
        // 注册该工具对不上。
        let cfg = Config::load()?;
        let profile = cfg.active_profile();

        if !wyj_computer::SUPPORTED {
            lines.push(tr("computer.not_registered_platform"));
        } else if !profile.vision {
            lines.push(tr("computer.not_registered_vision"));
        } else if !matches!(profile.provider, Provider::Anthropic) {
            lines.push(tr_fmt(
                "computer.not_registered_provider",
                &[("provider", &profile.provider.to_string())],
            ));
        } else if profile.is_official_anthropic_endpoint() {
            lines.push(tr("computer.mode_native"));
        } else {
            lines.push(tr("computer.mode_custom"));
        }

        let fallback = match cfg.computer_use.foreground_fallback {
            ForegroundFallback::Disabled => "disabled",
            ForegroundFallback::Ask => "ask",
            ForegroundFallback::IdleOnly => "idle_only",
        };
        lines.push(tr_fmt(
            "computer.foreground_policy",
            &[
                ("policy", fallback),
                ("quiet_ms", &cfg.computer_use.quiet_period_ms.to_string()),
                ("max_defer", &cfg.computer_use.max_defer_secs.to_string()),
            ],
        ));

        #[cfg(target_os = "macos")]
        {
            lines.push(tr("computer.background_supported"));
            if wyj_computer::accessibility::is_process_trusted() {
                lines.push(tr("computer.ax_trusted"));
            } else {
                lines.push(tr("computer.ax_untrusted"));
            }
        }
        #[cfg(not(target_os = "macos"))]
        lines.push(tr("computer.background_unavailable"));

        // 实时探测（截图/光标）即使工具未注册也照跑：既能帮用户提前定位权限
        // 问题（先解决系统权限，再切换 profile 满足注册条件），也不需要
        // 真的注册一份 ComputerTool 才能自检。
        if wyj_computer::SUPPORTED {
            let initial_status = wyj_computer::activity::ensure_monitor();
            if initial_status == wyj_computer::activity::InputMonitorStatus::Starting {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            match wyj_computer::activity::snapshot() {
                Ok(snapshot) => {
                    let idle = snapshot
                        .external_idle_secs
                        .map(|idle| format!("{idle:.2}"))
                        .unwrap_or_else(|| "n/a".to_string());
                    lines.push(tr_fmt(
                        "computer.input_monitor",
                        &[
                            ("status", snapshot.monitor_status.label()),
                            ("idle", &idle),
                            (
                                "seq",
                                &snapshot
                                    .external_event_seq
                                    .map(|seq| seq.to_string())
                                    .unwrap_or_else(|| "n/a".to_string()),
                            ),
                        ],
                    ));
                    if let Some(error) = snapshot.monitor_error {
                        lines.push(tr_fmt("computer.input_monitor_error", &[("err", &error)]));
                    }
                }
                Err(error) => lines.push(tr_fmt(
                    "computer.input_monitor_error",
                    &[("err", &error.to_string())],
                )),
            }

            match wyj_computer::target::list_windows() {
                Ok(windows) => lines.push(tr_fmt(
                    "computer.window_count",
                    &[("count", &windows.len().to_string())],
                )),
                Err(error) => lines.push(tr_fmt(
                    "computer.window_error",
                    &[("err", &error.to_string())],
                )),
            }

            let metrics = wyj_computer::telemetry::snapshot();
            lines.push(tr_fmt(
                "computer.telemetry_paths",
                &[
                    ("background", &metrics.background_actions.to_string()),
                    ("targeted", &metrics.targeted_pid_events.to_string()),
                    ("foreground", &metrics.foreground_actions.to_string()),
                    (
                        "auto_fallback",
                        &metrics.automatic_foreground_fallbacks.to_string(),
                    ),
                ],
            ));
            lines.push(tr_fmt(
                "computer.telemetry_guards",
                &[
                    ("preempted", &metrics.preempted_by_user.to_string()),
                    ("changed", &metrics.target_changed.to_string()),
                    ("requires", &metrics.requires_foreground.to_string()),
                    ("fuses", &metrics.background_focus_fuses.to_string()),
                ],
            ));

            match wyj_computer::primary_display_size() {
                Ok(d) => {
                    let (tw, th) = wyj_computer::scale::fit_within(
                        d.physical_width,
                        d.physical_height,
                        wyj_computer::DEFAULT_MAX_DIM,
                    );
                    lines.push(tr_fmt(
                        "computer.display_ok",
                        &[
                            ("pw", &d.physical_width.to_string()),
                            ("ph", &d.physical_height.to_string()),
                            ("tw", &tw.to_string()),
                            ("th", &th.to_string()),
                        ],
                    ));
                }
                Err(e) => lines.push(tr_fmt("computer.display_err", &[("err", &e.to_string())])),
            }

            match wyj_computer::capture_primary(64) {
                Ok(cap) => lines.push(tr_fmt(
                    "computer.screenshot_ok",
                    &[("bytes", &cap.png.len().to_string())],
                )),
                Err(e) => lines.push(tr_fmt(
                    "computer.screenshot_err",
                    &[("err", &e.to_string())],
                )),
            }

            match wyj_computer::cursor_location() {
                Ok((x, y)) => lines.push(tr_fmt(
                    "computer.input_ok",
                    &[("x", &x.to_string()), ("y", &y.to_string())],
                )),
                Err(e) => lines.push(tr_fmt("computer.input_err", &[("err", &e.to_string())])),
            }

            // 光标位置读取成功不代表输入合成一定生效：macOS 未授权「辅助功能」
            // 时 Enigo::new() 往往仍会成功，点击/按键被系统静默丢弃、不报错，
            // 这种失败模式没有错误可捕获，只能靠固定提醒兜底。
            lines.push(tr("computer.accessibility_hint"));
        }

        Ok(CommandResult::Output(lines.join("\n")))
    }
}

// ── /model ────────────────────────────────────────────────────────────────────

pub struct ModelCmd;

#[async_trait]
impl Command for ModelCmd {
    fn runs_while_busy(&self) -> bool {
        true
    }
    fn name(&self) -> &str {
        "model"
    }
    fn description(&self) -> String {
        tr("model.desc")
    }
    fn usage(&self) -> String {
        "/model [profile-name] | /model doctor [profile-name]".to_string()
    }
    async fn run(&self, args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        if args.trim().is_empty() {
            return Ok(CommandResult::OpenProfileDialog);
        }
        let mut parts = args.split_whitespace();
        if parts.next() == Some("doctor") {
            return Ok(CommandResult::ModelDoctor(parts.next().map(str::to_string)));
        }
        Ok(CommandResult::SwitchProfile(args.trim().to_string()))
    }
}

// ── /mode (占位，实际由 app.rs 硬编码拦截) ────────────────────────────────────

pub struct ModeCmd;

#[async_trait]
impl Command for ModeCmd {
    fn runs_while_busy(&self) -> bool {
        true
    }
    fn name(&self) -> &str {
        "mode"
    }
    fn description(&self) -> String {
        tr("mode.desc")
    }
    fn usage(&self) -> String {
        "/mode [normal|plan|bypass]".to_string()
    }
    async fn run(&self, _args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        // app.rs 在 dispatch 之前已拦截 /mode，此分支实际不会执行
        Ok(CommandResult::None)
    }
}

// ── /mouse ──────────────────────────────────────────────────────────────────

/// 切换鼠标捕获。只发结果，具体状态由 TUI 主循环翻转并写回终端。
pub struct MouseCmd;

#[async_trait]
impl Command for MouseCmd {
    fn runs_while_busy(&self) -> bool {
        true
    }
    fn name(&self) -> &str {
        "mouse"
    }
    fn description(&self) -> String {
        tr("mouse.desc")
    }
    fn usage(&self) -> String {
        "/mouse".to_string()
    }
    async fn run(&self, _args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        Ok(CommandResult::ToggleMouseCapture)
    }
}

// ── /cwd ─────────────────────────────────────────────────────────────────────

pub struct CwdCmd;

#[async_trait]
impl Command for CwdCmd {
    fn runs_while_busy(&self) -> bool {
        true
    }
    fn name(&self) -> &str {
        "cwd"
    }
    fn description(&self) -> String {
        tr("cwd.desc")
    }
    fn usage(&self) -> String {
        "/cwd".to_string()
    }
    async fn run(&self, _args: &str, ctx: &CommandContext) -> Result<CommandResult> {
        Ok(CommandResult::Output(tr_fmt(
            "cwd.body",
            &[("cwd", &ctx.cwd.display().to_string())],
        )))
    }
}

// ── /init ─────────────────────────────────────────────────────────────────────

pub struct InitCmd;

#[async_trait]
impl Command for InitCmd {
    fn name(&self) -> &str {
        "init"
    }
    fn description(&self) -> String {
        tr("init.desc")
    }
    fn usage(&self) -> String {
        "/init".to_string()
    }
    async fn run(&self, _args: &str, ctx: &CommandContext) -> Result<CommandResult> {
        // 骨架创建是确定性代码，不依赖 LLM：先确保 .wyj-code/ 目录 + 带注释的
        // 空 mcp.toml/settings.toml 模板存在（已存在则不动，防止覆盖用户已填
        // 内容），再触发 agent 回合生成/合并 AGENTS.md。失败按 best-effort
        // 处理——骨架初始化是锦上添花，不应该让 /init 的核心功能（生成
        // CLAUDE.md）因为这一步失败而整体失败。
        ensure_project_config_skeleton(&ctx.cwd);

        // 对齐真实 Claude Code：/init 不是静态模板写文件，而是触发一次真正的 agent
        // 回合，让它自己去探索项目（Cargo.toml/package.json/README/目录结构等）
        // 生成或合并改进规约文件。已存在则要求读取后合并而非整体覆盖。
        //
        // 目标文件名**必须跟着仓库现状走**：项目里已经有 CLAUDE.md 时再去生成
        // AGENTS.md，等于凭空造一份内容重复的第二份规约——「命中即停」只保证
        // 模型不会双份注入，管不住维护者把两份写成不同步的副本。这里复用
        // `claude_md::existing_memory_file_name`，与实际注入走同一条解析线，
        // 不存在「改了 A 文件、面板却说 B」的第二条路径。
        let target_name = wyj_core::claude_md::existing_memory_file_name(&ctx.cwd)
            .unwrap_or_else(|| "AGENTS.md".to_string());
        let target = ctx.cwd.join(&target_name);
        let prompt = tr_fmt(
            "init.agent_prompt",
            &[
                ("cwd", &ctx.cwd.display().to_string()),
                ("target", &target.display().to_string()),
                ("target_name", &target_name),
            ],
        );
        Ok(CommandResult::RunPrompt(prompt))
    }
}

const PROJECT_MCP_TEMPLATE: &str = r#"# 项目级 MCP server 定义（随仓库共享，团队成员克隆后自动生效）。
# 格式与全局 ~/.wyj-code/config.toml 的 [[mcp_servers]] 段一致，同名覆盖全局配置。
# 首次连接前需要在 TUI 里确认信任（防止克隆到陌生仓库时静默执行任意命令）。
#
# [[mcp_servers]]
# name = "postgres"
# transport = "stdio"
# command = "npx"
# args = ["-y", "@modelcontextprotocol/server-postgres"]

mcp_servers = []
"#;

const PROJECT_SETTINGS_TEMPLATE: &str = r#"# 项目级开关：控制本项目禁用哪些 skill / MCP server（按名字禁用，无论
# 条目来源——六层合并链任意一层、手写进 mcp.toml 的条目均适用）。
# 不影响 skill 文件内容或 mcp.toml 里的 server 定义本身，只是一层开关。
#
# disabled_skills = ["some-skill-name"]
# disabled_mcp_servers = ["some-server-name"]

disabled_skills = []
disabled_mcp_servers = []
"#;

/// 确保 `<git-root>/.wyj-code/` 目录 + 带注释的空 `mcp.toml`/`settings.toml` 模板存在。
/// 幂等：已存在的文件不覆盖。不预建空的 `skills/`/`agents/` 子目录——Git 不
/// 追踪空目录，预建了也不会随 `/init` 一起被提交，等真正需要时由既有的
/// `skill_install.rs` 等惰性 `create_dir_all` 即可。
fn ensure_project_config_skeleton(cwd: &std::path::Path) {
    let dir = wyj_config::project_config_dir(cwd);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!("创建项目配置目录失败: {e}");
        return;
    }
    let mcp_path = dir.join("mcp.toml");
    if !mcp_path.exists() {
        if let Err(e) = std::fs::write(&mcp_path, PROJECT_MCP_TEMPLATE) {
            tracing::warn!("写入 mcp.toml 模板失败: {e}");
        }
    }
    let settings_path = dir.join("settings.toml");
    if !settings_path.exists() {
        if let Err(e) = std::fs::write(&settings_path, PROJECT_SETTINGS_TEMPLATE) {
            tracing::warn!("写入 settings.toml 模板失败: {e}");
        }
    }
}

#[cfg(test)]
mod init_tests {
    use super::*;

    #[test]
    fn creates_skeleton_files_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        ensure_project_config_skeleton(dir.path());

        let mcp_path = dir.path().join(".wyj-code").join("mcp.toml");
        let settings_path = dir.path().join(".wyj-code").join("settings.toml");
        assert!(mcp_path.exists());
        assert!(settings_path.exists());

        // 生成的模板必须是合法可解析的 TOML，且是空配置（不预设任何 server/开关）。
        let servers = wyj_config::load_project_mcp(dir.path()).unwrap();
        assert!(servers.is_empty());
        let settings = wyj_config::load_project_settings(dir.path()).unwrap();
        assert!(settings.disabled_skills.is_empty());
        assert!(settings.disabled_mcp_servers.is_empty());
    }

    #[test]
    fn does_not_overwrite_existing_content() {
        let dir = tempfile::tempdir().unwrap();
        wyj_config::save_project_mcp(
            dir.path(),
            &[wyj_config::McpServerConfig {
                name: "existing".to_string(),
                transport: wyj_config::McpTransport::Stdio,
                command: Some("npx".to_string()),
                args: vec![],
                env: Default::default(),
                url: None,
                headers: Default::default(),
            }],
        )
        .unwrap();

        ensure_project_config_skeleton(dir.path());

        let servers = wyj_config::load_project_mcp(dir.path()).unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "existing");
    }

    #[test]
    fn init_from_nested_cwd_creates_skeleton_at_git_root() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".git")).unwrap();
        let nested = repo.path().join("crates").join("demo");
        std::fs::create_dir_all(&nested).unwrap();

        ensure_project_config_skeleton(&nested);

        assert!(repo.path().join(".wyj-code").join("mcp.toml").exists());
        assert!(repo.path().join(".wyj-code").join("settings.toml").exists());
        assert!(!nested.join(".wyj-code").exists());
    }
}

// ── /config ───────────────────────────────────────────────────────────────────

pub struct ConfigCmd;

#[async_trait]
impl Command for ConfigCmd {
    fn runs_while_busy(&self) -> bool {
        true
    }
    fn name(&self) -> &str {
        "config"
    }
    fn description(&self) -> String {
        tr("config.desc")
    }
    fn usage(&self) -> String {
        "/config".to_string()
    }
    async fn run(&self, _args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        Ok(CommandResult::OpenSettingsDialog)
    }
}

// ── /resume ───────────────────────────────────────────────────────────────────

pub struct ResumeCmd;

#[async_trait]
impl Command for ResumeCmd {
    fn name(&self) -> &str {
        "resume"
    }
    fn description(&self) -> String {
        tr("resume.desc")
    }
    fn usage(&self) -> String {
        "/resume [session-id]".to_string()
    }
    async fn run(&self, args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        let id = args.trim();
        if id.is_empty() {
            Ok(CommandResult::OpenSessionPicker)
        } else {
            Ok(CommandResult::ResumeSession(id.to_string()))
        }
    }
}

// ── /sessions ─────────────────────────────────────────────────────────────────

pub struct SessionsCmd;

#[async_trait]
impl Command for SessionsCmd {
    fn runs_while_busy(&self) -> bool {
        true
    }
    fn name(&self) -> &str {
        "sessions"
    }
    fn description(&self) -> String {
        tr("sessions.desc")
    }
    fn usage(&self) -> String {
        "/sessions".to_string()
    }
    async fn run(&self, _args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        Ok(CommandResult::OpenSessionPicker)
    }
}

// ── /quit ─────────────────────────────────────────────────────────────────────

pub struct QuitCmd;

#[async_trait]
impl Command for QuitCmd {
    fn runs_while_busy(&self) -> bool {
        true
    }
    fn name(&self) -> &str {
        "quit"
    }
    fn description(&self) -> String {
        tr("quit.desc")
    }
    fn usage(&self) -> String {
        "/quit".to_string()
    }
    async fn run(&self, _args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        Ok(CommandResult::Quit)
    }
}

// ── GitHub 协作命令辅助 ────────────────────────────────────────────────────────

/// 探测 `gh` CLI 是否可用
async fn gh_available() -> bool {
    tokio::process::Command::new("gh")
        .arg("--version")
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// 运行外部命令并捕获输出，返回 (是否成功, stdout, stderr)
async fn run_capture(
    program: &str,
    args: &[&str],
    cwd: &std::path::Path,
) -> (bool, String, String) {
    match tokio::process::Command::new(program)
        .args(args)
        .current_dir(cwd)
        .output()
        .await
    {
        Ok(o) => (
            o.status.success(),
            String::from_utf8_lossy(&o.stdout).to_string(),
            String::from_utf8_lossy(&o.stderr).to_string(),
        ),
        Err(e) => (false, String::new(), e.to_string()),
    }
}

/// 从 origin remote URL 解析 `owner/repo`（支持 https 与 ssh 形式）
fn parse_github_slug(url: &str) -> Option<String> {
    let url = url.trim();
    let rest = url
        .strip_prefix("git@github.com:")
        .or_else(|| url.strip_prefix("https://github.com/"))
        .or_else(|| url.strip_prefix("http://github.com/"))
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))?;
    let rest = rest
        .strip_suffix(".git")
        .unwrap_or(rest)
        .trim_end_matches('/');
    let mut parts = rest.split('/');
    let owner = parts.next().filter(|s| !s.is_empty())?;
    let repo = parts.next().filter(|s| !s.is_empty())?;
    Some(format!("{owner}/{repo}"))
}

/// URL 查询参数百分号编码
fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 用系统默认浏览器打开 URL（best-effort），返回是否成功启动
async fn open_browser(url: &str) -> bool {
    #[cfg(target_os = "macos")]
    let (prog, args): (&str, Vec<&str>) = ("open", vec![url]);
    #[cfg(target_os = "windows")]
    let (prog, args): (&str, Vec<&str>) = ("cmd", vec!["/C", "start", "", url]);
    #[cfg(all(unix, not(target_os = "macos")))]
    let (prog, args): (&str, Vec<&str>) = ("xdg-open", vec![url]);
    tokio::process::Command::new(prog)
        .args(&args)
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

// ── /bug ──────────────────────────────────────────────────────────────────────

pub struct BugCmd;

#[async_trait]
impl Command for BugCmd {
    fn name(&self) -> &str {
        "bug"
    }
    fn description(&self) -> String {
        tr("bug.desc")
    }
    fn usage(&self) -> String {
        "/bug [简述]".to_string()
    }
    fn needs_background(&self) -> bool {
        // 同步跑 `git remote get-url` 并拉起系统浏览器（`open`）：在 TUI 主循环
        // 里 inline await 会让按键（ESC / Ctrl+C）全部读不到。
        true
    }
    async fn run(&self, args: &str, ctx: &CommandContext) -> Result<CommandResult> {
        let (ok, stdout, _) = run_capture("git", &["remote", "get-url", "origin"], &ctx.cwd).await;
        let Some(slug) = (if ok { parse_github_slug(&stdout) } else { None }) else {
            return Ok(CommandResult::Output(tr("bug.no_remote")));
        };
        let title = args.trim();
        let body = format!(
            "## 环境\n- wyj-code: {ver}\n- OS: {os} ({arch})\n- Model: {model}\n\n\
             ## 问题描述\n{desc}\n\n## 复现步骤\n1. \n\n## 期望行为\n",
            ver = env!("CARGO_PKG_VERSION"),
            os = std::env::consts::OS,
            arch = std::env::consts::ARCH,
            model = ctx.model,
            desc = title,
        );
        let mut url = format!(
            "https://github.com/{slug}/issues/new?body={}",
            percent_encode(&body)
        );
        if !title.is_empty() {
            url.push_str(&format!("&title={}", percent_encode(title)));
        }
        if open_browser(&url).await {
            Ok(CommandResult::Output(tr_fmt(
                "bug.opened",
                &[("url", &url)],
            )))
        } else {
            Ok(CommandResult::Output(tr_fmt(
                "bug.manual",
                &[("url", &url)],
            )))
        }
    }
}

// ── /review ───────────────────────────────────────────────────────────────────

pub struct ReviewCmd;

#[async_trait]
impl Command for ReviewCmd {
    fn name(&self) -> &str {
        "review"
    }
    fn description(&self) -> String {
        tr("review.desc")
    }
    fn usage(&self) -> String {
        "/review [PR编号]".to_string()
    }
    // 刻意**不**标 needs_background：`/review` 成功时返回 RunPrompt（要把 diff
    // 交给 Agent 起新回合），而回合 spawn 只存在于 TUI 主循环里，后台 task 的
    // 结果回不到那里。代价是一次 `gh pr diff` 的秒级停顿——与 `/compact` 那种
    // 分钟级、且无任何反馈的冻结不同量级，故不做这次重构。
    async fn run(&self, args: &str, ctx: &CommandContext) -> Result<CommandResult> {
        if !gh_available().await {
            return Ok(CommandResult::Output(tr("gh.missing")));
        }
        let pr = args.trim();
        let mut diff_args = vec!["pr", "diff"];
        if !pr.is_empty() {
            diff_args.push(pr);
        }
        let (ok, stdout, stderr) = run_capture("gh", &diff_args, &ctx.cwd).await;
        if !ok || stdout.trim().is_empty() {
            let err = if stderr.trim().is_empty() {
                stdout
            } else {
                stderr
            };
            return Ok(CommandResult::Output(tr_fmt(
                "review.failed",
                &[("err", err.trim())],
            )));
        }
        let prompt = tr_fmt("review.agent_prompt", &[("diff", &stdout)]);
        Ok(CommandResult::RunPrompt(prompt))
    }
}

// ── /pr-comments ──────────────────────────────────────────────────────────────

pub struct PrCommentsCmd;

#[async_trait]
impl Command for PrCommentsCmd {
    fn name(&self) -> &str {
        "pr-comments"
    }
    fn description(&self) -> String {
        tr("prcomments.desc")
    }
    fn usage(&self) -> String {
        "/pr-comments [PR编号]".to_string()
    }
    fn needs_background(&self) -> bool {
        // `gh --version` + `gh pr view --comments`：两次子进程往返，PR 评论多时
        // 输出十几 KB，inline 执行会冻住界面。
        true
    }
    async fn run(&self, args: &str, ctx: &CommandContext) -> Result<CommandResult> {
        if !gh_available().await {
            return Ok(CommandResult::Output(tr("gh.missing")));
        }
        let pr = args.trim();
        let mut view_args = vec!["pr", "view"];
        if !pr.is_empty() {
            view_args.push(pr);
        }
        view_args.push("--comments");
        let (ok, stdout, stderr) = run_capture("gh", &view_args, &ctx.cwd).await;
        if ok {
            Ok(CommandResult::Output(stdout))
        } else {
            Ok(CommandResult::Output(tr_fmt(
                "prcomments.failed",
                &[("err", stderr.trim())],
            )))
        }
    }
}

/// 创建包含所有内置命令的注册表
pub fn standard_registry() -> Arc<CommandRegistry> {
    let mut reg = CommandRegistry::new();
    reg.register(Arc::new(HelpCmd));
    reg.register(Arc::new(ClearCmd));
    reg.register(Arc::new(CompactCmd));
    reg.register(Arc::new(ContextCmd));
    reg.register(Arc::new(NewCmd));
    reg.register(Arc::new(CheckpointCmd));
    reg.register(Arc::new(RewindCmd));
    reg.register(Arc::new(BranchCmd));
    reg.register(Arc::new(AgentControlCmd));
    reg.register(Arc::new(CostCmd));
    reg.register(Arc::new(AgentsCmd));
    reg.register(Arc::new(SubAgentsCmd));
    reg.register(Arc::new(ShellsCmd));
    reg.register(Arc::new(HooksCmd));
    reg.register(Arc::new(MemoryCmd));
    reg.register(Arc::new(EvolveCmd));
    reg.register(Arc::new(DoctorCmd));
    reg.register(Arc::new(ComputerCmd));
    reg.register(Arc::new(ModelCmd));
    // SandboxCmd 已随 OS sandbox 一起移除；/sandbox 不再注册。
    reg.register(Arc::new(ModeCmd));
    reg.register(Arc::new(MouseCmd));
    reg.register(Arc::new(CwdCmd));
    reg.register(Arc::new(ResumeCmd));
    reg.register(Arc::new(SessionsCmd));
    reg.register(Arc::new(InitCmd));
    reg.register(Arc::new(ConfigCmd));
    reg.register(Arc::new(McpCmd));
    reg.register(Arc::new(ScheduleCmd));
    reg.register(Arc::new(SkillsCmd));
    reg.register(Arc::new(PluginsCmd));
    reg.register(Arc::new(ExtensionsCmd));
    reg.register(Arc::new(BugCmd));
    reg.register(Arc::new(ReviewCmd));
    reg.register(Arc::new(PrCommentsCmd));
    reg.register(Arc::new(QuitCmd));
    Arc::new(reg)
}

/// 创建包含内置命令 + 已加载 skill 的注册表
/// skill 先注册（优先级低），内置命令后注册（同名时覆盖 skill）
pub fn standard_registry_with_skills(
    home: &std::path::Path,
    cwd: &std::path::Path,
    disabled_skills: &std::collections::HashSet<String>,
    plugin_skill_sources: &[std::path::PathBuf],
    project_gate: crate::skill::ProjectSkillsGate,
) -> Arc<CommandRegistry> {
    let mut reg = CommandRegistry::new();

    // 先注册 skill（优先级低）
    for skill in crate::skill::load_skills(
        home,
        cwd,
        disabled_skills,
        plugin_skill_sources,
        project_gate,
    ) {
        reg.register(skill);
    }

    // 再注册内置命令（后注册覆盖同名 skill）
    reg.register(Arc::new(HelpCmd));
    reg.register(Arc::new(ClearCmd));
    reg.register(Arc::new(CompactCmd));
    reg.register(Arc::new(ContextCmd));
    reg.register(Arc::new(NewCmd));
    reg.register(Arc::new(CheckpointCmd));
    reg.register(Arc::new(RewindCmd));
    reg.register(Arc::new(BranchCmd));
    reg.register(Arc::new(AgentControlCmd));
    reg.register(Arc::new(CostCmd));
    reg.register(Arc::new(AgentsCmd));
    reg.register(Arc::new(SubAgentsCmd));
    reg.register(Arc::new(ShellsCmd));
    reg.register(Arc::new(HooksCmd));
    reg.register(Arc::new(MemoryCmd));
    reg.register(Arc::new(EvolveCmd));
    reg.register(Arc::new(DoctorCmd));
    reg.register(Arc::new(ComputerCmd));
    reg.register(Arc::new(ModelCmd));
    // SandboxCmd 已随 OS sandbox 一起移除；/sandbox 不再注册。
    reg.register(Arc::new(ModeCmd));
    reg.register(Arc::new(MouseCmd));
    reg.register(Arc::new(CwdCmd));
    reg.register(Arc::new(ResumeCmd));
    reg.register(Arc::new(SessionsCmd));
    reg.register(Arc::new(InitCmd));
    reg.register(Arc::new(ConfigCmd));
    reg.register(Arc::new(McpCmd));
    reg.register(Arc::new(ScheduleCmd));
    reg.register(Arc::new(SkillsCmd));
    reg.register(Arc::new(PluginsCmd));
    reg.register(Arc::new(ExtensionsCmd));
    reg.register(Arc::new(BugCmd));
    reg.register(Arc::new(ReviewCmd));
    reg.register(Arc::new(PrCommentsCmd));
    reg.register(Arc::new(QuitCmd));

    Arc::new(reg)
}

#[cfg(test)]
mod help_tests {
    use super::*;

    pub(super) fn ctx_with_dynamic(
        dynamic_commands: Vec<(String, String, String)>,
    ) -> CommandContext {
        CommandContext {
            cwd: std::path::PathBuf::new(),
            model: String::new(),
            terminal_width: 0,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            api_calls: 0,
            prompt_cache_state: 0,
            context_window: 0,
            estimated_tokens: 0,
            context_audit: None,
            last_latency: Default::default(),
            compact_count: 0,
            context_edit: (0, 0),
            home_dir: std::path::PathBuf::new(),
            sub_input_tokens: 0,
            sub_output_tokens: 0,
            sub_cache_read_tokens: 0,
            sub_cache_write_tokens: 0,
            effective_mcp_count: 0,
            plugin_agent_paths: vec![],
            hooks_enabled: false,
            dynamic_commands,
            prompt_drift: None,
        }
    }

    #[tokio::test]
    async fn help_appends_custom_commands_section_when_dynamic_commands_present() {
        let ctx = ctx_with_dynamic(vec![(
            "fix-issue".to_string(),
            "修复一个 issue".to_string(),
            "/fix-issue <issue-number>".to_string(),
        )]);
        let CommandResult::Output(text) = HelpCmd.run("", &ctx).await.unwrap() else {
            panic!("expected Output");
        };
        assert!(text.contains(&tr("help.custom_commands_header")));
        assert!(text.contains("/fix-issue <issue-number>"));
        assert!(text.contains("修复一个 issue"));
    }

    #[tokio::test]
    async fn help_omits_custom_commands_section_when_none_dynamic() {
        let ctx = ctx_with_dynamic(vec![]);
        let CommandResult::Output(text) = HelpCmd.run("", &ctx).await.unwrap() else {
            panic!("expected Output");
        };
        assert!(!text.contains(&tr("help.custom_commands_header")));
    }

    #[tokio::test]
    async fn help_body_documents_subagents_command() {
        let ctx = ctx_with_dynamic(vec![]);
        let CommandResult::Output(text) = HelpCmd.run("", &ctx).await.unwrap() else {
            panic!("expected Output");
        };
        assert!(text.contains("/subagents"));
    }

    #[tokio::test]
    async fn evolve_is_registered_and_documented_in_help() {
        let registry = standard_registry();
        let command = registry.get("evolve").expect("/evolve is registered");
        assert_eq!(command.usage(), "/evolve");

        let ctx = ctx_with_dynamic(vec![]);
        let CommandResult::Output(text) = HelpCmd.run("", &ctx).await.unwrap() else {
            panic!("expected Output");
        };
        assert!(text.contains("/evolve"));

        let result = registry.dispatch("/evolve", &ctx).await.unwrap().unwrap();
        assert!(matches!(result, CommandResult::OpenEvolutionDialog));
    }
}

#[cfg(test)]
mod agents_tests {
    use super::*;

    fn empty_ctx() -> CommandContext {
        CommandContext {
            cwd: std::path::PathBuf::new(),
            model: String::new(),
            terminal_width: 0,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            api_calls: 0,
            prompt_cache_state: 0,
            context_window: 0,
            estimated_tokens: 0,
            context_audit: None,
            last_latency: Default::default(),
            compact_count: 0,
            context_edit: (0, 0),
            home_dir: std::path::PathBuf::new(),
            sub_input_tokens: 0,
            sub_output_tokens: 0,
            sub_cache_read_tokens: 0,
            sub_cache_write_tokens: 0,
            effective_mcp_count: 0,
            plugin_agent_paths: vec![],
            hooks_enabled: false,
            dynamic_commands: vec![],
            prompt_drift: None,
        }
    }

    #[tokio::test]
    async fn agents_command_returns_dialog_data_with_text_fallback() {
        let result = AgentsCmd.run("", &empty_ctx()).await.unwrap();
        let CommandResult::OpenAgentsDialog {
            defs,
            fallback_text,
        } = result
        else {
            panic!("expected OpenAgentsDialog");
        };
        assert!(defs.iter().any(|d| d.name == "general-purpose"));
        assert!(fallback_text.contains("general-purpose"));
    }
}

#[cfg(test)]
mod subagents_tests {
    use super::*;

    fn empty_ctx() -> CommandContext {
        CommandContext {
            cwd: std::path::PathBuf::new(),
            model: String::new(),
            terminal_width: 0,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            api_calls: 0,
            prompt_cache_state: 0,
            context_window: 0,
            estimated_tokens: 0,
            context_audit: None,
            last_latency: Default::default(),
            compact_count: 0,
            context_edit: (0, 0),
            home_dir: std::path::PathBuf::new(),
            sub_input_tokens: 0,
            sub_output_tokens: 0,
            sub_cache_read_tokens: 0,
            sub_cache_write_tokens: 0,
            effective_mcp_count: 0,
            plugin_agent_paths: vec![],
            hooks_enabled: false,
            dynamic_commands: vec![],
            prompt_drift: None,
        }
    }

    #[tokio::test]
    async fn no_args_opens_panel_without_target_id() {
        let ctx = empty_ctx();
        let result = SubAgentsCmd.run("", &ctx).await.unwrap();
        assert!(matches!(result, CommandResult::OpenSubAgentsPanel(None)));
    }

    #[tokio::test]
    async fn numeric_arg_parses_to_target_id() {
        let ctx = empty_ctx();
        let result = SubAgentsCmd.run("3", &ctx).await.unwrap();
        assert!(matches!(result, CommandResult::OpenSubAgentsPanel(Some(3))));
    }

    #[tokio::test]
    async fn a_prefixed_arg_parses_to_target_id() {
        let ctx = empty_ctx();
        let result = SubAgentsCmd.run("a12", &ctx).await.unwrap();
        assert!(matches!(
            result,
            CommandResult::OpenSubAgentsPanel(Some(12))
        ));
    }

    #[tokio::test]
    async fn garbage_arg_reports_error_instead_of_opening_panel() {
        let ctx = empty_ctx();
        let result = SubAgentsCmd.run("not-a-number", &ctx).await.unwrap();
        match result {
            CommandResult::Output(text) => assert!(text.contains("not-a-number")),
            other => panic!("expected Output error text, got {other:?}"),
        }
    }

    // ── /context ──────────────────────────────────────────────────────

    fn ctx_with_audit(
        audit: Option<wyj_core::compact::ContextAudit>,
        window: u32,
    ) -> CommandContext {
        let mut ctx = empty_ctx();
        ctx.context_window = window;
        ctx.context_audit = audit;
        ctx
    }

    #[tokio::test]
    async fn context_without_audit_reports_no_request_yet() {
        let ctx = ctx_with_audit(None, 200_000);
        let result = ContextCmd.run("", &ctx).await.unwrap();
        match result {
            // 用 i18n key 本身做断言而不是硬编码中文——测试跑在任意 locale 下
            // 都成立（`set_locale` 是进程级全局状态，不能在测试里改）。
            CommandResult::OutputPlain(text) | CommandResult::Output(text) => {
                assert!(text.contains(&tr("context.empty")), "got: {text}")
            }
            other => panic!("expected Output(Plain), got {other:?}"),
        }
    }

    /// 面板里的合计必须与压缩决策同源：`audit.total()` 直接等于阈值判定用的值，
    /// 不是 messages-only 的旧估算。回归背景：旧 `/cost` 百分比与压缩阈值差
    /// 一万多 token（200K 窗口下 6 个以上百分点）。
    #[tokio::test]
    async fn context_panel_uses_the_same_accounting_as_compaction() {
        let audit = wyj_core::compact::ContextAudit {
            system_tokens: 2_000,
            tool_schema_tokens: 5_000,
            history_tokens: 100_000,
            output_reserve: 8_192,
            framing_tokens: 300,
            window: 0,
            reclaimable_tool_result_tokens: 0,
        };
        let ctx = ctx_with_audit(Some(audit), 200_000);
        let result = ContextCmd.run("", &ctx).await.unwrap();
        match result {
            CommandResult::OutputPlain(text) | CommandResult::Output(text) => {
                // `fmt_num` 可能带千分位，断言前先归一化。
                let flat = text.replace(',', "");
                assert!(flat.contains(&audit.total().to_string()), "got: {text}");
                // 阈值 = 200K - max(8192+8192, 20000) = 180K
                assert!(
                    flat.contains("180000"),
                    "阈值未按新 buffer 公式显示: {text}"
                );
            }
            other => panic!("expected Output(Plain), got {other:?}"),
        }
    }

    /// `/context` 的分母与阈值必须取**本次判定实际用的窗口**，不是配置值。
    ///
    /// 端点拒绝过一次之后有效窗口会收紧（`Session::effective_context_window`），
    /// 此时面板若还按配置值算，用户看到的是一个永远触发不到的阈值——压缩其实
    /// 早在另一个线上就会触发。这与 v0.2.26 消灭的「同一个事实两套口径」是
    /// 同一种病换个地方复发。
    #[tokio::test]
    async fn context_panel_uses_the_effective_window_not_the_configured_one() {
        let audit = wyj_core::compact::ContextAudit {
            system_tokens: 2_000,
            tool_schema_tokens: 5_000,
            history_tokens: 100_000,
            output_reserve: 32_000,
            framing_tokens: 300,
            reclaimable_tool_result_tokens: 0,
            // 端点实测：这份会话实际只被允许 160K
            window: 160_000,
        };
        // 配置里写着 1M（典型的「填大了」）
        let ctx = ctx_with_audit(Some(audit), 1_000_000);
        match ContextCmd.run("", &ctx).await.unwrap() {
            CommandResult::OutputPlain(text) | CommandResult::Output(text) => {
                let flat = text.replace(',', "");
                // 阈值 = 160K - max(32000+8192, 16000) = 119808
                assert!(
                    flat.contains("119808"),
                    "阈值必须按有效窗口算，而不是配置的 1M: {text}"
                );
                assert!(
                    !flat.contains("900000"),
                    "不应再出现按配置窗口算出的阈值: {text}"
                );
                assert!(flat.contains("160000"), "窗口本身应显示有效值: {text}");
            }
            other => panic!("expected Output(Plain), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn context_panel_surfaces_reclaimable_tool_results() {
        let audit = wyj_core::compact::ContextAudit {
            history_tokens: 100_000,
            window: 0,
            reclaimable_tool_result_tokens: 23_100,
            ..Default::default()
        };
        let ctx = ctx_with_audit(Some(audit), 200_000);
        match ContextCmd.run("", &ctx).await.unwrap() {
            CommandResult::OutputPlain(text) | CommandResult::Output(text) => {
                // `fmt_num` 可能带千分位，断言前先把它归一化掉。
                assert!(
                    text.replace(',', "").contains("23100"),
                    "可回收量未显示: {text}"
                );
            }
            other => panic!("expected Output(Plain), got {other:?}"),
        }
    }

    // ── /shells ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn shells_no_args_opens_panel_without_target() {
        let ctx = empty_ctx();
        let result = ShellsCmd.run("", &ctx).await.unwrap();
        assert!(matches!(result, CommandResult::OpenShellsPanel(None)));
    }

    #[tokio::test]
    async fn shells_accepts_both_bash_prefixed_and_bare_numeric_ids() {
        let ctx = empty_ctx();
        // 面板里显示的就是 `bash_1`，用户也可能只记得尾号
        assert!(matches!(
            ShellsCmd.run("bash_3", &ctx).await.unwrap(),
            CommandResult::OpenShellsPanel(Some(id)) if id == "bash_3"
        ));
        assert!(matches!(
            ShellsCmd.run("3", &ctx).await.unwrap(),
            CommandResult::OpenShellsPanel(Some(id)) if id == "bash_3"
        ));
    }

    #[tokio::test]
    async fn shells_garbage_arg_reports_error() {
        let ctx = empty_ctx();
        let result = ShellsCmd.run("not-a-shell", &ctx).await.unwrap();
        match result {
            CommandResult::Output(text) => assert!(text.contains("not-a-shell")),
            other => panic!("expected Output error text, got {other:?}"),
        }
    }

    #[test]
    fn shells_command_is_registered() {
        // CLAUDE.md 约定：新增 slash 命令必须在同一个注册表里出现，
        // 否则用户根本无从发现（/help 是唯一入口）。
        let reg = standard_registry();
        assert!(reg.get("shells").is_some(), "/shells 必须注册进命令表");
    }

    /// `/mouse` 必须注册，且 `/help` 里能看到它——它是用户发现这个开关的唯一入口。
    #[tokio::test]
    async fn mouse_command_is_registered_and_documented_in_help() {
        let reg = standard_registry();
        assert!(reg.get("mouse").is_some(), "/mouse 必须注册进命令表");

        let ctx = super::help_tests::ctx_with_dynamic(vec![]);
        let CommandResult::Output(text) = HelpCmd.run("", &ctx).await.unwrap() else {
            panic!("expected Output");
        };
        assert!(text.contains("/mouse"), "/help 必须列出 /mouse");
        assert!(
            text.contains("Shift+↑"),
            "/help 必须列出进入内容区的 Shift+↑"
        );
    }
}

#[cfg(test)]
mod github_tests {
    use super::{parse_github_slug, percent_encode};

    #[test]
    fn slug_parses_https_and_ssh_forms() {
        assert_eq!(
            parse_github_slug("https://github.com/wangyooujin/wyj-code.git").as_deref(),
            Some("wangyooujin/wyj-code")
        );
        assert_eq!(
            parse_github_slug("git@github.com:wangyooujin/wyj-code.git").as_deref(),
            Some("wangyooujin/wyj-code")
        );
        assert_eq!(
            parse_github_slug("https://github.com/owner/repo").as_deref(),
            Some("owner/repo")
        );
        assert_eq!(
            parse_github_slug("ssh://git@github.com/owner/repo.git").as_deref(),
            Some("owner/repo")
        );
        // 非 GitHub 远程返回 None
        assert_eq!(parse_github_slug("https://gitlab.com/o/r.git"), None);
        assert_eq!(parse_github_slug("not-a-url"), None);
    }

    #[test]
    fn percent_encode_escapes_reserved_chars() {
        assert_eq!(percent_encode("a b"), "a%20b");
        assert_eq!(percent_encode("x/y#z"), "x%2Fy%23z");
        // 未保留字符保持原样
        assert_eq!(percent_encode("Aa0-_.~"), "Aa0-_.~");
    }
}

#[cfg(test)]
mod cost_tests {
    use super::*;
    use crate::cost_view::{display_width, fmt_int, MIN_WIDTH};

    fn base_ctx() -> CommandContext {
        CommandContext {
            cwd: std::path::PathBuf::new(),
            // 单价表里有的模型，否则会走"无单价"分支，看不到金额。
            model: "claude-sonnet-4".to_string(),
            terminal_width: 64,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            api_calls: 0,
            prompt_cache_state: 0,
            context_window: 0,
            estimated_tokens: 0,
            context_audit: None,
            last_latency: Default::default(),
            compact_count: 0,
            context_edit: (0, 0),
            home_dir: std::path::PathBuf::new(),
            sub_input_tokens: 0,
            sub_output_tokens: 0,
            sub_cache_read_tokens: 0,
            sub_cache_write_tokens: 0,
            effective_mcp_count: 0,
            plugin_agent_paths: vec![],
            hooks_enabled: false,
            dynamic_commands: vec![],
            prompt_drift: None,
        }
    }

    /// 完整会话场景：主 Agent + 子 Agent + 缓存命中 + 未上报的缓存写入。
    fn full_ctx(width: usize) -> CommandContext {
        CommandContext {
            terminal_width: width,
            input_tokens: 31_845,
            output_tokens: 535,
            cache_read_tokens: 183_427,
            cache_write_tokens: 0,
            api_calls: 3,
            sub_input_tokens: 1_132,
            sub_output_tokens: 193,
            sub_cache_read_tokens: 19_968,
            context_window: 200_000,
            context_audit: Some(wyj_core::compact::ContextAudit {
                system_tokens: 6_400,
                tool_schema_tokens: 8_044,
                history_tokens: 40_738,
                output_reserve: 32_000,
                framing_tokens: 200,
                window: 0,
                reclaimable_tool_result_tokens: 0,
            }),
            ..base_ctx()
        }
    }

    async fn render(ctx: &CommandContext) -> String {
        match CostCmd.run("", ctx).await.unwrap() {
            CommandResult::OutputPlain(text) | CommandResult::Output(text) => text,
            other => panic!("expected Output(Plain), got {other:?}"),
        }
    }

    /// 行首连续空格数。
    fn indent_of(line: &str) -> usize {
        line.chars().take_while(|c| *c == ' ').count()
    }

    /// 脚注在窄终端会被截断，断言完整文案必然失败。这里只断言"这行存在"，
    /// 具体文案交给 i18n 单测。
    fn has_note(text: &str) -> bool {
        text.lines().any(|l| l.trim_start().starts_with('⚠'))
    }

    // ── 宽度自适应 ──────────────────────────────────────────────────────

    /// 核心回归：面板里**每一行**（标题、派生指标、占比条、脚注）都必须落在
    /// 可用列数内。旧实现只约束了表格，派生行在 44 列终端下冲到 75 列，
    /// 被聊天区换行拆散。
    #[tokio::test]
    async fn every_line_fits_the_advertised_width() {
        for width in [120usize, 100, 80, 64, 52, 44, 36, 32] {
            let text = render(&full_ctx(width)).await;
            for line in text.lines() {
                assert!(
                    display_width(line) <= width.max(MIN_WIDTH),
                    "宽度 {width} 下超宽 ({} 列): {line:?}",
                    display_width(line)
                );
            }
        }
    }

    /// 表格内所有带金额的行必须等宽——表格"齐不齐"全看这条。
    /// 用"含金额"来筛而不是"含缩进"：派生指标行也有缩进但宽度本就不同，
    /// 混进来会把断言变成永远失败的假阳性。
    #[tokio::test]
    async fn every_table_row_shares_one_width() {
        let text = render(&full_ctx(64)).await;
        let widths: Vec<usize> = text
            .lines()
            .filter(|l| l.contains("~$"))
            .map(display_width)
            .collect();
        assert!(widths.len() >= 7, "行数不对: {widths:?}\n{text}");
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "表格行宽不一致: {widths:?}\n{text}"
        );
    }

    /// 终端再窄也不得丢行——宁可截标签。
    #[tokio::test]
    async fn narrow_terminal_never_drops_rows() {
        for width in [MIN_WIDTH, 40, 52] {
            let text = render(&full_ctx(width)).await;
            // 窄终端下**标签**会被截断成 `Input (unc…`，所以不能断言完整文案；
            // 真正不能丢的是行本身——按"含金额的行数"数。
            let rows = text
                .lines()
                .filter(|l| l.contains("~$") || l.contains(&tr("cost.unknown")))
                .count();
            assert!(
                rows >= 6,
                "宽度 {width} 下丢了行（只剩 {rows} 行）:\n{text}"
            );
        }
    }

    // ── 未知值不得伪装成 0 ──────────────────────────────────────────────

    /// 未上报的缓存写入必须显示占位符，**不能**显示成 `~$0.0000`——后者
    /// 等于告诉用户"这项免费"，而它其实是未知（可能还要按 1.25x 补）。
    #[tokio::test]
    async fn unreported_cache_write_is_never_rendered_as_zero_cost() {
        let text = render(&full_ctx(64)).await;
        let write_row = text
            .lines()
            .find(|l| l.contains(&tr("cost.row_cache_write")))
            .expect("缺少缓存写入行");
        assert!(
            write_row.contains(&tr("cost.unknown")),
            "未上报项应显示占位符: {write_row}"
        );
        assert!(
            !write_row.contains("~$0.0000"),
            "未上报项不能显示成 0 费用: {write_row}"
        );
        assert!(has_note(&text), "缺少下界说明脚注:\n{text}");
    }

    /// 反向：写入量**真的**有值时不能误报"未上报"。
    #[tokio::test]
    async fn reported_cache_write_is_not_flagged() {
        let ctx = CommandContext {
            cache_write_tokens: 15_000,
            ..full_ctx(64)
        };
        let text = render(&ctx).await;
        assert!(!has_note(&text), "写入量正常时不该出现下界警告:\n{text}");
    }

    /// 没有缓存活动时不下"下界"警告——纯 input/output 的会话不该被无关
    /// 告警打扰。
    #[tokio::test]
    async fn no_cache_activity_means_no_lower_bound_warning() {
        let ctx = CommandContext {
            cache_read_tokens: 0,
            sub_cache_read_tokens: 0,
            ..full_ctx(64)
        };
        let text = render(&ctx).await;
        assert!(!has_note(&text), "无缓存活动时不该警告:\n{text}");
    }

    // ── 数字自洽 ────────────────────────────────────────────────────────

    /// 合计行必须等于各明细行之和——用户会拿它对账，对不上就是 bug。
    #[tokio::test]
    async fn total_row_equals_the_sum_of_detail_rows() {
        let text = render(&full_ctx(64)).await;
        let sum = 31_845u64 + 535 + 183_427;
        let total_row = text
            .lines()
            .find(|l| l.contains(&tr("cost.row_total")))
            .expect("缺少合计行");
        assert!(
            total_row.contains(&fmt_int(sum)),
            "合计行与明细和不符: {total_row}"
        );
    }

    /// 子 Agent 扇出要出现在面板里，且比主表明细多一层缩进。
    #[tokio::test]
    async fn subagent_usage_appears_one_level_deeper() {
        let text = render(&full_ctx(64)).await;
        assert!(text.contains(&tr("cost.subagent_section")), "{text}");
        let sub_row = text
            .lines()
            .find(|l| l.contains("19,968"))
            .expect("子 Agent 缓存行缺失");
        let main_row = text
            .lines()
            .find(|l| l.contains(&fmt_int(31_845)))
            .expect("主 Agent 输入行缺失");
        assert_eq!(
            indent_of(sub_row),
            indent_of(main_row) + 2,
            "子 Agent 行应比主表明细多一层缩进: sub={sub_row:?} main={main_row:?}"
        );
    }

    /// 没有任何子 Agent 用量时，整节都不出现。
    #[tokio::test]
    async fn subagent_section_is_omitted_when_unused() {
        let ctx = CommandContext {
            sub_input_tokens: 0,
            sub_output_tokens: 0,
            sub_cache_read_tokens: 0,
            ..full_ctx(64)
        };
        let text = render(&ctx).await;
        assert!(
            !text.contains(&tr("cost.subagent_section")),
            "无子 Agent 用量时不该出现该节:\n{text}"
        );
    }

    // ── 上下文占比条 ────────────────────────────────────────────────────

    /// 条要跟着 used/window 变化，且随终端变窄而收缩。
    #[tokio::test]
    async fn context_bar_tracks_usage_and_shrinks_when_narrow() {
        let cells = |s: &str| s.matches('█').count() + s.matches('░').count();
        let wide = render(&full_ctx(120)).await;
        let narrow = render(&full_ctx(36)).await;
        let wide_cells = wide.lines().find(|l| l.contains('█')).map(cells);
        let narrow_cells = narrow.lines().find(|l| l.contains('█')).map(cells);
        let (wide_cells, narrow_cells) = (wide_cells.unwrap_or(0), narrow_cells.unwrap_or(0));
        assert!(wide_cells > 0, "宽终端应有占比条:\n{wide}");
        assert!(
            narrow_cells < wide_cells,
            "窄终端的条应收缩: wide={wide_cells} narrow={narrow_cells}"
        );
    }

    /// 占比要反映真实占用，不是恒定值。
    #[tokio::test]
    async fn context_bar_reflects_the_real_ratio() {
        let low = render(&full_ctx(64)).await;
        let high = render(&CommandContext {
            context_audit: Some(wyj_core::compact::ContextAudit {
                system_tokens: 6_400,
                tool_schema_tokens: 8_044,
                history_tokens: 175_000,
                output_reserve: 32_000,
                framing_tokens: 200,
                window: 0,
                reclaimable_tool_result_tokens: 0,
            }),
            ..full_ctx(64)
        })
        .await;
        let filled = |s: &str| s.matches('█').count();
        assert!(
            filled(&high) > filled(&low),
            "占用更高时实心格应更多: high={} low={}",
            filled(&high),
            filled(&low)
        );
    }

    /// 拿不到窗口信息时整行省略，而不是画一个假的 0%。
    #[tokio::test]
    async fn context_section_is_omitted_without_a_window() {
        let ctx = CommandContext {
            context_window: 0,
            context_audit: None,
            last_latency: Default::default(),
            ..full_ctx(64)
        };
        let text = render(&ctx).await;
        assert!(
            !text.contains('█') && !text.contains('░'),
            "无窗口时不该画占比条:\n{text}"
        );
    }

    // ── 端点能力缺失时的诚实降级 ────────────────────────────────────────

    /// 单价表未覆盖：必须显式说明"没有金额"，且不出现任何金额。
    #[tokio::test]
    async fn missing_price_is_explained_instead_of_silent_zero() {
        let ctx = CommandContext {
            model: "some-unlisted-model".to_string(),
            ..full_ctx(64)
        };
        let text = render(&ctx).await;
        assert!(has_note(&text), "无单价时应有说明脚注:\n{text}");
        assert!(!text.contains("~$"), "无单价时不该出现金额:\n{text}");
        // token 数照常显示——没有单价不等于没有用量。
        assert!(
            text.contains(&fmt_int(31_845)),
            "无单价时也应显示 token 数:\n{text}"
        );
    }

    /// 缓存被端点拒绝而停用：与"未上报写入"是两回事，各出各的脚注。
    #[tokio::test]
    async fn downgraded_cache_is_reported_separately() {
        let ctx = CommandContext {
            prompt_cache_state: wyj_core::prompt_cache_state::DOWNGRADED,
            ..CommandContext {
                cache_read_tokens: 100_000,
                ..base_ctx()
            }
        };
        let text = render(&ctx).await;
        assert!(has_note(&text), "缓存降级应有自己的说明脚注:\n{text}");
        // 关键回归：缓存被端点拒绝而停用 ≠ 端点不报写入量。此时绝不能再套
        // "金额是下界"——那是两种完全不同的故障，混起来会误导用户。
        assert!(
            !text.contains(&tr("cost.unknown")),
            "缓存已停用时不该出现未上报占位符:\n{text}"
        );
    }

    // ── 单价表 ──────────────────────────────────────────────────────────

    /// `MiniMax-M3.1-Flash` 必须是 M3.1-Flash 的价（$0.15/M），**不能**被
    /// 宽前缀 `minimax-m3` 抢先命中而算成 $0.30/M——那是两倍的偏差，会让
    /// 「用了更便宜的辅助模型」在 `/cost` 里看起来反而更贵。
    ///
    /// 匹配是子串匹配、顺序即优先级，所以这条钉的是**表里的顺序**。
    #[test]
    fn minimax_m31_flash_is_priced_as_flash_not_as_m3() {
        let (input, output, _) = lookup_price("MiniMax-M3.1-Flash-Preview").expect("应有单价");
        assert!(
            (input - 0.15).abs() < f64::EPSILON,
            "M3.1-Flash 输入价应为 0.15，实际 {input}"
        );
        assert!(
            (output - 0.60).abs() < f64::EPSILON,
            "M3.1-Flash 输出价应为 0.60，实际 {output}"
        );
    }

    /// M3 自己的价不能被 M3.1-Flash 的条目抢走。
    #[test]
    fn minimax_m3_keeps_its_own_price() {
        let (input, output, _) = lookup_price("MiniMax-M3").expect("应有单价");
        assert!(
            (input - 0.30).abs() < f64::EPSILON,
            "M3 输入价应为 0.30，实际 {input}"
        );
        assert!(
            (output - 1.20).abs() < f64::EPSILON,
            "M3 输出价应为 1.20，实际 {output}"
        );
    }

    /// 缓存读取倍率是**按模型**的：M3.1-Flash 是 0.1x，M3 是 0.2x。
    /// 写死 0.1x 会把 M3 的缓存费用少算一半。
    #[test]
    fn cache_read_ratio_differs_between_minimax_models() {
        let (_, _, flash_ratio) = lookup_price("MiniMax-M3.1-Flash").expect("应有单价");
        let (_, _, m3_ratio) = lookup_price("MiniMax-M3").expect("应有单价");
        assert!((flash_ratio - 0.1).abs() < f64::EPSILON);
        assert!((m3_ratio - 0.2).abs() < f64::EPSILON);
    }

    /// 未知模型仍然查不到单价（不能因为放宽匹配而误命中）。
    #[test]
    fn unknown_model_still_has_no_price() {
        assert!(lookup_price("some-unlisted-model").is_none());
    }
}
