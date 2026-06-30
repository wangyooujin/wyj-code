//! Markdown → ratatui `Line<'static>` 渲染器
//!
//! 支持：表格（box-drawing 对齐）、标题、代码块、列表、粗体/斜体、块引用、分隔线。

use crate::theme::Theme;
use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

// ── 字符宽度 ──────────────────────────────────────────────────────────────────

/// 终端列宽：CJK 全角字符=2，其余=1
fn char_width(c: char) -> usize {
    let cp = c as u32;
    if (0x1100..=0x115F).contains(&cp)       // Hangul Jamo
        || (0x2E80..=0x9FFF).contains(&cp)   // CJK 各区块
        || (0xA960..=0xA97F).contains(&cp)
        || (0xAC00..=0xD7FF).contains(&cp)   // 谚文音节
        || (0xF900..=0xFAFF).contains(&cp)
        || (0xFE10..=0xFE1F).contains(&cp)
        || (0xFE30..=0xFE4F).contains(&cp)
        || (0xFF00..=0xFF60).contains(&cp)   // 全角字母
        || (0xFFE0..=0xFFE6).contains(&cp)
        || cp >= 0x2_0000
    // 扩展区 B+
    {
        2
    } else {
        1
    }
}

pub fn display_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

/// 按显示宽度截断，超出后附 `…`
fn truncate_dw(s: &str, max: usize) -> String {
    let mut w = 0usize;
    let mut out = String::new();
    let chars: Vec<char> = s.chars().collect();
    for &c in &chars {
        let cw = char_width(c);
        if w + cw > max {
            // 留一格放 …
            out.push('…');
            break;
        }
        out.push(c);
        w += cw;
    }
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

fn table_row_str(cells: &[String], widths: &[usize], aligns: &[Alignment]) -> String {
    let mut s = String::from("│");
    for (i, &w) in widths.iter().enumerate() {
        let raw = cells.get(i).map(String::as_str).unwrap_or("");
        let cell = if display_width(raw) > w {
            truncate_dw(raw, w)
        } else {
            raw.to_string()
        };
        let align = aligns.get(i).cloned().unwrap_or(Alignment::Left);
        s.push(' ');
        s.push_str(&pad_dw(&cell, w, align));
        s.push(' ');
        s.push('│');
    }
    s
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
                    j += 2;
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
            return s.fg(Theme::CODE_FG);
        }
        if let Some(lvl) = self.heading {
            s = match lvl {
                1 => s.fg(Theme::CLAUDE).add_modifier(Modifier::BOLD),
                2 => s.fg(Theme::TEXT).add_modifier(Modifier::BOLD),
                _ => s.fg(Theme::INACTIVE).add_modifier(Modifier::BOLD),
            };
        } else if self.blockquote > 0 {
            s = s.fg(Theme::INACTIVE);
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

    /// 渲染整个表格（在 End(Table) 时调用）
    fn emit_table(&self, lines: &mut Vec<Line<'static>>) {
        if self.table_header.is_empty() {
            return;
        }
        let ncols = self.table_header.len();
        let aligns: Vec<Alignment> = (0..ncols)
            .map(|i| self.table_aligns.get(i).cloned().unwrap_or(Alignment::Left))
            .collect();

        // 每列最小宽 = 表头/数据中最大的显示宽度
        let mut col_w: Vec<usize> = self.table_header.iter().map(|h| display_width(h)).collect();
        col_w.resize(ncols, 0);

        for row in &self.table_rows {
            for (i, cell) in row.iter().enumerate() {
                if i < ncols {
                    col_w[i] = col_w[i].max(display_width(cell));
                }
            }
        }

        // 若表格超出可用宽度，只压缩宽列（保留窄列，最小 4）
        let total = col_w.iter().sum::<usize>() + ncols * 3 + 1;
        if total > self.max_width && ncols > 0 {
            let avail = self.max_width.saturating_sub(ncols * 3 + 1);
            let max_col = (avail / ncols).max(4);
            for w in &mut col_w {
                if *w > max_col {
                    *w = max_col;
                }
            }
        }

        // 顶边框
        lines.push(Line::from(Span::styled(
            table_border(&col_w, '┌', '┬', '┐', '─'),
            Theme::dim(),
        )));
        // 表头（加粗）
        lines.push(Line::from(Span::styled(
            table_row_str(&self.table_header, &col_w, &aligns),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        // 分隔线
        lines.push(Line::from(Span::styled(
            table_border(&col_w, '├', '┼', '┤', '─'),
            Theme::dim(),
        )));
        // 数据行
        for row in &self.table_rows {
            lines.push(Line::from(Span::raw(table_row_str(row, &col_w, &aligns))));
        }
        // 底边框
        lines.push(Line::from(Span::styled(
            table_border(&col_w, '└', '┴', '┘', '─'),
            Theme::dim(),
        )));
    }
}

// ── 公开入口 ──────────────────────────────────────────────────────────────────

/// 将 Markdown 字符串渲染为 ratatui `Line<'static>` 列表。
///
/// `max_width`：可用字符宽度（用于截断和表格列宽计算）。
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
                lines.push(Line::from(""));
                c.heading = None;
            }

            // ─── 段落 ───────────────────────────────────────────────────────
            Event::Start(Tag::Paragraph) => {}
            Event::End(TagEnd::Paragraph) => {
                c.flush(lines);
                lines.push(Line::from(""));
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
                let lang_raw = match &kind {
                    CodeBlockKind::Fenced(l) if !l.is_empty() => l.to_string(),
                    _ => String::new(),
                };
                c.code_lang = lang_raw.to_lowercase();
                let lang_display = if lang_raw.is_empty() {
                    String::new()
                } else {
                    format!(" {lang_raw} ")
                };
                let dash = max_width.saturating_sub(lang_display.len() + 3);
                lines.push(Line::from(Span::styled(
                    format!("  ╭─{lang_display}{}", "─".repeat(dash)),
                    Theme::dim(),
                )));
            }
            Event::End(TagEnd::CodeBlock) => {
                c.in_code_block = false;
                c.code_lang.clear();
                lines.push(Line::from(Span::styled(
                    format!("  ╰{}", "─".repeat(max_width.saturating_sub(4))),
                    Theme::dim(),
                )));
                lines.push(Line::from(""));
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
                lines.push(Line::from(""));
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
                let style = Style::default().fg(Theme::CODE_FG);
                c.cur.push((format!("`{text}`"), style));
            }

            // ─── 文本内容 ───────────────────────────────────────────────────
            Event::Text(text) => {
                if c.in_code_block {
                    let max_line = max_width.saturating_sub(6);
                    for raw_line in text.lines() {
                        let s = if display_width(raw_line) > max_line {
                            truncate_dw(raw_line, max_line)
                        } else {
                            raw_line.to_string()
                        };
                        if c.code_lang.is_empty() {
                            lines.push(Line::from(Span::styled(
                                format!("  │ {s}"),
                                Theme::code_body(),
                            )));
                        } else {
                            let mut spans = vec![Span::styled("  │ ".to_string(), Theme::dim())];
                            spans.extend(highlight_code_line(&s, &c.code_lang));
                            lines.push(Line::from(spans));
                        }
                    }
                } else {
                    c.push_text(&text);
                }
            }
            Event::SoftBreak => c.push_text(" "),
            Event::HardBreak => {
                c.flush(lines);
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
