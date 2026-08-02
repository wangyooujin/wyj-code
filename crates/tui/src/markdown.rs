//! Markdown → ratatui `Line<'static>` 渲染器
//!
//! 支持：表格（box-drawing 对齐）、标题、代码块、列表、粗体/斜体、块引用、分隔线。

use crate::theme::Theme;
use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

// ── 字符宽度 ──────────────────────────────────────────────────────────────────

/// 与 ratatui 保持一致：用 unicode-width 0.2 计算终端显示列数
pub fn display_width(s: &str) -> usize {
    s.width()
}

/// 按显示宽度换行（不截断，不加省略号），供代码块和表格单元格使用，
/// 避免长内容被裁剪导致展示不完整。
fn wrap_dw(s: &str, max: usize) -> Vec<String> {
    if max == 0 {
        return vec![s.to_string()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut w = 0usize;
    for c in s.chars() {
        let cw = c.width().unwrap_or(1);
        if w + cw > max && w > 0 {
            out.push(std::mem::take(&mut cur));
            w = 0;
        }
        cur.push(c);
        w += cw;
    }
    out.push(cur);
    out
}

/// 按显示宽度 padding 到目标宽度（对齐）
fn pad_dw(s: &str, target: usize, align: Alignment) -> String {
    let w = display_width(s);
    let pad = target.saturating_sub(w);
    match align {
        Alignment::Right => format!("{}{}", " ".repeat(pad), s),
        Alignment::Center => {
            let l = pad / 2;
            let r = pad - l;
            format!("{}{}{}", " ".repeat(l), s, " ".repeat(r))
        }
        _ => format!("{}{}", s, " ".repeat(pad)),
    }
}

// ── 表格渲染 ──────────────────────────────────────────────────────────────────

fn table_border(widths: &[usize], l: char, m: char, r: char, f: char) -> String {
    let mut s = String::from(l);
    for (i, &w) in widths.iter().enumerate() {
        for _ in 0..w + 2 {
            s.push(f);
        }
        if i + 1 < widths.len() {
            s.push(m);
        }
    }
    s.push(r);
    s
}

fn table_cell_width(cell: &str) -> usize {
    cell.split('\n').map(display_width).max().unwrap_or(0)
}

/// 将列宽压进可用宽度。优先保留窄列，把剩余空间分配给真正需要的宽列，
/// 避免旧实现按平均值一刀切后浪费空间、再被 `Paragraph` 二次折行。
fn fit_table_widths(widths: &mut [usize], max_width: usize) {
    if widths.is_empty() {
        return;
    }

    // 每列左右各 1 个空格，另有 n + 1 根竖线。
    let frame_width = widths.len() * 3 + 1;
    let content_budget = max_width.saturating_sub(frame_width);
    let natural_total = widths.iter().sum::<usize>();
    if natural_total <= content_budget {
        return;
    }

    // 正常窗口尽量给每列至少 4 列；极窄窗口则均分已有空间。
    let min_width = if content_budget >= widths.len() * 4 {
        4
    } else {
        (content_budget / widths.len()).max(1)
    };
    let wanted = widths
        .iter()
        .map(|width| (*width).max(min_width))
        .collect::<Vec<_>>();

    // 找出最大的公平列宽上限，使总宽仍不超过预算。
    let mut low = min_width;
    let mut high = wanted.iter().copied().max().unwrap_or(min_width);
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        let used = wanted.iter().map(|width| (*width).min(mid)).sum::<usize>();
        if used <= content_budget {
            low = mid;
        } else {
            high = mid - 1;
        }
    }

    for (width, wanted) in widths.iter_mut().zip(&wanted) {
        *width = (*wanted).min(low);
    }

    // 二分上限可能留下少量余宽，按列补回，尽量用满而不越界。
    let mut remaining = content_budget.saturating_sub(widths.iter().sum::<usize>());
    while remaining > 0 {
        let mut changed = false;
        for (width, wanted) in widths.iter_mut().zip(&wanted) {
            if *width < *wanted {
                *width += 1;
                remaining -= 1;
                changed = true;
                if remaining == 0 {
                    break;
                }
            }
        }
        if !changed {
            break;
        }
    }
}

fn wrap_table_cell(cell: &str, width: usize) -> Vec<String> {
    let mut lines = cell
        .split('\n')
        .flat_map(|line| wrap_dw(line, width))
        .collect::<Vec<_>>();
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// 一条 Markdown 表格记录可能因为列宽不足而占多条终端物理行。
/// 边框与正文拆成不同 Span，避免竖线和正文一样亮、视觉上喧宾夺主。
fn table_row_lines(
    cells: &[String],
    widths: &[usize],
    aligns: &[Alignment],
    cell_styles: &[Style],
) -> Vec<Line<'static>> {
    let wrapped = widths
        .iter()
        .enumerate()
        .map(|(i, width)| wrap_table_cell(cells.get(i).map(String::as_str).unwrap_or(""), *width))
        .collect::<Vec<_>>();
    let row_height = wrapped.iter().map(Vec::len).max().unwrap_or(1);

    (0..row_height)
        .map(|line_idx| {
            let mut spans = Vec::with_capacity(widths.len() * 2 + 1);
            for (i, width) in widths.iter().copied().enumerate() {
                spans.push(Span::styled(
                    if i == 0 { "│ " } else { " │ " },
                    Theme::border(),
                ));
                let text = wrapped[i].get(line_idx).map(String::as_str).unwrap_or("");
                let align = aligns.get(i).cloned().unwrap_or(Alignment::Left);
                let style = cell_styles.get(i).cloned().unwrap_or_default();
                spans.push(Span::styled(pad_dw(text, width, align), style));
            }
            spans.push(Span::styled(" │", Theme::border()));
            Line::from(spans)
        })
        .collect()
}

/// Markdown 表格与可识别的结构化文本共用同一套网格，保证边框颜色、
/// 行间横线、列宽收缩和单元格换行规则完全一致。
fn emit_grid_table(
    lines: &mut Vec<Line<'static>>,
    header: Option<&[String]>,
    rows: &[Vec<String>],
    aligns: &[Alignment],
    body_styles: &[Style],
    max_width: usize,
) {
    let ncols = header
        .map(<[String]>::len)
        .unwrap_or_else(|| rows.iter().map(Vec::len).max().unwrap_or(0));
    if ncols == 0 {
        return;
    }

    let mut col_w = vec![0; ncols];
    if let Some(header) = header {
        for (i, cell) in header.iter().take(ncols).enumerate() {
            col_w[i] = table_cell_width(cell);
        }
    }
    for row in rows {
        for (i, cell) in row.iter().take(ncols).enumerate() {
            col_w[i] = col_w[i].max(table_cell_width(cell));
        }
    }
    fit_table_widths(&mut col_w, max_width);

    lines.push(Line::from(Span::styled(
        table_border(&col_w, '┌', '┬', '┐', '─'),
        Theme::border(),
    )));

    if let Some(header) = header {
        let header_styles = vec![Style::default().add_modifier(Modifier::BOLD); ncols];
        lines.extend(table_row_lines(header, &col_w, aligns, &header_styles));
        lines.push(Line::from(Span::styled(
            table_border(&col_w, '├', '┼', '┤', '─'),
            Theme::border(),
        )));
    }

    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            lines.push(Line::from(Span::styled(
                table_border(&col_w, '├', '┼', '┤', '─'),
                Theme::border(),
            )));
        }
        lines.extend(table_row_lines(row, &col_w, aligns, body_styles));
    }

    lines.push(Line::from(Span::styled(
        table_border(&col_w, '└', '┴', '┘', '─'),
        Theme::border(),
    )));
}

