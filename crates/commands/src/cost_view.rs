//! `/cost` 的排版引擎。
//!
//! **为什么排版不能写在 i18n 模板里**：旧实现把对齐硬编码进翻译串
//! （zh: `"  输入:   {input} tokens"`），靠中文字符恰好占 2 列来凑齐——
//! 切到英文立刻散架（`Input` 有缩进而 `Output`/`Total` 没有）。表格的列宽
//! 是**排版问题**，不是翻译问题：标签只负责"叫什么"，宽度由本模块按显示
//! 宽度（`unicode-width`，与 tui 渲染同版本）算，中英文都能对齐。
//!
//! 宽度自适应：调用方传入聊天区可用列数（`CommandContext::terminal_width`，
//! 0 = 未知走 [`DEFAULT_WIDTH`]）。窄终端下先压缩标签列，再退化为省略号，
//! 数字列永远保底——它们才是用户真正要读的数。
//!
//! 本模块**不含任何 i18n 调用**：需要翻译的短串由调用方（`builtin.rs`）
//! 传进来。文案与排版的职责边界就在这里——文案归 i18n，缩进/列宽/补位归这里。

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// 终端宽度未知时的兜底列数（80 列终端去掉边框与缩进后的常见值）。
pub const DEFAULT_WIDTH: usize = 64;
/// 可用列数下限。再窄就没法读了，此时宁可截断标签。
pub const MIN_WIDTH: usize = 32;
/// 标签列的最小宽度；低于此值说明终端太窄，标签会被截断成省略号。
const MIN_LABEL_W: usize = 8;
/// 表格缩进（与聊天区 assistant 消息的左缩进一致）。
pub const INDENT: &str = "  ";
/// 列间距。
const GAP: usize = 2;
/// 上下文占比条的格数上限。
const BAR_MAX_CELLS: usize = 24;
/// 有效宽度的上限：超宽终端下不让表格拉成一行几百列的散沙。
const MAX_WIDTH: usize = 160;

/// 字符串的终端显示列数（CJK / 全角算 2 列）。
pub fn display_width(s: &str) -> usize {
    s.width()
}

/// 把调用方给的宽度归一化：`0`（未知）走 [`DEFAULT_WIDTH]`，其余夹进
/// `[MIN_WIDTH, MAX_WIDTH]`。
///
/// 排版入口应当**先调一次**它拿到 `width`，之后所有子部件都直接吃这个值——
/// 否则每个子部件各自夹一次，缩进、列宽、文本预算会在不同的地方被夹到
/// 不同边界，凑起来反而超宽。
pub fn usable_width(width: usize) -> usize {
    if width == 0 {
        DEFAULT_WIDTH.clamp(MIN_WIDTH, MAX_WIDTH)
    } else {
        width.clamp(MIN_WIDTH, MAX_WIDTH)
    }
}

/// 把宽度夹到 `[MIN_WIDTH, MAX_WIDTH]`，0/未知走 [`DEFAULT_WIDTH`]。
fn normalize(width: usize) -> usize {
    usable_width(width)
}

/// 普通文本行按**调用方给的精确预算**收口（放得下原样返回，超出才截断）。
///
/// 这里刻意**不**再归一化：调用方往往已经扣掉了缩进/前缀（`INDENT` + `⚠ `），
/// 若这里再把预算抬回 `MIN_WIDTH`，拼回去就必然超宽（实测 32 列终端下
/// 派生指标行宽 34）。
pub fn fit_line(s: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if display_width(s) <= width {
        s.to_string()
    } else {
        truncate_to_width(s, width)
    }
}

/// 按显示宽度左对齐补齐到 `width` 列；超出则截断并加省略号。
pub fn pad_end(s: &str, width: usize) -> String {
    let w = display_width(s);
    if w == width {
        return s.to_string();
    }
    if w < width {
        return format!("{s}{}", " ".repeat(width - w));
    }
    truncate_to_width(s, width)
}

/// 按显示宽度截断到 `width` 列（省略号本身占 1 列）。
pub fn truncate_note(s: &str, width: usize) -> String {
    truncate_to_width(s, width)
}

