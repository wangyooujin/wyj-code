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

/// 按显示宽度折行（**不截断**），用于诊断面板里那些长解释文本。
///
/// 为什么不用截断：`/computer` 的权限提示是「截图正常但操作无反应该去改
/// 哪个系统设置」，砍掉尾巴等于没说。折行则信息完整，只是多占两行。
///
/// 断点规则：ASCII 单词尽量整词不拆（行内最后一个空格处回退，但仅在回退
/// 后仍能占满一半宽度时才回退，否则宁可拆词也别把行弄成一两个词），中文
/// 没有空格，逐字断——与 TUI 的 markdown 折行取向一致。
pub fn wrap_text(s: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for raw in s.split('\n') {
        if raw.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut line = String::new();
        let mut used = 0usize;
        for c in raw.chars() {
            let cw = c.width().unwrap_or(0);
            if used + cw > width && !line.is_empty() {
                if let Some(pos) = line.rfind(' ') {
                    if display_width(&line[..pos]) * 2 >= width {
                        let rest = line[pos + 1..].to_string();
                        line.truncate(pos);
                        out.push(std::mem::take(&mut line));
                        line = rest;
                        used = display_width(&line);
                        line.push(c);
                        used += cw;
                        continue;
                    }
                }
                out.push(std::mem::take(&mut line));
                used = 0;
            }
            line.push(c);
            used += cw;
        }
        if !line.is_empty() {
            out.push(line);
        }
    }
    out
}

/// 诊断面板一行状态的严重级别。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    Ok,
    Warn,
    Err,
    Info,
}

impl Level {
    /// 图标列必须**恰好 1 列**。
    ///
    /// 旧版 `/computer` 用的是 `✓` / `✗` / `ℹ️`，而 `ℹ️` 带 U+FE0F 变体
    /// 选择符：终端按 emoji 呈现占 2 列，`unicode-width` 只算 1 列。于是凡
    /// 是 info 行，后面的标签都右移一格，整块面板看起来「一团乱」。这里
    /// 只留确定宽度 1 列的符号，宽度口径才和实际渲染一致。
    fn icon(self) -> &'static str {
        match self {
            Level::Ok => "✓",
            Level::Warn => "!",
            Level::Err => "✗",
            Level::Info => "•",
        }
    }
}

/// 诊断面板的一行：级别 + 标签 + 值。
pub struct Diag {
    level: Level,
    label: String,
    value: String,
}

enum DiagRow {
    /// 分段标题（不带图标，上方留一空行）。
    Section(String),
    Item(Diag),
}

/// 诊断面板：`/computer` 这类「一串状态 + 解释」的输出。
///
/// **为什么不用 [`Table`]**：那张表的第二列是右对齐数字、第三列是费用，
/// 语义对不上诊断面板。**为什么排版不能写在 i18n 模板里**：旧模板靠
/// `"  ✓  平台        {os}"` 里的空格凑列宽，标签一长一短就散架，而且
/// 值再长会一路冲出聊天区（预排版消息**不折行**，只截断）。
///
/// 本模块只负责列宽与折行，文案由调用方本地化后传入。
pub struct DiagPanel {
    title: String,
    rows: Vec<DiagRow>,
    /// 尾部提示：单独成段（前空一行），按可用宽度折行而不是截断。
    hint: Option<(String, String)>,
}

/// 值列至少保留的列数。低于这个宽度时宁可截断标签，也不能把值挤成一条竖线。
const MIN_VALUE_W: usize = 20;