/// 模型有时会把时间线放进无语言代码围栏。它本质上是两列结构化数据，
/// 若至少三行都满足 `HH:MM  内容`，就按统一表格样式渲染，而不是伪装成代码。
fn parse_timeline_rows(text: &str) -> Option<Vec<Vec<String>>> {
    let mut rows = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let bytes = line.as_bytes();
        if bytes.len() < 8
            || !bytes[0].is_ascii_digit()
            || !bytes[1].is_ascii_digit()
            || bytes[2] != b':'
            || !bytes[3].is_ascii_digit()
            || !bytes[4].is_ascii_digit()
        {
            return None;
        }

        let hour = line[..2].parse::<u8>().ok()?;
        let minute = line[3..5].parse::<u8>().ok()?;
        let rest = &line[5..];
        let separator_len = rest.chars().take_while(|ch| ch.is_whitespace()).count();
        let content = rest.trim_start();
        if hour > 23 || minute > 59 || separator_len < 2 || content.is_empty() {
            return None;
        }
        rows.push(vec![line[..5].to_string(), content.to_string()]);
    }

    (rows.len() >= 3).then_some(rows)
}

// ── 语法高亮 ──────────────────────────────────────────────────────────────────

fn syntax_keywords(lang: &str) -> &'static [&'static str] {
    match lang {
        "rust" | "rs" => &[
            "fn", "let", "mut", "pub", "use", "mod", "impl", "trait", "struct", "enum", "match",
            "if", "else", "for", "while", "return", "async", "await", "move", "dyn", "true",
            "false", "self", "Self", "super", "crate", "type", "where", "loop", "break",
            "continue", "ref", "const", "static", "unsafe", "extern",
        ],
        "python" | "py" => &[
            "def", "class", "import", "from", "return", "if", "else", "elif", "for", "while",
            "with", "as", "in", "not", "and", "or", "pass", "break", "continue", "True", "False",
            "None", "lambda", "yield", "raise", "try", "except", "finally", "global", "nonlocal",
            "del", "is",
        ],
        "js" | "ts" | "javascript" | "typescript" => &[
            "function",
            "const",
            "let",
            "var",
            "class",
            "interface",
            "type",
            "export",
            "import",
            "from",
            "return",
            "if",
            "else",
            "for",
            "while",
            "async",
            "await",
            "new",
            "this",
            "true",
            "false",
            "null",
            "undefined",
            "break",
            "continue",
            "switch",
            "case",
            "default",
            "throw",
            "try",
            "catch",
            "finally",
            "extends",
            "implements",
            "static",
            "typeof",
            "instanceof",
        ],
        "go" => &[
            "func",
            "var",
            "const",
            "type",
            "struct",
            "interface",
            "map",
            "chan",
            "package",
            "import",
            "return",
            "if",
            "else",
            "for",
            "range",
            "select",
            "switch",
            "case",
            "default",
            "break",
            "continue",
            "go",
            "defer",
            "true",
            "false",
            "nil",
            "make",
            "new",
            "len",
            "cap",
            "append",
        ],
        "sh" | "bash" | "zsh" | "shell" => &[
            "if", "fi", "then", "else", "elif", "for", "do", "done", "while", "case", "esac",
            "echo", "export", "local", "return", "exit", "true", "false", "function", "source",
            "set", "unset",
        ],
        _ => &[],
    }
}