/// 按显示宽度截断到 `width` 列（省略号本身占 1 列）。
///
/// **放得下就原样返回**：这个函数被分节标题、脚注、标签共用，无条件追加
/// 省略号会让每一行标题都挂个假的「截断痕迹」（实测分节标题明明只有 27 列
/// 却在 62 列预算下也挂了 `…`）。
fn truncate_to_width(s: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if display_width(s) <= width {
        return s.to_string();
    }
    let budget = width.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0usize;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if used + cw > budget {
            break;
        }
        out.push(c);
        used += cw;
    }
    out.push('…');
    out
}

/// 按显示宽度右对齐补齐（数字列）。超宽时**原样返回**——数字宁可破坏
/// 对齐也不能被截错。
pub fn pad_start(s: &str, width: usize) -> String {
    let w = display_width(s);
    if w >= width {
        return s.to_string();
    }
    format!("{}{s}", " ".repeat(width - w))
}

/// 千分位分隔：`1234567` → `1,234,567`。
pub fn fmt_int(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    let bytes = digits.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// 费用格式化。固定 4 位小数——塌成 `0.0000` 会被误读成"没花钱"。
pub fn fmt_cost(v: f64) -> String {
    format!("~${v:.4}")
}

/// 百分比：固定 0 位小数。
pub fn fmt_pct(v: f64) -> String {
    format!("{v:.0}%")
}

/// 一行：标签 + token 数 + 费用。
pub struct Row {
    pub label: String,
    /// 预格式化的 token 数（右对齐列）。
    pub tokens: String,
    /// 预格式化的费用源值（右对齐列），`None` = 该端点不上报此项。
    pub cost: Option<f64>,
    /// 分组小标题：非空时该行渲染为一个分段标题而非数据行。
    pub section: Option<String>,
    /// 汇总行：渲染前自动插一条横线与明细行分隔。
    pub is_total: bool,
    /// 额外缩进层数（每层 2 列）。子 Agent 分节里的行用它表示"从属"，
    /// 而不是在 i18n 标签里塞前导空格——那会让对齐随文案变化而漂移。
    pub indent: usize,
}

impl Row {
    pub fn new(label: impl Into<String>, tokens: u64, cost: Option<f64>) -> Self {
        Self {
            label: label.into(),
            tokens: fmt_int(tokens),
            cost,
            section: None,
            is_total: false,
            indent: 0,
        }
    }

    /// 该行从属于上一节（如"子 Agent 扇出"），标签额外缩进一层。
    pub fn indented(mut self) -> Self {
        self.indent = 1;
        self
    }

    /// 汇总行。`tokens` 应等于上方各明细行之和——`/cost` 用它让"这些数字
    /// 加起来是多少"一眼可验，而不是让用户自己心算。
    pub fn total(label: impl Into<String>, tokens: u64, cost: Option<f64>) -> Self {
        Self {
            is_total: true,
            ..Self::new(label, tokens, cost)
        }
    }

    /// 追加一个分段标题行（不是数据行）。
    pub fn section(label: impl Into<String>) -> Self {
        Self {
            label: String::new(),
            tokens: String::new(),
            cost: None,
            section: Some(label.into()),
            is_total: false,
            indent: 0,
        }
    }
}

/// 已算好列宽的表格，负责按可用宽度渲染。
pub struct Table {
    rows: Vec<Row>,
    /// 未知/不适用项在费用列显示的占位符（由调用方本地化后传入）。
    unknown: String,
    /// 表头三列：`(标签列名, 数字列名, 费用列名)`。
    header: Option<(String, String, String)>,
}

impl Table {
    pub fn new(rows: Vec<Row>, unknown: impl Into<String>) -> Self {
        Self {
            rows,
            unknown: unknown.into(),
            header: None,
        }
    }

    pub fn with_header(
        mut self,
        label: impl Into<String>,
        tokens: impl Into<String>,
        cost: impl Into<String>,
    ) -> Self {
        self.header = Some((label.into(), tokens.into(), cost.into()));
        self
    }

    fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// 参与列宽计算的行（分段标题与空表头不参与）。
    fn data_rows(&self) -> impl Iterator<Item = &Row> {
        self.rows.iter().filter(|r| r.section.is_none())
    }

    fn token_w(&self) -> usize {
        let from_rows = self
            .data_rows()
            .map(|r| display_width(&r.tokens))
            .max()
            .unwrap_or(1);
        let from_header = self
            .header
            .as_ref()
            .map(|(_, t, _)| display_width(t))
            .unwrap_or(0);
        from_rows.max(from_header).max(1)
    }

    /// 费用列宽。全表没有任何费用、且占位符为空时返回 0 —— 表示**这一列
    /// 不存在**（`/context` 只有 token 数），此时不能保留空列，否则每行都
    /// 会多出一截行尾空白。
    fn cost_w(&self) -> usize {
        let from_rows = self
            .data_rows()
            .map(|r| r.cost.map(fmt_cost).map_or(0, |c| display_width(&c)))
            .max()
            .unwrap_or(0);
        let from_header = self
            .header
            .as_ref()
            .map(|(_, _, c)| display_width(c))
            .unwrap_or(0);
        let widest = from_rows.max(from_header).max(display_width(&self.unknown));
        if widest == 0 {
            0
        } else {
            widest.max(1)
        }
    }

    /// 标签列宽 = 标签自然宽度（含缩进），但绝不侵占数字列的空间，也绝不
    /// 小于 [`MIN_LABEL_W`]（保底给最小可读宽度，溢出部分截断）。
    fn label_w(&self, available: usize) -> usize {
        let natural = self
            .data_rows()
            .map(|r| display_width(&r.label) + r.indent * GAP)
            .max()
            .unwrap_or(0);
        let from_header = self
            .header
            .as_ref()
            .map(|(l, _, _)| display_width(l))
            .unwrap_or(0);
        natural
            .max(from_header)
            .clamp(MIN_LABEL_W, available.max(MIN_LABEL_W))
    }

    fn cost_cell(&self, row: &Row) -> String {
        match row.cost {
            Some(v) => pad_start(&fmt_cost(v), self.cost_w()),
            None => pad_start(&self.unknown, self.cost_w()),
        }
    }

    /// 渲染成多行文本（不含整体标题）。`width` = 可用列数。
    pub fn render(&self, width: usize) -> String {
        if self.is_empty() {
            return String::new();
        }
        let avail = normalize(width);
        let indent_w = display_width(INDENT);
        let tw = self.token_w();
        let cw = self.cost_w();
        // 先给数字列留够，标签列拿剩下的。注意非标签部分有**两个** GAP：
        // 行 = 缩进 + 标签 + GAP + 数字 + GAP + 费用。漏掉一个会让整表
        // 在最窄宽度下溢出 avail（实测 32 列时渲染出 34 列）。
        let fixed = if cw == 0 {
            indent_w + GAP + tw
        } else {
            indent_w + GAP + tw + GAP + cw
        };
        let label_w = self.label_w(avail.saturating_sub(fixed));
        // `fixed` 已含缩进，所以 `fixed + label_w` 就是**整行**宽度。
        // 横线是缩进之后才打印的，长度必须扣掉缩进，否则行宽比 avail 多
        // `indent_w` 列（实测 36 列终端下横线行宽 38）。
        let line_w = (fixed + label_w).min(avail);
        let rule = "─".repeat(line_w.saturating_sub(indent_w));

        let mut out = String::new();
        let emit_rule = |out: &mut String| {
            out.push_str(INDENT);
            out.push_str(&rule);
            out.push('\n');
        };

        if let Some((h_label, h_tokens, h_cost)) = &self.header {
            out.push_str(INDENT);
            out.push_str(&pad_end(h_label, label_w));
            out.push_str(&" ".repeat(GAP));
            out.push_str(&pad_start(h_tokens, tw));
            if cw > 0 {
                out.push_str(&" ".repeat(GAP));
                out.push_str(&pad_start(h_cost, cw));
            }
            out.push('\n');
            emit_rule(&mut out);
        }

        for row in &self.rows {
            if let Some(section) = &row.section {
                if !out.is_empty() && !out.ends_with("\n\n") {
                    out.push('\n');
                }
                out.push_str(INDENT);
                out.push_str(&truncate_to_width(section, avail.saturating_sub(indent_w)));
                out.push('\n');
                emit_rule(&mut out);
                continue;
            }
            if row.is_total {
                emit_rule(&mut out);
            }
            out.push_str(INDENT);
            if row.indent > 0 {
                out.push_str(&" ".repeat(row.indent * GAP));
            }
            out.push_str(&pad_end(
                &row.label,
                label_w.saturating_sub(row.indent * GAP),
            ));
            out.push_str(&" ".repeat(GAP));
            out.push_str(&pad_start(&row.tokens, tw));
            if cw > 0 {
                out.push_str(&" ".repeat(GAP));
                out.push_str(&self.cost_cell(row));
            }
            out.push('\n');
        }
        out
    }

    /// 表体去掉首尾换行（便于与其他片段拼接）。
    pub fn render_block(&self, width: usize) -> String {
        self.render(width).trim_end_matches('\n').to_string()
    }
}

/// 上下文占比条：`[██████░░░░░░░░░░░]`。
///
/// 格子数按可用宽度自适应并封顶 [`BAR_MAX_CELLS`]——占比条是用来"一眼看到
/// 还剩多少"的，不该反过来把面板撑宽。`window == 0`（拿不到窗口）返回
/// `None`，由调用方省略整行。`full`/`empty` 由调用方本地化后传入；`suffix`
/// 为空串时只出条不追加空格（条后由调用方接自己的文案，如百分比与绝对值）。
pub fn context_bar(
    used: u32,
    window: u32,
    width: usize,
    full: &str,
    empty: &str,
    suffix: &str,
) -> Option<String> {
    if window == 0 {
        return None;
    }
    let ratio = (used as f64 / window as f64 * 100.0).clamp(0.0, 100.0);
    let avail = normalize(width);
    let suffix_w = if suffix.is_empty() {
        0
    } else {
        1 + display_width(suffix)
    };
    let cells = avail
        .saturating_sub(display_width(INDENT) + 2 + suffix_w)
        .clamp(4, BAR_MAX_CELLS);
    let filled = (((ratio / 100.0) * cells as f64).round() as usize).min(cells);
    let full_w = display_width(full).max(1);
    let empty_w = display_width(empty).max(1);
    let bar = format!(
        "[{}{}]",
        full.repeat(filled.div_ceil(full_w)),
        empty.repeat((cells - filled).div_ceil(empty_w))
    );
    let tail = if suffix.is_empty() {
        String::new()
    } else {
        format!(" {suffix}")
    };
    Some(format!("{INDENT}{bar}{tail}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_rows() -> Vec<Row> {
        vec![
            Row::new("输入（未缓存）", 31_845, Some(0.0096)),
            Row::new("输出", 535, Some(0.0006)),
            Row::new("缓存命中", 183_427, Some(0.0055)),
            Row::new("缓存写入", 0, None),
        ]
    }

    #[test]
    fn fmt_int_groups_by_thousands() {
        assert_eq!(fmt_int(0), "0");
        assert_eq!(fmt_int(999), "999");
        assert_eq!(fmt_int(1_000), "1,000");
        assert_eq!(fmt_int(1_234_567), "1,234,567");
        assert_eq!(fmt_int(183_427_354), "183,427,354");
    }

    #[test]
    fn pad_end_counts_cjk_as_two_columns() {
        // 「输入」显示宽度 4，不是 2 —— 对齐算错就会在这里露馅
        assert_eq!(display_width("输入"), 4);
        assert_eq!(display_width("Input"), 5);
        assert_eq!(display_width(&pad_end("输入", 10)), 10);
        assert_eq!(display_width(&pad_end("Cache read", 10)), 10);
    }

    #[test]
    fn truncate_respects_display_width_not_char_count() {
        let t = truncate_to_width("缓存写入量", 6);
        assert!(display_width(&t) <= 6, "got width {}", display_width(&t));
        assert!(t.ends_with('…'));
    }

    /// 回归：放得下就**不能**挂省略号。这个函数被分节标题/脚注/标签共用，
    /// 无条件追加 `…` 会让每一行标题都带上假的截断痕迹。
    #[test]
    fn truncate_leaves_short_enough_text_untouched() {
        let s = "子 Agent 扇出（已计入合计）";
        assert_eq!(truncate_to_width(s, 62), s);
        assert_eq!(truncate_to_width(s, display_width(s)), s);
        assert!(!truncate_to_width(s, 62).ends_with('…'));
        assert!(truncate_to_width(s, 10).ends_with('…'));
    }

    #[test]
    fn pad_start_right_aligns_numbers_and_never_truncates_them() {
        assert_eq!(pad_start("7", 5), "    7");
        assert_eq!(pad_start("1,234", 5), "1,234");
        assert_eq!(pad_start("1,234,567", 5), "1,234,567");
    }

    #[test]
    fn every_data_row_shares_one_total_width() {
        let t = Table::new(sample_rows(), "未上报");
        let out = t.render(64);
        let widths: Vec<usize> = out
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(display_width)
            .collect();
        assert!(widths.len() >= 4, "{out}");
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "行宽不一致，表格是散的: {widths:?}\n{out}"
        );
    }

    #[test]
    fn narrow_terminal_keeps_all_rows_and_respects_width() {
        let t = Table::new(sample_rows(), "未上报");
        for width in [MIN_WIDTH, 36, 40, 50, 64, 100, 200] {
            let out = t.render(width);
            let lines = out.lines().filter(|l| !l.trim().is_empty()).count();
            assert_eq!(lines, 4, "宽度 {width} 下丢了行:\n{out}");
            for line in out.lines() {
                assert!(
                    display_width(line) <= normalize(width),
                    "宽度 {width} 下超宽: {line:?}"
                );
            }
        }
    }

    #[test]
    fn unknown_cost_shows_placeholder_not_a_fake_zero() {
        let out = Table::new(sample_rows(), "未上报").render(64);
        let write_line = out
            .lines()
            .find(|l| l.contains("缓存写入"))
            .expect("缺少缓存写入行");
        assert!(write_line.contains("未上报"), "{write_line}");
        assert!(
            !write_line.contains("~$0.0000"),
            "未知项不能显示成 0 费用: {write_line}"
        );
    }

    #[test]
    fn section_row_renders_as_a_labelled_rule() {
        let mut rows = vec![Row::new("输入", 100, Some(0.001))];
        rows.push(Row::section("子 Agent 扇出（已计入上方合计）"));
        rows.push(Row::new("  输入", 50, Some(0.0005)));
        let out = Table::new(rows, "—").render(64);
        assert!(out.contains("子 Agent 扇出"), "{out}");
        assert!(out.contains('─'), "分段标题下应有横线: {out}");
    }

    #[test]
    fn context_bar_width_scales_and_stays_in_bounds() {
        for width in [MIN_WIDTH, 40, 64, 100, 200] {
            let bar = context_bar(50_000, 200_000, width, "█", "░", "25%").unwrap();
            assert!(
                display_width(&bar) <= normalize(width),
                "宽度 {width} 下条超宽: {bar:?}"
            );
            assert!(bar.contains("25%"), "{bar}");
        }
    }

    #[test]
    fn context_bar_reflects_ratio_in_filled_cells() {
        let half = context_bar(50_000, 100_000, 80, "█", "░", "50%").unwrap();
        let full = context_bar(100_000, 100_000, 80, "█", "░", "100%").unwrap();
        assert!(full.matches('█').count() > half.matches('█').count());
        let none_bar = context_bar(100_000, 100_000, 80, "█", "░", "100%").unwrap();
        assert!(!none_bar.contains('░'), "100% 不该留空格: {none_bar}");
    }

    #[test]
    fn context_bar_is_none_without_window() {
        assert!(context_bar(100, 0, 64, "█", "░", "10%").is_none());
    }
}