impl DiagPanel {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            rows: Vec::new(),
            hint: None,
        }
    }

    pub fn row(mut self, level: Level, label: impl Into<String>, value: impl Into<String>) -> Self {
        self.rows.push(DiagRow::Item(Diag {
            level,
            label: label.into(),
            value: value.into(),
        }));
        self
    }

    /// 分段标题。十几行状态平铺读起来是一堵墙，按「配置 / 权限 / 实时探测」
    /// 分组后每段只有三四行，扫读成本低一个量级。
    pub fn section(mut self, title: impl Into<String>) -> Self {
        self.rows.push(DiagRow::Section(title.into()));
        self
    }

    /// 尾部提示段（`label` 作为首行前缀，如「提示」）。
    pub fn hint(mut self, label: impl Into<String>, text: impl Into<String>) -> Self {
        self.hint = Some((label.into(), text.into()));
        self
    }

    fn label_width(&self, avail: usize) -> usize {
        let fixed = display_width(INDENT) + 1 + GAP + GAP;
        let natural = self
            .rows
            .iter()
            .filter_map(|r| match r {
                DiagRow::Item(d) => Some(display_width(&d.label)),
                DiagRow::Section(_) => None,
            })
            .chain(self.hint.iter().map(|(l, _)| display_width(l)))
            .max()
            .unwrap_or(0);
        let cap = avail.saturating_sub(fixed + MIN_VALUE_W).max(MIN_LABEL_W);
        natural.clamp(MIN_LABEL_W, cap)
    }

    /// 渲染成多行文本。`width` = 可用列数（0 = 未知，走 [`DEFAULT_WIDTH`]）。
    pub fn render(&self, width: usize) -> String {
        let avail = normalize(width);
        let indent_w = display_width(INDENT);
        let label_w = self.label_width(avail);
        // 行 = 缩进 + 图标(1) + GAP + 标签 + GAP + 值
        let value_start = indent_w + 1 + GAP + label_w + GAP;
        let value_budget = avail.saturating_sub(value_start).max(1);

        let mut out = String::new();
        out.push_str(INDENT);
        out.push_str(&fit_line(&self.title, avail.saturating_sub(indent_w)));
        out.push('\n');

        // 标签+值折行的公共渲染（首行接在已写好的前缀之后）。
        let emit_value = |out: &mut String, value: &str| {
            let wrapped = wrap_text(value, value_budget);
            match wrapped.split_first() {
                None => out.push('\n'),
                Some((first, rest)) => {
                    out.push_str(first);
                    out.push('\n');
                    // 续行对齐到值列起点：折行后的文本仍属于同一个标签，
                    // 顶格会让读者以为那是另一行状态。
                    for cont in rest {
                        out.push_str(&" ".repeat(value_start));
                        out.push_str(cont);
                        out.push('\n');
                    }
                }
            }
        };

        for row in &self.rows {
            match row {
                DiagRow::Section(title) => {
                    if !out.ends_with("\n\n") {
                        out.push('\n');
                    }
                    out.push_str(INDENT);
                    out.push_str(&truncate_to_width(title, avail.saturating_sub(indent_w)));
                    out.push('\n');
                }
                DiagRow::Item(d) => {
                    out.push_str(INDENT);
                    out.push_str(d.level.icon());
                    out.push_str(&" ".repeat(GAP));
                    out.push_str(&pad_end(&d.label, label_w));
                    out.push_str(&" ".repeat(GAP));
                    emit_value(&mut out, &d.value);
                }
            }
        }

        if let Some((label, text)) = &self.hint {
            out.push('\n');
            out.push_str(INDENT);
            out.push_str(Level::Info.icon());
            out.push_str(&" ".repeat(GAP));
            out.push_str(&pad_end(label, label_w));
            out.push_str(&" ".repeat(GAP));
            emit_value(&mut out, text);
        }

        out
    }
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

    /// 图标列必须恒为 1 列。`ℹ️` 这类带 U+FE0F 的符号终端按 2 列渲染、
    /// `unicode-width` 只算 1 列，是旧版 `/computer` 标签列错位的根因。
    #[test]
    fn every_level_icon_is_exactly_one_column() {
        for level in [Level::Ok, Level::Warn, Level::Err, Level::Info] {
            assert_eq!(
                display_width(level.icon()),
                1,
                "{level:?} 的图标宽度不是 1 列，标签列会整体错位"
            );
        }
    }

    fn diag_sample() -> DiagPanel {
        DiagPanel::new("wyj-code computer-use 诊断")
            .section("配置与注册")
            .row(Level::Ok, "平台", "macos（支持）")
            .row(Level::Err, "工具注册", "未启用（当前 provider=openai）")
            .section("后台通道")
            .row(Level::Ok, "辅助功能", "已授权")
    }

    /// 分段标题上方留空行、不加图标：否则读起来和状态行混在一起。
    #[test]
    fn diag_sections_are_separated_by_a_blank_line() {
        let out = diag_sample().render(72);
        assert!(out.contains("\n\n  配置与注册\n"), "{out}");
        assert!(out.contains("\n\n  后台通道\n"), "{out}");
        assert!(
            !out.lines().any(|l| l.trim_start().starts_with("✓ 配置")),
            "分段标题不该带状态图标:\n{out}"
        );
    }

    /// 所有数据行的值列必须**从同一列开始**——这是「一团乱」的直接判据。
    #[test]
    fn diag_rows_align_their_value_column() {
        let rows = [
            ("平台", "macos（支持）"),
            ("工具注册", "未启用（当前 provider=openai）"),
            ("辅助功能", "已授权"),
        ];
        let mut panel = DiagPanel::new("诊断");
        for (label, value) in rows {
            panel = panel.row(Level::Ok, label, value);
        }
        let out = panel.render(72);

        let value_cols: Vec<usize> = rows
            .iter()
            .map(|(_, value)| {
                let line = out
                    .lines()
                    .find(|l| l.contains(value))
                    .unwrap_or_else(|| panic!("找不到值 {value:?}:\n{out}"));
                display_width(&line[..line.find(value).unwrap()])
            })
            .collect();
        assert!(
            value_cols.windows(2).all(|w| w[0] == w[1]),
            "值列起点不一致（{value_cols:?}），面板会错位:\n{out}"
        );
    }

    /// 窄终端下整块面板不得超宽——预排版消息**不折行**，超宽就是右侧被切掉。
    #[test]
    fn diag_panel_never_exceeds_available_width() {
        let long = "未授权；已请求系统授权框，请在 系统设置 → 隐私与安全性 → 辅助功能 中启用 wyj-code 后重跑 /computer";
        let panel = DiagPanel::new("wyj-code computer-use 诊断")
            .section("后台通道")
            .row(Level::Err, "辅助功能", long)
            .hint("注意", long);
        for width in [MIN_WIDTH, 40, 64, 100, 200] {
            let out = panel.render(width);
            for line in out.lines() {
                assert!(
                    display_width(line) <= normalize(width),
                    "宽度 {width} 下超宽（{} 列）: {line:?}",
                    display_width(line)
                );
            }
        }
    }

    /// 长解释必须**折行**而不是截断：提示文字砍掉尾巴等于没说。
    #[test]
    fn diag_hint_wraps_instead_of_truncating() {
        let text = "以上「输入自检」只验证读取光标位置成功，不代表点击/按键一定生效。";
        let out = DiagPanel::new("诊断").hint("注意", text).render(40);
        assert!(!out.contains('…'), "提示段被截断了，应当折行:\n{out}");
        // 折行后的续行必须与首行**连续**：把整块面板去掉空白后拼起来，
        // 正文应当完整出现（标题与标签在正文之前，不影响 contains 判定）。
        let joined: String = out
            .lines()
            .flat_map(|l| l.chars().filter(|c| !c.is_whitespace()))
            .collect();
        let expected: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            joined.contains(&expected),
            "折行后正文丢了字符:\n{out}\n--- 期望包含 ---\n{expected}"
        );
    }

    /// 折行不能拆坏 CJK：每行宽度必须达标，且不出现只剩一两个字撑满一行的碎块。
    #[test]
    fn wrap_text_keeps_cjk_lines_within_budget() {
        let text = "枚举到 1 块屏；区域截图按矩形所在的那块屏定位，副屏原点可以为负。";
        for width in [12, 20, 33, 64] {
            let lines = wrap_text(text, width);
            assert!(!lines.is_empty());
            for line in &lines {
                assert!(
                    display_width(line) <= width,
                    "宽度 {width} 下折出超宽行: {line:?}"
                );
            }
            let joined: String = lines.concat();
            assert_eq!(
                joined.chars().filter(|c| !c.is_whitespace()).count(),
                text.chars().filter(|c| !c.is_whitespace()).count(),
                "折行丢了字符: {lines:?}"
            );
        }
    }
}