fn comment_prefix(lang: &str) -> Option<&'static str> {
    match lang {
        "rust" | "rs" | "go" | "java" | "c" | "cpp" | "js" | "ts" | "javascript" | "typescript"
        | "kotlin" | "swift" => Some("//"),
        "python" | "py" | "ruby" | "rb" | "sh" | "bash" | "zsh" | "shell" | "toml" | "yaml"
        | "yml" | "ini" | "r" => Some("#"),
        "lua" => Some("--"),
        _ => None,
    }
}

/// 对一行代码做简单词法着色，返回着色后的 Span 列表。
fn highlight_code_line(line: &str, lang: &str) -> Vec<Span<'static>> {
    let kw_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let str_style = Style::default().fg(Color::Green);
    let num_style = Style::default().fg(Color::Yellow);
    let cmt_style = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::ITALIC);
    let def_style = Theme::code_body();

    if line.is_empty() {
        return vec![];
    }

    let kws = syntax_keywords(lang);
    let cmt = comment_prefix(lang);
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    let mut spans: Vec<Span<'static>> = vec![];
    let mut i = 0;

    while i < n {
        // 行注释
        if let Some(prefix) = cmt {
            let remaining: String = chars[i..].iter().collect();
            if remaining.starts_with(prefix) {
                let rest: String = chars[i..].iter().collect();
                spans.push(Span::styled(rest, cmt_style));
                break;
            }
        }

        // 字符串字面量 " 或 '
        let q = chars[i];
        if q == '"' || q == '\'' {
            let mut j = i + 1;
            while j < n {
                if chars[j] == '\\' {
                    // 代码块按显示宽度逐行硬切（wrap_dw），转义反斜杠可能正好落在
                    // 某个换行片段的最后一个字符上，它的"配对字符"已被切到下一段——
                    // 不能假设 j+2 一定在界内，否则 chars[i..j] 越界 panic。
                    j = (j + 2).min(n);
                    continue;
                }
                if chars[j] == q {
                    j += 1;
                    break;
                }
                j += 1;
            }
            let s: String = chars[i..j].iter().collect();
            spans.push(Span::styled(s, str_style));
            i = j;
            continue;
        }

        // 数字
        if chars[i].is_ascii_digit() {
            let start = i;
            while i < n && (chars[i].is_ascii_alphanumeric() || chars[i] == '.' || chars[i] == '_')
            {
                i += 1;
            }
            let s: String = chars[start..i].iter().collect();
            spans.push(Span::styled(s, num_style));
            continue;
        }

        // 标识符 / 关键字
        if chars[i].is_alphabetic() || chars[i] == '_' {
            let start = i;
            while i < n && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            let style = if kws.contains(&word.as_str()) {
                kw_style
            } else {
                def_style
            };
            spans.push(Span::styled(word, style));
            continue;
        }

        // 其余字符
        let c: String = chars[i..i + 1].iter().collect();
        spans.push(Span::styled(c, def_style));
        i += 1;
    }

    spans
}

// ── 渲染器内部状态 ────────────────────────────────────────────────────────────

struct Ctx {
    max_width: usize,
    /// 当前行待输出的 (文本, 样式)
    cur: Vec<(String, Style)>,
    /// 列表栈：(有序?, 下一序号)
    list_stack: Vec<(bool, usize)>,
    /// 缩进层数（每层 2 空格）
    indent: usize,
    /// 行内样式计数
    strong: usize,
    em: usize,
    code_span: bool,
    /// 当前标题级别
    heading: Option<u8>,
    /// 块引用深度
    blockquote: usize,
    /// 是否在代码块内
    in_code_block: bool,
    /// 当前代码块语言标签（如 "rust", "python"）
    code_lang: String,
    /// 代码块完整正文；在结束事件统一判断普通代码或结构化时间线。
    code_buf: String,
    /// 表格状态
    in_table: bool,
    is_table_head: bool,
    table_aligns: Vec<Alignment>,
    table_header: Vec<String>,
    table_rows: Vec<Vec<String>>,
    cur_cell: String,
    /// 链接 URL（暂存，用于末尾注释）
    link_url: Option<String>,
}

impl Ctx {
    fn new(max_width: usize) -> Self {
        Self {
            max_width,
            cur: vec![],
            list_stack: vec![],
            indent: 0,
            strong: 0,
            em: 0,
            code_span: false,
            heading: None,
            blockquote: 0,
            in_code_block: false,
            code_lang: String::new(),
            code_buf: String::new(),
            in_table: false,
            is_table_head: false,
            table_aligns: vec![],
            table_header: vec![],
            table_rows: vec![],
            cur_cell: String::new(),
            link_url: None,
        }
    }

    fn cur_style(&self) -> Style {
        let mut s = Style::default();
        if self.code_span {
            return s.fg(Theme::code_fg_color());
        }
        if let Some(lvl) = self.heading {
            s = match lvl {
                1 => s.fg(Theme::claude_color()).add_modifier(Modifier::BOLD),
                2 => s.fg(Theme::text_color()).add_modifier(Modifier::BOLD),
                _ => s.fg(Theme::inactive_color()).add_modifier(Modifier::BOLD),
            };
        } else if self.blockquote > 0 {
            s = s.fg(Theme::inactive_color());
        }
        if self.strong > 0 {
            s = s.add_modifier(Modifier::BOLD);
        }
        if self.em > 0 {
            s = s.add_modifier(Modifier::ITALIC);
        }
        s
    }

    fn push_text(&mut self, text: &str) {
        if self.in_table {
            self.cur_cell.push_str(text);
            return;
        }
        if text.is_empty() {
            return;
        }
        let style = self.cur_style();
        // 合并相邻同样式 span 减少碎片
        if let Some(last) = self.cur.last_mut() {
            if last.1 == style {
                last.0.push_str(text);
                return;
            }
        }
        self.cur.push((text.to_string(), style));
    }

    /// 将累积 span 合并成一行，推入 lines
    fn flush(&mut self, lines: &mut Vec<Line<'static>>) {
        if self.cur.is_empty() {
            return;
        }
        let mut spans: Vec<Span<'static>> = vec![];

        // 块引用竖线前缀
        for _ in 0..self.blockquote {
            spans.push(Span::styled("│ ", Theme::dim()));
        }

        // 列表/段落缩进
        if self.indent > 0 {
            spans.push(Span::raw("  ".repeat(self.indent)));
        } else if self.heading.is_none() && self.blockquote == 0 {
            // 普通段落/列表条目加 2 空格（与 User 消息视觉对齐）
            spans.push(Span::raw("  "));
        }

        for (text, style) in self.cur.drain(..) {
            if style == Style::default() {
                spans.push(Span::raw(text));
            } else {
                spans.push(Span::styled(text, style));
            }
        }
        lines.push(Line::from(spans));
    }

    fn emit_code_block(&self, lines: &mut Vec<Line<'static>>) {
        if self.code_lang.is_empty() {
            if let Some(rows) = parse_timeline_rows(&self.code_buf) {
                let aligns = [Alignment::Left, Alignment::Left];
                let body_styles = [Theme::code_body(), Style::default()];
                emit_grid_table(lines, None, &rows, &aligns, &body_styles, self.max_width);
                return;
            }
        }

        let lang_display = if self.code_lang.is_empty() {
            String::new()
        } else {
            format!(" {} ", self.code_lang)
        };
        // 前缀 "  ╭─" 占 4 列，右上角占 1 列；边框统一使用深灰色。
        let dash = self
            .max_width
            .saturating_sub(display_width(&lang_display) + 5);
        lines.push(Line::from(Span::styled(
            format!("  ╭─{lang_display}{}╮", "─".repeat(dash)),
            Theme::border(),
        )));

        let max_line = self.max_width.saturating_sub(6);
        for raw_line in self.code_buf.lines() {
            for text in wrap_dw(raw_line, max_line) {
                let pad = (max_line + 1).saturating_sub(display_width(&text));
                let mut spans = vec![Span::styled("  │ ", Theme::border())];
                if self.code_lang.is_empty() {
                    spans.push(Span::styled(text, Theme::code_body()));
                } else {
                    spans.extend(highlight_code_line(&text, &self.code_lang));
                }
                spans.push(Span::styled(
                    format!("{}│", " ".repeat(pad)),
                    Theme::border(),
                ));
                lines.push(Line::from(spans));
            }
        }

        lines.push(Line::from(Span::styled(
            format!("  ╰{}╯", "─".repeat(self.max_width.saturating_sub(4))),
            Theme::border(),
        )));
    }

    /// 渲染整个表格（在 End(Table) 时调用）
    fn emit_table(&self, lines: &mut Vec<Line<'static>>) {
        if self.table_header.is_empty() {
            return;
        }
        let ncols = self.table_header.len();
        let aligns: Vec<Alignment> = (0..ncols)
            .map(|i| self.table_aligns.get(i).cloned().unwrap_or(Alignment::Left))
            .collect();
        emit_grid_table(
            lines,
            Some(&self.table_header),
            &self.table_rows,
            &aligns,
            &[],
            self.max_width,
        );
    }
}

// ── 公开入口 ──────────────────────────────────────────────────────────────────

/// 将 Markdown 字符串渲染为 ratatui `Line<'static>` 列表。
///
/// `max_width`：可用字符宽度（用于内容换行和表格列宽计算）。
pub fn render_markdown(lines: &mut Vec<Line<'static>>, text: &str, max_width: usize) {
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH;
    let parser = Parser::new_ext(text, opts);
    let mut c = Ctx::new(max_width);

    for event in parser {
        match event {
            // ─── 标题 ───────────────────────────────────────────────────────
            Event::Start(Tag::Heading { level, .. }) => {
                c.heading = Some(match level {
                    HeadingLevel::H1 => 1,
                    HeadingLevel::H2 => 2,
                    _ => 3,
                });
                let prefix = match level {
                    HeadingLevel::H1 => "█ ",
                    HeadingLevel::H2 => "▌ ",
                    _ => "▎ ",
                };
                c.cur.push((prefix.to_string(), Theme::assistant_prefix()));
            }
            Event::End(TagEnd::Heading(_)) => {
                c.flush(lines);
                c.heading = None;
            }

            // ─── 段落 ───────────────────────────────────────────────────────
            Event::Start(Tag::Paragraph) => {}
            Event::End(TagEnd::Paragraph) => {
                c.flush(lines);
            }

            // ─── 块引用 ─────────────────────────────────────────────────────
            Event::Start(Tag::BlockQuote(_)) => c.blockquote += 1,
            Event::End(TagEnd::BlockQuote(_)) => {
                c.flush(lines);
                c.blockquote = c.blockquote.saturating_sub(1);
            }

            // ─── 代码块 ─────────────────────────────────────────────────────
            Event::Start(Tag::CodeBlock(kind)) => {
                c.in_code_block = true;
                c.code_buf.clear();
                let lang_raw = match &kind {
                    CodeBlockKind::Fenced(l) if !l.is_empty() => l.to_string(),
                    _ => String::new(),
                };
                c.code_lang = lang_raw.to_lowercase();
            }
            Event::End(TagEnd::CodeBlock) => {
                c.emit_code_block(lines);
                c.in_code_block = false;
                c.code_lang.clear();
                c.code_buf.clear();
            }

            // ─── 列表 ───────────────────────────────────────────────────────
            Event::Start(Tag::List(start)) => {
                c.list_stack
                    .push((start.is_some(), start.unwrap_or(1) as usize));
                c.indent += 1;
            }
            Event::End(TagEnd::List(_)) => {
                c.list_stack.pop();
                c.indent = c.indent.saturating_sub(1);
            }
            Event::Start(Tag::Item) => {
                let bullet = if let Some((ordered, ref mut n)) = c.list_stack.last_mut() {
                    if *ordered {
                        let s = format!("{}. ", n);
                        *n += 1;
                        s
                    } else {
                        "• ".to_string()
                    }
                } else {
                    "• ".to_string()
                };
                c.cur.push((bullet, Theme::assistant_prefix()));
            }
            Event::End(TagEnd::Item) => {
                c.flush(lines);
            }

            // ─── 表格 ───────────────────────────────────────────────────────
            Event::Start(Tag::Table(aligns)) => {
                c.in_table = true;
                c.table_aligns = aligns;
                c.table_header.clear();
                c.table_rows.clear();
                c.is_table_head = true;
            }
            Event::End(TagEnd::Table) => {
                // 推入最后一个未提交的 cell（以防万一）
                if !c.cur_cell.is_empty() {
                    let cell = std::mem::take(&mut c.cur_cell);
                    if c.is_table_head {
                        c.table_header.push(cell);
                    } else if let Some(row) = c.table_rows.last_mut() {
                        row.push(cell);
                    }
                }
                c.emit_table(lines);
                c.in_table = false;
                c.is_table_head = false;
            }
            Event::Start(Tag::TableHead) => {
                c.is_table_head = true;
            }
            Event::End(TagEnd::TableHead) => {
                c.is_table_head = false;
            }
            Event::Start(Tag::TableRow) => {
                if !c.is_table_head {
                    c.table_rows.push(vec![]);
                }
            }
            Event::End(TagEnd::TableRow) => {}
            Event::Start(Tag::TableCell) => {
                c.cur_cell.clear();
            }
            Event::End(TagEnd::TableCell) => {
                let cell = std::mem::take(&mut c.cur_cell);
                if c.is_table_head {
                    c.table_header.push(cell);
                } else if let Some(row) = c.table_rows.last_mut() {
                    row.push(cell);
                }
            }

            // ─── 链接 ───────────────────────────────────────────────────────
            Event::Start(Tag::Link { dest_url, .. }) => {
                c.link_url = Some(dest_url.to_string());
            }
            Event::End(TagEnd::Link) => {
                if let Some(url) = c.link_url.take() {
                    // 在链接文字后追加灰色 URL 注释
                    c.cur.push((format!(" ({url})"), Theme::dim()));
                }
            }

            // ─── 行内样式 ───────────────────────────────────────────────────
            Event::Start(Tag::Strong) => c.strong += 1,
            Event::End(TagEnd::Strong) => c.strong = c.strong.saturating_sub(1),
            Event::Start(Tag::Emphasis) => c.em += 1,
            Event::End(TagEnd::Emphasis) => c.em = c.em.saturating_sub(1),
            Event::Code(text) => {
                // 行内代码：用 ` 包裹显示
                let style = Style::default().fg(Theme::code_fg_color());
                c.cur.push((format!("`{text}`"), style));
            }

            // ─── 文本内容 ───────────────────────────────────────────────────
            Event::Text(text) => {
                if c.in_code_block {
                    c.code_buf.push_str(&text);
                } else {
                    c.push_text(&text);
                }
            }
            Event::SoftBreak => c.push_text(" "),
            Event::HardBreak => {
                if c.in_table {
                    c.cur_cell.push('\n');
                } else {
                    c.flush(lines);
                }
            }

            // ─── 水平分隔线 ─────────────────────────────────────────────────
            Event::Rule => {
                lines.push(Line::from(Span::styled(
                    "─".repeat(max_width.min(60)),
                    Theme::dim(),
                )));
            }

            _ => {}
        }
    }

    // 刷新最后可能残留的内容
    c.flush(lines);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 回归测试：代码块内超宽行此前被 `truncate_dw` 截断加 "…"，导致长命令/长路径
    /// 在渲染框内看似"隐藏"了一部分——现在应改为换行展示全部内容，不丢字符。
    #[test]
    fn code_block_wraps_long_lines_instead_of_truncating() {
        let long_cmd = "0 8 * * * /usr/bin/env PATH=/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin /Users/foo/venv/bin/python /Users/foo/very/long/script/path/main.py";
        let text = format!("```\n{long_cmd}\n```\n");
        let mut lines = vec![];
        render_markdown(&mut lines, &text, 40);

        let rendered: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            !rendered.contains('…'),
            "长行应换行完整展示，而不是截断加省略号: {rendered}"
        );
        for ch in long_cmd.chars() {
            assert!(rendered.contains(ch), "换行后的渲染结果应包含原始字符 {ch}");
        }
    }

    #[test]
    fn plain_code_block_keeps_border_color_separate_from_body_color() {
        let mut lines = vec![];
        render_markdown(&mut lines, "```\nplain text\n```", 40);

        assert_eq!(lines.len(), 3);
        assert_eq!(lines[1].spans[0].style, Theme::border());
        assert_eq!(lines[1].spans[1].style, Theme::code_body());
        assert_eq!(lines[1].spans.last().unwrap().style, Theme::border());
        assert_ne!(Theme::border(), Theme::code_body());
    }

    #[test]
    fn fenced_timeline_uses_the_same_grid_and_row_separators_as_tables() {
        let text =
            "```\n09:00  半导体能不能追？\n09:15  苏州招商能不能建？\n09:30  海康能不能加仓？\n```";
        let mut lines = vec![];
        render_markdown(&mut lines, text, 80);
        let rendered = rendered_text(&lines);

        assert_eq!(rendered.len(), 7, "顶线/3 行/2 行间线/底线");
        assert!(rendered[0].starts_with('┌') && rendered[0].contains('┬'));
        for index in [2, 4] {
            assert!(rendered[index].starts_with('├'));
            assert!(rendered[index].contains('┼'));
            assert!(rendered[index].ends_with('┤'));
        }
        assert!(rendered[6].starts_with('└') && rendered[6].contains('┴'));

        let first_row = &lines[1];
        assert_eq!(first_row.spans[0].style, Theme::border());
        assert_eq!(first_row.spans[1].style, Theme::code_body());
        assert_eq!(first_row.spans[2].style, Theme::border());
        assert_eq!(first_row.spans[3].style, Style::default());
        assert_eq!(first_row.spans.last().unwrap().style, Theme::border());
    }

    /// 回归测试：代码块内长行被 `wrap_dw` 按显示宽度硬切时，若切分点恰好落在字符串
    /// 字面量转义反斜杠（如 `\n`）之后——即换行片段以孤立的 `\` 结尾、其配对字符被
    /// 换到了下一段——`highlight_code_line` 曾因未做边界检查而越界 panic
    /// （对应真实场景：终端较窄时显示一段含 `f"...\n"` 转义的长 Python 源码）。
    #[test]
    fn highlight_code_line_does_not_panic_on_trailing_escape_backslash() {
        let seg = "            f\"[{i}] 来源:{it.get('source','')} | 时间:{pub}\\";
        let _ = highlight_code_line(seg, "python");
    }

    #[test]
    fn paragraphs_render_compact_without_forced_blank_lines() {
        let mut lines = vec![];
        render_markdown(&mut lines, "第一段\n\n第二段\n\n## 标题\n\n正文", 80);
        let rendered = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();

        assert!(rendered.iter().all(|line| !line.trim().is_empty()));
        assert_eq!(rendered.len(), 4);
        assert!(rendered[0].contains("第一段"));
        assert!(rendered[1].contains("第二段"));
        assert!(rendered[2].contains("标题"));
        assert!(rendered[3].contains("正文"));
    }

    fn rendered_text(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn table_draws_horizontal_separators_between_body_rows() {
        let text = "| 标的 | 现持仓 |\n| --- | ---: |\n| 兴蓉 | 800 股 |\n| 海康 | 200 股 |\n| 总仓位 | 18.8% |";
        let mut lines = vec![];
        render_markdown(&mut lines, text, 80);
        let rendered = rendered_text(&lines);

        assert_eq!(rendered.len(), 9, "顶/表头/表头线/3 行/2 行间线/底");
        for index in [2, 4, 6] {
            assert!(
                rendered[index].starts_with('├') && rendered[index].ends_with('┤'),
                "表头和每条正文记录之间都应有完整横线: {}",
                rendered[index]
            );
            assert!(rendered[index].contains('┼'));
        }
    }

    #[test]
    fn table_uses_subdued_borders_without_dimming_cell_content() {
        let mut lines = vec![];
        render_markdown(
            &mut lines,
            "| 标的 | 仓位 |\n| --- | --- |\n| 兴蓉 | 800 |",
            80,
        );

        let body = &lines[3];
        assert_eq!(body.spans[0].style, Theme::border());
        assert_eq!(body.spans[1].style, Style::default());
        assert_eq!(body.spans.last().unwrap().style, Theme::border());
    }

    #[test]
    fn narrow_table_wraps_inside_cells_without_losing_grid_shape() {
        let text =
            "| 检查项 | 动作 |\n| --- | --- |\n| 半导体指数是否守住五个点 | 守住则继续持有 |";
        let mut lines = vec![];
        render_markdown(&mut lines, text, 24);
        let rendered = rendered_text(&lines);

        for line in &rendered {
            assert!(
                display_width(line) <= 24,
                "表格应在单元格内换行，不应触发终端二次折行: {line}"
            );
            if line.starts_with('│') {
                assert_eq!(
                    line.chars().filter(|ch| *ch == '│').count(),
                    3,
                    "两列表格每个物理行只应有 3 根必要的竖线: {line}"
                );
            }
        }
        let content = rendered.join("");
        assert!(!content.contains('…'), "狭表格不应截断内容");

        let body_lines = rendered
            .iter()
            .skip(3)
            .take_while(|line| !line.starts_with('└'))
            .filter(|line| line.starts_with('│'))
            .collect::<Vec<_>>();
        let first_cell = body_lines
            .iter()
            .filter_map(|line| line.split('│').nth(1))
            .map(str::trim)
            .collect::<String>();
        let second_cell = body_lines
            .iter()
            .filter_map(|line| line.split('│').nth(2))
            .map(str::trim)
            .collect::<String>();
        assert_eq!(first_cell, "半导体指数是否守住五个点");
        assert_eq!(second_cell, "守住则继续持有");
    }
}
