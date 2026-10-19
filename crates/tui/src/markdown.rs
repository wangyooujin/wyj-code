//! Markdown → ratatui `Line<'static>` 渲染器
//!
//! 支持：表格（box-drawing 对齐）、标题、代码块、列表、粗体/斜体、块引用、分隔线。

use crate::theme::Theme;
use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use std::borrow::Cow;
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

// ── 折行：宽度感知的可视行拆分 ─────────────────────────────────────────────────

/// 中文排版禁则：这些字符不能出现在**行首**。
///
/// 终端没有标点悬挂，行首冒出一个收尾标点是排版事故——「段落，第二句」被读成
/// 「第二 / 句」时，那个开头的逗号既不属于上句也不属于下句（它本来就是上句的），
/// 而中文字与中文字之间没有空格兜底，读者只能回扫一行确认自己没读串行。
/// 折行时若新行首是这些字符，就把行尾一个原子一起带走（见 [`wrap_spans`]）。
///
/// 只收全角标点与英文句读标点。ASCII 的 `]` `}` `"` `'` **不收**：英文里
/// `[a][b]`、`{"k": 1}` 折行时行首出现它们是常态，按禁则处理反而会拖慢装行。
fn is_forbidden_line_start(c: char) -> bool {
    matches!(
        c,
        '，' | '。'
            | '、'
            | '；'
            | '：'
            | '？'
            | '！'
            | '…'
            | '—'
            | '～'
            | '·'
            | '∶'
            | '）'
            | '】'
            | '》'
            | '〉'
            | '」'
            | '』'
            | '］'
            | '｝'
            | '〕'
            | '”'
            | '’'
            | '％'
            | '‰'
            | ','
            | '.'
            | ';'
            | ':'
            | '?'
            | '!'
            | ')'
    )
}

/// 这些字符不能出现在**行尾**：它们是成对符号的开半边，被拆到下一行就悬在那里。
fn is_forbidden_line_end(c: char) -> bool {
    matches!(
        c,
        '（' | '【' | '《' | '〈' | '「' | '『' | '〔' | '｛' | '“' | '‘'
    )
}

/// 成对符号里那些**以重复形式出现**的（破折号、省略号），连续同字符合并成
/// 一个原子，避免 `——` / `……` 被从中间劈成两行。
fn is_pairable_symbol(c: char) -> bool {
    matches!(c, '—' | '…' | '―' | '～' | '‥' | '-' | '"' | '\'' | '·')
}

/// 折行原子：一段连续、默认不从中打断的文本。
struct Atom {
    text: String,
    style: Style,
    width: usize,
    /// 在本原子**之前**是否可以断行（前面是空白，或是序列开头）。
    break_before: bool,
    /// 纯空白——断行时要从行尾剔除，不留拖尾空格。
    is_space: bool,
}

/// 把 `(文本, 样式)` 序列切成折行原子。
///
/// 三类原子：空白（自身不可断，但它之后的原子 `break_before` 为真）、
/// 宽字符（汉字 / 全角标点 / emoji，逐字之间可断）、ASCII 词（整词不断，
/// 交给 [`split_oversized`] 处理超长的情况）。
///
/// 宽字符判据用 `width == 2` 而不是 Unicode 脚本属性：它同时覆盖汉字、全角标点
/// 和 emoji，而这些正是「按字排版」的全部场景；代价是宽 emoji 的 ZWJ 序列会被
/// 逐码点切开——但那本来也不是可折行的文本。
fn tokenize_atoms(chunks: &[(String, Style)]) -> Vec<Atom> {
    let mut atoms: Vec<Atom> = Vec::new();
    let mut word = String::new();
    let mut word_style = Style::default();
    // 下一个原子是否可在其之前断行（= 上一个原子是空白或序列开头）
    let mut next_break = true;

    macro_rules! flush_word {
        () => {
            if !word.is_empty() {
                atoms.push(Atom {
                    width: display_width(&word),
                    text: std::mem::take(&mut word),
                    style: word_style,
                    break_before: std::mem::replace(&mut next_break, false),
                    is_space: false,
                });
            }
        };
    }

    for (text, style) in chunks {
        for ch in text.chars() {
            let cw = ch.width().unwrap_or(0);
            // 成对重复符号（`——` / `……`）合并进上一个原子，免得被劈成两行
            if is_pairable_symbol(ch) {
                if let Some(last) = atoms.last_mut() {
                    if !last.is_space
                        && last.text.ends_with(ch)
                        && !last.text.chars().next().is_some_and(is_forbidden_line_end)
                    {
                        last.text.push(ch);
                        last.width += cw;
                        continue;
                    }
                }
            }
            if ch.is_whitespace() {
                flush_word!();
                if let Some(last) = atoms.last_mut() {
                    if last.is_space {
                        last.text.push(ch);
                        last.width += cw;
                        continue;
                    }
                }
                atoms.push(Atom {
                    text: ch.to_string(),
                    style: *style,
                    width: cw,
                    break_before: false,
                    is_space: true,
                });
                next_break = true;
            } else if cw == 2 {
                flush_word!();
                atoms.push(Atom {
                    text: ch.to_string(),
                    style: *style,
                    width: cw,
                    break_before: next_break,
                    is_space: false,
                });
                next_break = !is_forbidden_line_start(ch);
            } else {
                word.push(ch);
                word_style = *style;
            }
        }
    }
    flush_word!();
    atoms
}

/// 超长原子（长路径 / 长 URL / 无空格的标识符）按字符切片，保证每片 ≤ `avail`。
///
/// 不切就会溢出终端。旧实现把这件事交给 `Paragraph::wrap` 的 WordWrapper，
/// 它确实会硬切，但那时首行前缀早已丢失——切出来的后半截既没有缩进也没有
/// 引用竖线，是「排版整体塌陷」里最常见的现场。
fn split_oversized(atoms: Vec<Atom>, avail: usize) -> Vec<Atom> {
    if avail == 0 {
        return atoms;
    }
    let mut out: Vec<Atom> = Vec::with_capacity(atoms.len());
    for atom in atoms {
        if atom.width <= avail {
            out.push(atom);
            continue;
        }
        let mut piece = String::new();
        let mut w = 0usize;
        let mut break_before = atom.break_before;
        for ch in atom.text.chars() {
            let cw = ch.width().unwrap_or(0);
            if w + cw > avail && w > 0 {
                out.push(Atom {
                    width: w,
                    text: std::mem::take(&mut piece),
                    style: atom.style,
                    break_before,
                    is_space: false,
                });
                break_before = true;
                w = 0;
            }
            piece.push(ch);
            w += cw;
        }
        if !piece.is_empty() {
            out.push(Atom {
                width: w,
                text: piece,
                style: atom.style,
                break_before,
                is_space: false,
            });
        }
    }
    out
}

/// 把一段已排好样式的文本折成若干可视行，每行显示宽度 ≤ `avail`。
///
/// 断点优先级：① 空白之后（并丢掉行尾空白）；② 宽字符逐字之间；
/// ③ 原子内部硬切（只发生在单个原子本身就超宽时）。
/// 中间的禁则回退只在「原子内硬切」这条路上生效——那里没有更好的断点可挑，
/// 要让标点不落行首就只能把行尾一个原子带下去。
fn wrap_spans(chunks: &[(String, Style)], avail: usize) -> Vec<Vec<(String, Style)>> {
    if avail == 0 || chunks.is_empty() {
        return if chunks.is_empty() {
            vec![Vec::new()]
        } else {
            vec![chunks.to_vec()]
        };
    }

    let atoms = split_oversized(tokenize_atoms(chunks), avail);
    let mut lines: Vec<Vec<(String, Style)>> = Vec::new();
    let mut line: Vec<Atom> = Vec::new();
    let mut used = 0usize;

    for atom in atoms {
        if used > 0 && used + atom.width > avail {
            let new_line_forbidden = atom
                .text
                .chars()
                .next()
                .is_some_and(is_forbidden_line_start);
            let tail_forbidden = line
                .last()
                .and_then(|a| a.text.chars().next())
                .is_some_and(is_forbidden_line_end);
            // `carry` 是为了让断点合规而从上一行「摘」下来的原子。它们**必须跟着
            // 下一行走**——丢掉就等于吃字（曾把「开始——」的「始」吃成了裸的「——」）。
            let mut carry: Vec<Atom> = Vec::new();
            if new_line_forbidden {
                // 行尾连续的收尾标点本来就属于下一句，先摘走；再多摘一个非空白原子，
                // 让剩下的行首不是标点，于是标点在下一行里重新有了归属。
                while line.last().is_some_and(|a| {
                    a.is_space || a.text.chars().next().is_some_and(is_forbidden_line_start)
                }) {
                    if let Some(last) = line.pop() {
                        if !last.is_space {
                            carry.insert(0, last);
                        }
                    }
                }
                if let Some(last) = line.pop() {
                    if !last.is_space {
                        carry.insert(0, last);
                    }
                }
            } else if tail_forbidden {
                // 开括号 / 开引号落在行尾同样违规，跟着下一行走。
                while let Some(last) = line.pop() {
                    if !last.is_space {
                        carry.insert(0, last);
                        break;
                    }
                }
            }
            if atom.break_before {
                while line.last().is_some_and(|a| a.is_space) {
                    line.pop();
                }
            }
            if !line.is_empty() {
                lines.push(take_atoms(&mut line));
            }
            used = carry.iter().map(|a| a.width).sum();
            line = carry;
        }
        used += atom.width;
        line.push(atom);
    }
    if !line.is_empty() {
        lines.push(take_atoms(&mut line));
    }
    if lines.is_empty() {
        lines.push(Vec::new());
    }
    lines
}

/// 原子序列 → span 序列，顺带合并相邻同样式，减少碎片。
fn take_atoms(line: &mut Vec<Atom>) -> Vec<(String, Style)> {
    let mut out: Vec<(String, Style)> = Vec::with_capacity(line.len());
    for atom in line.drain(..) {
        if let Some(last) = out.last_mut() {
            if last.1 == atom.style {
                last.0.push_str(&atom.text);
                continue;
            }
        }
        out.push((atom.text, atom.style));
    }
    out
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

/// 单元格折行走统一折行器（[`wrap_spans`]）而不是 `wrap_dw` 的纯按字硬切。
///
/// 差别是实际的：`_ => {}` 里的 `{}`、`` `Tag::Image` `` 的反引号对、末行的
/// 收尾标点，在硬切下都会被劈成两半（曾出现续行只剩一个孤零零的反引号）。
/// 单元格只有纯字符串没有 span 级样式，所以走折行器不吃亏。
fn wrap_table_cell(cell: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = cell
        .split('\n')
        .flat_map(|line| {
            wrap_spans(&[(line.to_string(), Style::default())], width)
                .into_iter()
                .map(|visual| visual.into_iter().map(|(text, _)| text).collect::<String>())
                .collect::<Vec<_>>()
        })
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
    indent: &[Span<'static>],
) -> Vec<Line<'static>> {
    let wrapped = widths
        .iter()
        .enumerate()
        .map(|(i, width)| wrap_table_cell(cells.get(i).map(String::as_str).unwrap_or(""), *width))
        .collect::<Vec<_>>();
    let row_height = wrapped.iter().map(Vec::len).max().unwrap_or(1);

    (0..row_height)
        .map(|line_idx| {
            let mut spans = Vec::with_capacity(indent.len() + widths.len() * 2 + 1);
            spans.extend(indent.iter().cloned());
            for (i, width) in widths.iter().copied().enumerate() {
                spans.push(Span::styled(
                    if i == 0 { "│ " } else { " │ " },
                    Theme::border(),
                ));
                let text = wrapped[i].get(line_idx).map(String::as_str).unwrap_or("");
                let align = aligns.get(i).cloned().unwrap_or(Alignment::Left);
                // 单元格默认用正文色，而不是 `Style::default()`（透传终端前景色）：
                // 表格内容与正文属于同一层，颜色必须由主题决定。
                let style = cell_styles.get(i).cloned().unwrap_or_else(Theme::body);
                spans.push(Span::styled(pad_dw(text, width, align), style));
            }
            spans.push(Span::styled(" │", Theme::border()));
            Line::from(spans)
        })
        .collect()
}

/// 表格列数太多、宽屏也放不下时，网格会碎成每列 1~2 个字符的竖条，
/// 再被 `Paragraph::wrap` 从中间二次折行、竖线彻底错位（`┐` `┘` 各占一行）。
/// 阈值取「每列至少要有 4 列内容」——低于这个宽度，横向对比已经无从读起，
/// 表格能承载的信息只剩下「行与行的对应关系」，而键值列表反而完整。
const MIN_COL_CONTENT: usize = 4;

/// 预算内放不下就退化成 `值一 · 值二 · 值三` 的缩进列表。
///
/// 退化后**逐列信息全部保留**，只是失去列对齐——这比保留一个碎掉的框强。
///
/// 折行**只在 ` · ` 分隔符处发生**，单元格内容是一个不可分的整体。通用折行器
/// （[`wrap_spans`]）按字断行，会把「列六」劈成 `列` / `六` 两行——表格的语义
/// 是「列值的整体」，拆开之后读者对不上是哪一列的值。只有单元格本身宽到
/// 放不下时才在它内部按显示宽度硬切，那是「必须溢出还是必须拆」的取舍。
fn emit_flat_table(
    lines: &mut Vec<Line<'static>>,
    header: Option<&[String]>,
    rows: &[Vec<String>],
    body_styles: &[Style],
    indent: &[Span<'static>],
    max_width: usize,
) {
    const SEP: &str = " · ";
    let sep_w = display_width(SEP);
    let indent_width: usize = indent
        .iter()
        .map(|s| display_width(s.content.as_ref()))
        .sum();
    let avail = max_width.saturating_sub(indent_width).max(1);

    fn styled(text: String, style: Style) -> Span<'static> {
        if style == Style::default() {
            Span::raw(text)
        } else {
            Span::styled(text, style)
        }
    }

    fn emit_row(
        lines: &mut Vec<Line<'static>>,
        cells: &[String],
        styles: &[Style],
        indent: &[Span<'static>],
        avail: usize,
        sep_w: usize,
    ) {
        let indent_width: usize = indent
            .iter()
            .map(|s| display_width(s.content.as_ref()))
            .sum();
        let mut body: Vec<Span<'static>> = Vec::new();
        let mut used = indent_width;
        let push = |body: &mut Vec<Span<'static>>, lines: &mut Vec<Line<'static>>| {
            let mut spans = indent.to_vec();
            spans.append(body);
            lines.push(Line::from(spans));
        };

        for (i, cell) in cells.iter().enumerate() {
            let style = styles.get(i).cloned().unwrap_or_default();
            // 单元格内的硬换行折叠成空格：退化形态没有行结构了。
            let text = if cell.contains('\n') {
                cell.split_whitespace().collect::<Vec<_>>().join(" ")
            } else {
                cell.clone()
            };
            let cw = display_width(&text);

            if i > 0 {
                if used + sep_w + cw > avail {
                    push(&mut body, lines);
                    body = Vec::new();
                    used = indent_width;
                } else {
                    body.push(Span::raw(" · ".to_string()));
                    used += sep_w;
                }
            }

            if cw > avail {
                for (j, seg) in wrap_dw(&text, avail).into_iter().enumerate() {
                    if j > 0 {
                        push(&mut body, lines);
                        body = Vec::new();
                        used = indent_width;
                    }
                    used += display_width(&seg);
                    body.push(styled(seg, style));
                }
            } else {
                used += cw;
                body.push(styled(text, style));
            }
        }
        if body.len() > indent.len() || !body.is_empty() {
            push(&mut body, lines);
        }
    }

    if let Some(header) = header {
        let bold = Theme::body().add_modifier(Modifier::BOLD);
        emit_row(
            lines,
            header,
            &vec![bold; header.len()],
            indent,
            avail,
            sep_w,
        );
    }
    for row in rows {
        let row_styles: Vec<Style> = (0..row.len())
            .map(|i| body_styles.get(i).cloned().unwrap_or_else(Theme::body))
            .collect();
        emit_row(lines, row, &row_styles, indent, avail, sep_w);
    }
}

/// Markdown 表格与可识别的结构化文本共用同一套网格，保证边框颜色、
/// 行间横线、列宽收缩和单元格换行规则完全一致。
///
/// `indent` 是继承自列表 / 引用的结构前缀（见 `Ctx::block_indent`）：
/// 表格嵌在列表项里时必须跟着 bullet 一起缩进，否则读者看不出那些行属于上面那一条。
fn emit_grid_table(
    lines: &mut Vec<Line<'static>>,
    header: Option<&[String]>,
    rows: &[Vec<String>],
    aligns: &[Alignment],
    body_styles: &[Style],
    indent: &[Span<'static>],
    max_width: usize,
) {
    let ncols = header
        .map(<[String]>::len)
        .unwrap_or_else(|| rows.iter().map(Vec::len).max().unwrap_or(0));
    if ncols == 0 {
        return;
    }

    let indent_width: usize = indent
        .iter()
        .map(|s| display_width(s.content.as_ref()))
        .sum();
    let body_width = max_width.saturating_sub(indent_width);
    // 每列左右各 1 个空格，另有 n + 1 根竖线。
    let frame_width = ncols * 3 + 1;
    if body_width < frame_width + ncols * MIN_COL_CONTENT {
        emit_flat_table(lines, header, rows, body_styles, indent, max_width);
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
    fit_table_widths(&mut col_w, body_width);

    let border_line = |l: char, m: char, r: char| {
        let mut spans = Vec::with_capacity(indent.len() + 1);
        spans.extend(indent.iter().cloned());
        spans.push(Span::styled(
            table_border(&col_w, l, m, r, '─'),
            Theme::border(),
        ));
        Line::from(spans)
    };

    lines.push(border_line('┌', '┬', '┐'));

    if let Some(header) = header {
        let header_styles = vec![Theme::body().add_modifier(Modifier::BOLD); ncols];
        lines.extend(table_row_lines(
            header,
            &col_w,
            aligns,
            &header_styles,
            indent,
        ));
        lines.push(border_line('├', '┼', '┤'));
    }

    // 数据行之间**不画横线**。旧实现每行一条，三行数据就是四条横线，
    // 竖线被切成一格一格的小格子，视觉噪音比它承载的信息量大；表头下那一条
    // 已经足够把表头和正文分开。
    for row in rows {
        lines.extend(table_row_lines(row, &col_w, aligns, body_styles, indent));
    }

    lines.push(border_line('└', '┴', '┘'));
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
    let kw_style = Theme::code_keyword();
    let str_style = Theme::code_string();
    let num_style = Theme::code_number();
    let cmt_style = Theme::code_comment();
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

/// 单层 list 的渲染状态。
///
/// `leader` 是这一层 Item bullet 的起始列宽:
/// - 顶层 list = 2(对应原 indent=1 的 2 空格基础缩进,与 User 消息视觉对齐)
/// - 嵌套 list = 父层 leader + 父层 bullet 宽度 + 1(空格),保证子层 bullet 起点
///   始终落在父层文字起点之后,且不论同层用的是 `•`(2 列)、`1.`(3 列)还是
///   `10.`(4 列),文字起始列在同层内保持一致,跨嵌套时按父 bullet 宽度自适应。
struct ListState {
    is_ordered: bool,
    next_n: usize,
    leader: usize,
}

impl ListState {
    /// 当前层下一项的 bullet 文本(用于让父层知道"如果再加嵌套层,leader 应该
    /// 推多少列")。这只是个 hint,真正的 bullet 在 Item start 时计算并存到
    /// `Ctx.cur_bullet`,以保证序号递增的方向与显示一致。
    fn sample_bullet(&self) -> String {
        if self.is_ordered {
            format!("{}. ", self.next_n)
        } else {
            "• ".to_string()
        }
    }
}

/// 组装一行：`prefix` 是这一行专属的前缀（首行带 bullet，续行带等宽占位）。
///
/// 两者宽度相同是 [`Ctx::block_prefix`] 的契约——续行的正文因此与首行严格对齐。
fn build_line(prefix: &[Span<'static>], chunk: &[(String, Style)]) -> Line<'static> {
    let mut spans = Vec::with_capacity(prefix.len() + chunk.len());
    spans.extend(prefix.iter().cloned());
    for (text, style) in chunk {
        if *style == Style::default() {
            spans.push(Span::raw(text.clone()));
        } else {
            spans.push(Span::styled(text.clone(), *style));
        }
    }
    Line::from(spans)
}

struct Ctx {
    max_width: usize,
    /// 当前行待输出的 (文本, 样式)
    /// 累积中的 span：`break_after` 为真表示此处要断行（见 [`Ctx::push_break`]）。
    cur: Vec<(String, Style, bool)>,
    /// 当前 Item 的 bullet 文本。在 Item start 时设置,Item end flush 后清空。
    /// flush 时把它放在 leader 位置前缀 spans 里,而不是塞到 cur 里被连续 push。
    cur_bullet: Option<String>,
    /// 列表栈,每项含 is_ordered / 下一序号 / 当前层 leader(文字起始列宽)
    list_stack: Vec<ListState>,
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
    /// 图片 URL（暂存，用于末尾注释）
    image_url: Option<String>,
    /// 删除线嵌套深度（`~~删除~~`）
    strike: usize,
    /// 脚注定义块深度：非 0 时整块按 `[^label]: ` 起头的缩进段落渲染
    footnote_def: Option<String>,
    /// 定义列表的标题行（`: 术语`）/ 说明行（`: 说明`）
    def_title: bool,
    def_desc: bool,
    /// 待补的空行数。块级元素之间的呼吸全走它，见 [`Ctx::ensure_blank`]。
    pending_blank: bool,
    /// 当前段落是否「像一张正在逐行写出来的表格」（每个非空行都以 `|` 开头）。
    /// 只在流式期间参与渲染：段内换行对普通文本应当折叠成空格，对半张表则是灾难。
    pipe_paragraph: bool,
    /// thinking 模式（见 [`render_thinking_markdown`]）：软换行保留成真实换行，
    /// 代码块退化成 `│` 引用式纯文本而不是带行号的方框。
    ///
    /// 这两条都不是"更好看"，而是 thinking 与正文的**语义差异**：正文的段内换行
    /// 是排版噪音（应该由折行器重新排版），thinking 的换行是思考的**断句**
    /// （模型一行一个念头，合并成一段反而读不出层次）；正文的代码块是要照着
    /// 执行的代码，thinking 里的围栏多数只是路径 / 命令片段，套一个方框会让它
    /// 比周围思考还显眼。
    thinking: bool,
}

impl Ctx {
    fn new(max_width: usize) -> Self {
        Self {
            max_width,
            cur: vec![],
            cur_bullet: None,
            list_stack: vec![],
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
            image_url: None,
            strike: 0,
            footnote_def: None,
            def_title: false,
            def_desc: false,
            pending_blank: false,
            pipe_paragraph: false,
            thinking: false,
        }
    }

    fn cur_style(&self) -> Style {
        // thinking 的正文基调是灰的：它是一段旁白，不是用户要读的结论。所有
        // 派生样式都从这个基色出发，因此 `**粗体**` / 标题 / 引用在 thinking
        // 里只体现修饰（BOLD/ITALIC），不会突然跳成白色或品牌橙。
        let mut s = if self.thinking {
            Theme::dim()
        } else {
            // 正文显式取 `text` 槽位，而不是 `Style::default()`（透传终端前景色）：
            // 主题是一个封闭的色板，正文颜色必须能被插件主题改到，也不该随
            // 用户终端的前景色漂移——否则冷灰配色会在别人的终端里变成暖白。
            Theme::body()
        };
        if self.code_span {
            // thinking 里的行内代码不加色：反引号本身已经足以标示，再换成亮色
            // 会让零散提到的一个标识符比整段思考还抢眼。
            return if self.thinking {
                s
            } else {
                s.fg(Theme::inline_code_color())
            };
        }
        if let Some(lvl) = self.heading {
            s = if self.thinking {
                s.add_modifier(Modifier::BOLD)
            } else {
                match lvl {
                    1 => s.fg(Theme::claude_color()).add_modifier(Modifier::BOLD),
                    2 => s.fg(Theme::text_color()).add_modifier(Modifier::BOLD),
                    // h3–h6 是「小标题」，比正文弱一档而不是弱两档：
                    // 旧实现用 `inactive`（#999）在暗底上与正文差距过大，
                    // 深色终端里几乎读不出来。
                    _ => s.fg(Theme::text_dim_color()).add_modifier(Modifier::BOLD),
                }
            };
        } else if self.blockquote > 0 {
            // 引用是「另一种正文」，不是「更弱的正文」——用次级正文而不是 dim。
            s = s.fg(Theme::text_dim_color());
        }
        // 定义列表的术语加粗：终端里没有「术语 / 说明」的语义标记，
        // 两者只差一级缩进的话读起来就是两段普通文字。
        if self.def_title {
            s = s.add_modifier(Modifier::BOLD);
        }
        if self.strong > 0 {
            s = s.add_modifier(Modifier::BOLD);
        }
        if self.em > 0 {
            s = s.add_modifier(Modifier::ITALIC);
        }
        if self.strike > 0 {
            s = s.add_modifier(Modifier::CROSSED_OUT);
        }
        s
    }

    fn push_text(&mut self, text: &str) {
        let style = self.cur_style();
        self.push_styled_text(text, style);
    }

    /// 以**指定样式**推入一段文本。
    ///
    /// 表格单元格只存纯字符串，所以 `in_table` 时样式被丢弃——这是表格渲染的
    /// 固有限制（网格要靠对齐而不是颜色传递语义），但**内容必须进单元格**：
    /// 漏掉这个分支的调用点（过去是行内代码、脚注、链接 URL 三处）会让文本
    /// 滞留在 `cur` 里，等表格结束后与下一段粘成一行。
    fn push_styled_text(&mut self, text: &str, style: Style) {
        if self.in_table {
            self.cur_cell.push_str(text);
            return;
        }
        if text.is_empty() {
            return;
        }
        // 合并相邻同样式 span 减少碎片。**带 break 标记的 span 不参与合并**——
        // 断行点之后的内容必须另起一个 span，否则会被追加到断行点之前，
        // 标记就此失效（表现为「该换行的地方没换」）。
        if let Some(last) = self.cur.last_mut() {
            if last.1 == style && !last.2 {
                last.0.push_str(text);
                return;
            }
        }
        self.cur.push((text.to_string(), style, false));
    }

    /// 在当前累积位置插入一个**强制换行**。
    ///
    /// Markdown 语义里段内换行是 SoftBreak（渲染成空格），但流式期间那条规则
    /// 会帮倒忙：模型正在逐行写表格，分隔行尚未到达，整块被当段落合并成一行
    /// 巨长文本；等分隔行到达又整体重排成表格。逐行渲染让中间态就已经是
    /// 「一格一行」的形态，闭合时的跳变只剩列宽，不再是结构塌陷。
    fn push_break(&mut self) {
        match self.cur.last_mut() {
            Some(last) => last.2 = true,
            None => self.cur.push((String::new(), Style::default(), true)),
        }
    }

    /// 按流式规则推送一段文本：含 `|` 的多行块逐行渲染，其余走普通段落合并。
    ///
    /// 只在 `streaming` 时启用，且只对「连续两行以上、每行都有竖线」的内容生效——
    /// 这正是模型正在写的表格；正文里偶然提到 `a | b` 的单行不受影响。
    fn push_streaming_text(&mut self, text: &str, streaming: bool) {
        if !streaming || self.in_table || self.in_code_block {
            self.push_text(text);
            return;
        }
        // 段落首行以 `|` 开头即视为「正在写表格」，此后任一行不以 `|` 开头就撤销。
        // 逐行判定而不是「整段至少两行有竖线」：后者会把正文里顺口提到的
        // `a | b` 误当成表格。
        let head = text.split('\n').next().unwrap_or_default();
        if !head.trim().is_empty() {
            self.pipe_paragraph = head.trim_start().starts_with('|');
        }
        // 极少数情况下单个 Text 事件本身就带换行
        if self.pipe_paragraph && text.contains('\n') {
            for (idx, part) in text.split('\n').enumerate() {
                if idx > 0 {
                    self.push_break();
                }
                self.push_text(part);
            }
            return;
        }
        self.push_text(text);
    }

    /// 块级元素的结构前缀：引用竖线 + 列表缩进。
    ///
    /// 代码块与表格此前各自硬编码两个空格，于是嵌在列表项里时与正文齐平顶格，
    /// 读者看不出那些行属于上面那一条。这里让它们继承当前层的 leader 与引用深度。
    fn block_indent(&self) -> Vec<Span<'static>> {
        let mut spans: Vec<Span<'static>> = Vec::new();
        for _ in 0..self.blockquote {
            spans.push(Span::styled("│ ", Theme::border()));
        }
        let base = match self.list_stack.last() {
            Some(state) => state.leader,
            // thinking 的顶层缩进由调用方统一补（同 `block_prefix`）
            None if self.thinking => 0,
            None => 2,
        };
        if base > 0 {
            spans.push(Span::raw(" ".repeat(base)));
        }
        spans
    }

    /// 段落首行前缀与续行前缀。
    ///
    /// 返回 `(head, cont)`：`head` 只用在首行（缩进 + bullet 本体 / 脚注标签），
    /// `cont` 用在每一条续行（缩进 + 同宽的空白占位）。**两者显示宽度必须相等**，
    /// 续行的正文起点才会与首行严格对齐——这就是悬挂缩进（hanging indent）。
    ///
    /// 返回值是两个 Vec 而不是「一个前缀 + 一个只出现一次的尾巴」：那样写
    /// 很容易在组装时把两者都拼上去，于是正文被推到 bullet 右边整整一个缩进。
    fn block_prefix(&mut self) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
        let mut head: Vec<Span<'static>> = Vec::new();
        let mut cont: Vec<Span<'static>> = Vec::new();

        // 块引用竖线前缀。**续行必须重放**：竖线只画在首行的话，引用块一折行
        // 就和普通段落再也分不出来了。
        for _ in 0..self.blockquote {
            head.push(Span::styled("│ ", Theme::border()));
            cont.push(Span::styled("│ ", Theme::border()));
        }

        // 列表 leader(嵌套累积:每层 = 父层 leader + 父层 bullet 宽度 + 1 空格)
        // leader 含义是"这一层 bullet 起点列宽",所以前缀空格直接 = leader。
        // 没有 bullet 的纯段落/标题不加 leader,改用下方"普通段落对齐"。
        if let Some(bullet) = self.cur_bullet.take() {
            let leader = self.list_stack.last().map(|s| s.leader).unwrap_or(0);
            if leader > 0 {
                head.push(Span::raw(" ".repeat(leader)));
                cont.push(Span::raw(" ".repeat(leader)));
            }
            let bullet_w = display_width(&bullet);
            cont.push(Span::raw(" ".repeat(bullet_w)));
            head.push(Span::styled(bullet, Theme::text_dim()));
        } else if let Some(state) = self.list_stack.last() {
            // list 内的纯段落(无 Item 包装的 loose 内容)按当前层 leader 缩进
            if state.leader > 0 {
                head.push(Span::raw(" ".repeat(state.leader)));
                cont.push(Span::raw(" ".repeat(state.leader)));
            } else if self.heading.is_none() {
                head.push(Span::raw("  "));
                cont.push(Span::raw("  "));
            }
        } else if let Some(label) = self.footnote_def.clone() {
            // 脚注定义：`[^1]: 正文` 缩进 4 格，与引用块区分开
            let tag = format!("[^{label}]: ");
            let tag_w = display_width(&tag);
            head.push(Span::raw("    "));
            head.push(Span::styled(tag, Theme::info()));
            cont.push(Span::raw(" ".repeat(4 + tag_w)));
        } else if self.def_desc {
            head.push(Span::raw("    "));
            cont.push(Span::raw("    "));
        } else if self.def_title {
            head.push(Span::raw("  "));
            cont.push(Span::raw("  "));
        } else if self.heading.is_none() && self.blockquote == 0 && !self.thinking {
            // 普通段落(无 list 包裹)加 2 空格与 User 消息视觉对齐。
            //
            // thinking 模式不加：那一层缩进由调用方统一补（见
            // `render_thinking_block`），两处都加就是 6 格，比列表 bullet
            // 还靠右，层级关系正好反了。
            head.push(Span::raw("  "));
            cont.push(Span::raw("  "));
        }
        (head, cont)
    }

    /// 在推入任何可见行之前补上待插入的空行。
    ///
    /// 块级元素之间留一行空是终端 markdown 的通行做法（glow / glamour 给每个
    /// block 都配 `block_prefix: "\n"`）：消息内部完全没有空行时，一屏长回复
    /// 读起来是一堵连续的灰字墙，段落边界只能靠句号去猜。NN/g 的扫描阅读研究
    /// 里「用空行把内容切成块」正是让长内容可扫读的主要手段。
    ///
    /// 只补**一行**：补多行会把折叠态的工具结果撑高，而一行已经足够让边界可见。
    fn ensure_blank(&mut self, lines: &mut Vec<Line<'static>>) {
        if !self.pending_blank {
            return;
        }
        self.pending_blank = false;
        // 开头与已有空行都不重复插：块间距是**块之间**的，不是消息开头就该有的。
        if lines.is_empty() || lines.last().is_some_and(|l| l.spans.is_empty()) {
            return;
        }
        lines.push(Line::from(""));
    }

    /// 将累积 span 合并成一行（超宽时折成多行），推入 lines
    fn flush(&mut self, lines: &mut Vec<Line<'static>>) {
        if self.cur.is_empty() && self.cur_bullet.is_none() {
            return;
        }
        let (head, cont) = self.block_prefix();
        let cont_width: usize = cont
            .iter()
            .map(|s| display_width(s.content.as_ref()))
            .sum::<usize>();
        let avail = self.max_width.saturating_sub(cont_width).max(1);

        // `break_after` 标记把段落切成若干段（流式表格：正在逐行写出来的一格一行），
        // 每段独立折行，首段带 bullet，续段靠 `cont` 对齐正文起点。
        let mut segments: Vec<Vec<(String, Style)>> = Vec::new();
        let mut chunk: Vec<(String, Style)> = Vec::new();
        for (text, style, break_after) in self.cur.drain(..) {
            chunk.push((text, style));
            if break_after {
                segments.push(std::mem::take(&mut chunk));
            }
        }
        segments.push(chunk);

        let mut is_first = true;
        for segment in &segments {
            for visual in wrap_spans(segment, avail) {
                if is_first {
                    self.ensure_blank(lines);
                    is_first = false;
                    lines.push(build_line(&head, &visual));
                } else {
                    lines.push(build_line(&cont, &visual));
                }
            }
        }
    }

    fn emit_code_block(&mut self, lines: &mut Vec<Line<'static>>) {
        self.ensure_blank(lines);
        if self.thinking {
            self.emit_thinking_code_block(lines);
            return;
        }
        if self.code_lang.is_empty() {
            if let Some(rows) = parse_timeline_rows(&self.code_buf) {
                let body_styles = [Theme::code_body(), Theme::body()];
                let indent = self.block_indent();
                emit_grid_table(
                    lines,
                    None,
                    &rows,
                    &[Alignment::Left; 2],
                    &body_styles,
                    &indent,
                    self.max_width,
                );
                return;
            }
        }

        let indent = self.block_indent();
        let indent_width: usize = indent
            .iter()
            .map(|s| display_width(s.content.as_ref()))
            .sum::<usize>();
        let body_width = self.max_width.saturating_sub(indent_width).max(8);

        let lang_display = if self.code_lang.is_empty() {
            String::new()
        } else {
            format!(" {} ", self.code_lang)
        };
        // 框头 `╭─` 占 2 列、右上角 `╮` 占 1 列，右框正好收在 body_width。
        let dash = body_width.saturating_sub(display_width(&lang_display) + 3);
        let mut top = indent.clone();
        top.push(Span::styled(
            format!("╭─{lang_display}{}╮", "─".repeat(dash)),
            Theme::border(),
        ));
        lines.push(Line::from(top));

        // 行号只给**带语言标签**的块。无标签块在实践中装的是 diff / JSON /
        // 命令输出 / 日志——那里行号既无法定位源文件，又把每行左移若干列，
        // 纯粹是噪音；带标签的块才是"代码"，行号用来对照编辑器里的位置。
        let total_lines = self.code_buf.lines().count();
        let number_width = total_lines.to_string().len().max(2);
        let gutter = if self.code_lang.is_empty() {
            0
        } else {
            // 数字 + 一个分隔空格
            number_width + 1
        };
        // 正文起点 = indent + `│ ` + gutter；右边线留 1 列，pad 再多留 1 列
        // （pad 的宽度取 max_line + 1 是既有约定，那一列让右边线前留一个空格）。
        let max_line = body_width.saturating_sub(4 + gutter).max(1);
        let blank_gutter = " ".repeat(gutter);

        for (idx, raw_line) in self.code_buf.lines().enumerate() {
            let lineno = format!("{:>number_width$} ", idx + 1, number_width = number_width);
            for (seg, text) in wrap_dw(raw_line, max_line).into_iter().enumerate() {
                let pad = (max_line + 1).saturating_sub(display_width(&text));
                let mut spans = indent.clone();
                spans.push(Span::styled("│ ", Theme::border()));
                // 行号**只在折行后的首段出现**：续行留等宽空白，右边线才不会
                // 因为"哪一段是续行"而左右跳动。高亮在折行之后逐段进行，
                // 所以这里拿到的是切片——行号必须按源行施加，不能按切片。
                if gutter > 0 {
                    let gutter_text = if seg == 0 {
                        lineno.clone()
                    } else {
                        blank_gutter.clone()
                    };
                    spans.push(Span::styled(gutter_text, Theme::muted()));
                }
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

        let mut bottom = indent;
        bottom.push(Span::styled(
            format!("╰{}╯", "─".repeat(body_width.saturating_sub(2))),
            Theme::border(),
        ));
        lines.push(Line::from(bottom));
    }

    /// thinking 里的围栏：只留一条 `│` 左边线 + 原文，不画方框、不加行号。
    ///
    /// 正文的代码块是"要照着执行的代码"，方框与行号帮得上忙；thinking 里的围栏
    /// 多数是路径、命令片段、半截伪代码——套上带行号的 `╭─╮` 之后，它比周围
    /// 思考亮一大截，而真正要看的是思考本身。左边线保留，是为了让"这一段是
    /// 引用的原样内容"与普通思考行仍然可分。
    fn emit_thinking_code_block(&mut self, lines: &mut Vec<Line<'static>>) {
        let indent = self.block_indent();
        let indent_width: usize = indent
            .iter()
            .map(|s| display_width(s.content.as_ref()))
            .sum::<usize>();
        let body_width = self.max_width.saturating_sub(indent_width + 2).max(8);
        for raw_line in self.code_buf.lines() {
            for wrapped in wrap_dw(raw_line, body_width) {
                let mut spans = indent.clone();
                spans.push(Span::styled("│ ", Theme::border()));
                spans.push(Span::styled(wrapped, Theme::dim()));
                lines.push(Line::from(spans));
            }
        }
    }

    /// 渲染整个表格（在 End(Table) 时调用）
    fn emit_table(&mut self, lines: &mut Vec<Line<'static>>) {
        if self.table_header.is_empty() {
            return;
        }
        self.ensure_blank(lines);
        let ncols = self.table_header.len();
        let aligns: Vec<Alignment> = (0..ncols)
            .map(|i| self.table_aligns.get(i).cloned().unwrap_or(Alignment::Left))
            .collect();
        let indent = self.block_indent();
        emit_grid_table(
            lines,
            Some(&self.table_header),
            &self.table_rows,
            &aligns,
            &[],
            &indent,
            self.max_width,
        );
    }
}

// ── 公开入口 ──────────────────────────────────────────────────────────────────

fn table_delimiter_columns(line: &str) -> Option<usize> {
    let trimmed = line.trim();
    if !trimmed.contains('|') {
        return None;
    }
    let cells = trimmed
        .trim_start_matches('|')
        .trim_end_matches('|')
        .split('|')
        .map(str::trim)
        .collect::<Vec<_>>();
    if cells.len() < 2 {
        return None;
    }
    cells
        .iter()
        .all(|cell| {
            let dashes = cell.trim_start_matches(':').trim_end_matches(':');
            !dashes.is_empty() && dashes.chars().all(|ch| ch == '-')
        })
        .then_some(cells.len())
}

fn table_header_columns(line: &str) -> Option<usize> {
    let trimmed = line.trim();
    (trimmed.starts_with('|') && trimmed.contains('|')).then(|| {
        trimmed
            .trim_start_matches('|')
            .trim_end_matches('|')
            .split('|')
            .count()
    })
}

/// 模型偶尔会把 ATX 标题和紧随其后的表头连成同一逻辑行，例如
/// "### 4. 仓位规划| 标的 | 仓位 |"。下一行虽是合法分隔行，CommonMark 仍会把整行
/// 当标题，表格自然无法成立。这里只在下一行确实是同列数分隔行时补回丢失的换行，
/// 避免把普通标题中的竖线误判成表格。
fn normalize_malformed_heading_tables(text: &str) -> Cow<'_, str> {
    let source = text.lines().collect::<Vec<_>>();
    let mut normalized = Vec::with_capacity(source.len() + 1);
    let mut changed = false;

    for (index, line) in source.iter().copied().enumerate() {
        let trimmed = line.trim_start();
        let heading_marks = trimmed.chars().take_while(|ch| *ch == '#').count();
        let is_heading = (1..=6).contains(&heading_marks)
            && trimmed
                .chars()
                .nth(heading_marks)
                .is_some_and(char::is_whitespace);
        let next_columns = source
            .get(index + 1)
            .and_then(|next| table_delimiter_columns(next));

        if is_heading {
            if let (Some(pipe), Some(delimiter_columns)) = (line.find('|'), next_columns) {
                let heading = line[..pipe].trim_end();
                let header = &line[pipe..];
                if !heading.trim_end_matches('#').trim().is_empty()
                    && table_header_columns(header) == Some(delimiter_columns)
                {
                    normalized.push(heading.to_string());
                    normalized.push(header.to_string());
                    changed = true;
                    continue;
                }
            }
        }
        normalized.push(line.to_string());
    }

    if !changed {
        return Cow::Borrowed(text);
    }
    let mut output = normalized.join("\n");
    if text.ends_with('\n') {
        output.push('\n');
    }
    Cow::Owned(output)
}

/// 将 Markdown 字符串渲染为 ratatui `Line<'static>` 列表。
///
/// `max_width`：可用字符宽度（用于内容换行和表格列宽计算）。
pub fn render_markdown(lines: &mut Vec<Line<'static>>, text: &str, max_width: usize) {
    render_markdown_inner(lines, text, max_width, false, false)
}

/// thinking 正文的渲染入口：Markdown 结构照常解析，但整块按「旁白」处理。
///
/// 与 [`render_markdown`] 的三处差异都来自同一件事——thinking 是模型**边想边写**
/// 的草稿，不是要交给用户执行的成品：
///
/// 1. **段内换行保留**（`Event::SoftBreak` 走 `push_break`）。模型的思考天然一行
///    一个念头，正文那条"段内换行折叠成空格"的规则会把它们揉成一整段。
/// 2. **围栏退化成 `│` 引用式纯文本**：thinking 里的围栏多半是路径 / 命令片段，
///    套上带行号的方框会比周围思考更抢眼。
/// 3. **整块走 `Theme::dim()` 基色**：只保留 BOLD / ITALIC 这类修饰，不换颜色。
///
/// 列表、标题、表格、行内代码照常渲染——它们恰恰是「乱」的主要来源：原实现把
/// `- ` / `1.` / `|` / `**` 原样打在屏幕上，而真实思考里约 15% 的行带这些结构。
pub fn render_thinking_markdown(lines: &mut Vec<Line<'static>>, text: &str, max_width: usize) {
    render_markdown_inner(lines, text, max_width, false, true)
}

/// 流式 thinking 的入口：尾部片段按中间态渲染。
///
/// 与 [`render_streaming_markdown`] 同源（未闭合围栏不画框），额外打开
/// thinking 的两条规则（软换行保留、围栏退化成引用式）。流式期间每帧只喂
/// 尾部 [`render::LIVE_TAIL_LINES`] 行，输入有硬上界，解析成本与整段思考
/// 的长度无关。
pub fn render_streaming_thinking_markdown(
    lines: &mut Vec<Line<'static>>,
    text: &str,
    max_width: usize,
) {
    render_markdown_inner(lines, text, max_width, true, true)
}

/// **流式期间**的渲染入口。差异只有两处，且都只在未定稿时生效——定稿走
/// [`render_markdown`]，所以下面那些"中间态"规则绝不会污染最终显示：
///
/// 1. 未闭合的围栏按纯文本渲染，不画框、不做语法高亮；
/// 2. 正在逐行写、还没成形表格的段落按行渲染，不合并成一整行。
///
/// 两者针对的都是同一个现象：pulldown-cmark 是树式解析，未完成的内容会被
/// 当成**已完成的**结构，于是围栏一开就画出完整边框、表格一成形就整体重排，
/// 用户看着内容在一帧帧地"变形"。等它闭合反而是最后一件事。
pub fn render_streaming_markdown(lines: &mut Vec<Line<'static>>, text: &str, max_width: usize) {
    render_markdown_inner(lines, text, max_width, true, false)
}

fn render_markdown_inner(
    lines: &mut Vec<Line<'static>>,
    text: &str,
    max_width: usize,
    streaming: bool,
    thinking: bool,
) {
    // 五项都开：STRIKETHROUGH 早就开着却没有对应分支（文本保留、无修饰），
    // TASKLISTS 不开的话 `- [ ]` 原样显示成字面量——模型**很爱**用任务列表
    // 汇报进度，字面量 checkbox 在终端里读起来像代码而不是待办。
    // DEFINITION_LIST 同理：`: 术语` 不开解析就是一段以冒号开头的普通文字。
    let opts = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_DEFINITION_LIST;
    let normalized = normalize_malformed_heading_tables(text);
    // 流式期间把未闭合的围栏摘出来单独渲染：`@@`…不对，围栏本身。pulldown 到 EOF
    // 时未闭合的围栏照样产出完整的 CodeBlock 事件流，事件层**无法**区分。
    let (parseable, unclosed) = if streaming {
        split_unclosed_fence(&normalized)
    } else {
        (normalized.as_ref(), None)
    };
    let parser = Parser::new_ext(parseable, opts);
    let mut c = Ctx::new(max_width);
    c.thinking = thinking;

    for event in parser {
        match event {
            // ─── 标题 ───────────────────────────────────────────────────────
            Event::Start(Tag::Heading { level, .. }) => {
                c.pending_blank = true;
                c.heading = Some(match level {
                    HeadingLevel::H1 => 1,
                    HeadingLevel::H2 => 2,
                    _ => 3,
                });
                let (prefix, prefix_style) = match level {
                    HeadingLevel::H1 => ("█ ", Theme::assistant_prefix()),
                    HeadingLevel::H2 => (
                        "▌ ",
                        Style::default()
                            .fg(Theme::text_color())
                            .add_modifier(Modifier::BOLD),
                    ),
                    // h3–h6 共用一个前缀：终端没有字号可用，层级只能靠缩进表现。
                    // 逐级加宽前缀（`▎` × n）会让深层标题顶到第 6 列，比标题本身
                    // 还要显眼，深层小标题在长文档里本来就该最不显眼。
                    //
                    // 前缀色与正文色一致：旧实现所有标题前缀都用品牌橙，于是
                    // 「▎ 小标题」的橙色前缀比它自己的灰色正文还亮，层级读起来
                    // 正好反了。
                    _ => (
                        "▎ ",
                        Style::default()
                            .fg(Theme::text_dim_color())
                            .add_modifier(Modifier::BOLD),
                    ),
                };
                c.cur.push((prefix.to_string(), prefix_style, false));
            }
            Event::End(TagEnd::Heading(_)) => {
                c.flush(lines);
                c.heading = None;
                c.pending_blank = true;
            }

            // ─── 段落 ───────────────────────────────────────────────────────
            Event::Start(Tag::Paragraph) => c.pipe_paragraph = false,
            Event::End(TagEnd::Paragraph) => {
                c.flush(lines);
                // 列表内部保持紧凑：loose list 每项之间都空一行的话，
                // 三项的清单会散成六段，反而看不出是一个列表。
                if c.list_stack.is_empty() {
                    c.pending_blank = true;
                }
            }

            // ─── 定义列表 ───────────────────────────────────────────────────
            Event::Start(Tag::DefinitionList) => c.pending_blank = true,
            Event::End(TagEnd::DefinitionList) => c.pending_blank = true,
            Event::Start(Tag::DefinitionListTitle) => {
                c.flush(lines);
                c.def_title = true;
            }
            Event::End(TagEnd::DefinitionListTitle) => {
                c.flush(lines);
                c.def_title = false;
            }
            Event::Start(Tag::DefinitionListDefinition) => {
                c.flush(lines);
                c.def_desc = true;
            }
            Event::End(TagEnd::DefinitionListDefinition) => {
                c.flush(lines);
                c.def_desc = false;
            }

            // ─── 块引用 ─────────────────────────────────────────────────────
            Event::Start(Tag::BlockQuote(_)) => {
                c.flush(lines);
                c.pending_blank = true;
                c.blockquote += 1;
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                c.flush(lines);
                c.blockquote = c.blockquote.saturating_sub(1);
                c.pending_blank = true;
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
                c.pending_blank = true;
            }

            // ─── 列表 ───────────────────────────────────────────────────────
            Event::Start(Tag::List(start)) => {
                // 进新 list 层前先把上一层的"半成品"cur flush 掉,避免上层 Item
                // 内容被带进新层的 bullet 之后再渲染(典型症状:父 Item 文本
                // "盘中观察" 和嵌套子项 bullet "• " 拼到一行)。同一 list 内嵌套
                // 也是 Item 结束就会 flush,但这里覆盖 list end 之外的另一种顺序。
                c.flush(lines);
                // 顶层列表前后留白，嵌套列表**不留**：嵌在父项里的子列表一空行
                // 就被拆散成两个独立块，层级关系反而看不见了。
                c.pending_blank = c.list_stack.is_empty();
                // 新层 leader = 父层 leader + 父层 bullet 宽度 + 1(空格)。
                // 顶层 list 父层为空时 leader = 2(等同原 indent=1 的 2 空格缩进)。
                // 这样保证:同层不同 bullet 宽度(•/1./10.)的文字列对齐;嵌套
                // 子层 bullet 起点落在父层文字起点之后,而不是按嵌套层数 ×2。
                let leader = match c.list_stack.last() {
                    Some(parent) => {
                        let sample = parent.sample_bullet();
                        parent.leader + display_width(&sample) + 1
                    }
                    None => 2,
                };
                c.list_stack.push(ListState {
                    is_ordered: start.is_some(),
                    next_n: start.unwrap_or(1) as usize,
                    leader,
                });
            }
            Event::End(TagEnd::List(_)) => {
                // 出 list 时先把最后一个 Item 的 cur flush,然后再 pop,
                // 否则 Item 内容会留在 cur 里被下次 flush 带错位置。
                c.flush(lines);
                c.list_stack.pop();
                if c.list_stack.is_empty() {
                    c.pending_blank = true;
                }
            }
            Event::Start(Tag::Item) => {
                let bullet = if let Some(state) = c.list_stack.last_mut() {
                    if state.is_ordered {
                        let s = format!("{}. ", state.next_n);
                        state.next_n += 1;
                        s
                    } else {
                        "• ".to_string()
                    }
                } else {
                    "• ".to_string()
                };
                c.cur_bullet = Some(bullet);
            }
            Event::End(TagEnd::Item) => {
                c.flush(lines);
            }
            // 任务列表勾选框：`TaskListMarker` 紧跟在 `Start(Item)` 之后，
            // 所以直接覆盖刚设好的 bullet。宽度与 `• ` 同为 2，leader 对齐不受影响。
            Event::TaskListMarker(checked) => {
                c.cur_bullet = Some(if checked {
                    "\u{2611} ".to_string()
                } else {
                    "\u{2610} ".to_string()
                });
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
                c.pending_blank = true;
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
                    // 在链接文字后追加灰色 URL 注释。**必须走 push_text**：它带着
                    // `in_table` 分支，直接 `cur.push` 会让表格里的链接把 URL 甩到
                    // 表格外面（`emit_table` 是 `&self`，不会顺手 flush 掉残留）。
                    c.push_styled_text(&format!(" ({url})"), Theme::muted());
                }
            }

            // ─── 行内样式 ───────────────────────────────────────────────────
            Event::Start(Tag::Strong) => c.strong += 1,
            Event::End(TagEnd::Strong) => c.strong = c.strong.saturating_sub(1),
            Event::Start(Tag::Emphasis) => c.em += 1,
            Event::End(TagEnd::Emphasis) => c.em = c.em.saturating_sub(1),
            Event::Start(Tag::Strikethrough) => c.strike += 1,
            Event::End(TagEnd::Strikethrough) => c.strike = c.strike.saturating_sub(1),

            // ─── 脚注 ───────────────────────────────────────────────────────
            // 终端没有上标/下标，引用处只能退回 `[^1]` 字面形式；定义块渲染成
            // 缩进的 `[^1]: 正文`，与普通段落区分得开即可。
            Event::FootnoteReference(label) => {
                c.push_styled_text(&format!("[^{label}]"), Theme::info());
            }
            Event::Start(Tag::FootnoteDefinition(label)) => {
                c.flush(lines);
                c.footnote_def = Some(label.to_string());
            }
            Event::End(TagEnd::FootnoteDefinition) => {
                c.flush(lines);
                c.footnote_def = None;
                c.pending_blank = true;
            }
            Event::Code(text) => {
                // 行内代码：用 ` 包裹显示。与链接 URL 同理，必须走 `push_styled_text`
                // 才能落进表格单元格——旧实现直接 `cur.push`，于是表格里的
                // `常量名` 被留下在 cur 里，等表格结束后和下一段粘成一行。
                let style = Theme::inline_code();
                c.push_styled_text(&format!("`{text}`"), style);
            }

            // ─── 图片 ───────────────────────────────────────────────────────
            // 旧实现 `Tag::Image` 落进 `_ => {}`：只剩 alt 文本，URL 直接消失，
            // 用户看到「架构图」三个字却没法打开它。补成 `[图片] alt → url`，
            // URL 会被 hyperlink.rs 的 linkify 识别成可点链接。
            Event::Start(Tag::Image { dest_url, .. }) => {
                c.push_text("[图片] ");
                c.image_url = Some(dest_url.to_string());
            }
            Event::End(TagEnd::Image) => {
                if let Some(url) = c.image_url.take() {
                    c.push_text(&format!(" → {url}"));
                }
            }

            // ─── HTML ───────────────────────────────────────────────────────
            // 原样 dim 输出而不是静默吞掉。`<div align="center">…</div>` 这类
            // 装饰标签模型偶尔会吐出来，整块消失等于内容丢失。
            Event::Html(html) => {
                let trimmed = html.trim();
                if !trimmed.is_empty() {
                    c.flush(lines);
                    c.ensure_blank(lines);
                    c.push_text(trimmed);
                    c.flush(lines);
                    c.pending_blank = true;
                }
            }
            Event::InlineHtml(html) => {
                let trimmed = html.trim();
                if !trimmed.is_empty() {
                    c.push_text(trimmed);
                }
            }

            // ─── 文本内容 ───────────────────────────────────────────────────
            Event::Text(text) => {
                if c.in_code_block {
                    c.code_buf.push_str(&text);
                } else {
                    c.push_streaming_text(&text, streaming);
                }
            }
            Event::SoftBreak => {
                // 正在逐行输出的表格：段内换行必须保留成换行，否则三行表会被
                // 压成一行，读起来像乱码。普通段落照旧折叠成空格。
                //
                // thinking 同理，且理由更强：模型的思考本来就是一行一个念头，
                // 合并成一段后层次全没了（见 `Ctx.thinking` 的注释）。用
                // `push_break` 而不是 `flush`：前者在**同一段内**分段，bullet
                // 与续行缩进仍由 `block_prefix` 统一处理；后者会提前消耗
                // `cur_bullet`，列表项的后续行会退化成无 bullet 的普通段落。
                if c.thinking || (streaming && c.pipe_paragraph) {
                    c.push_break();
                } else {
                    c.push_text(" ");
                }
            }
            Event::HardBreak => {
                if c.in_table {
                    c.cur_cell.push('\n');
                } else {
                    c.flush(lines);
                }
            }

            // ─── 水平分隔线 ─────────────────────────────────────────────────
            Event::Rule => {
                c.flush(lines);
                c.ensure_blank(lines);
                lines.push(Line::from(Span::styled(
                    "─".repeat(max_width.min(60)),
                    Theme::border(),
                )));
                c.pending_blank = true;
            }

            _ => {}
        }
    }

    // 刷新最后可能残留的内容
    c.flush(lines);
    if let Some(tail) = unclosed {
        render_unclosed_fence(lines, tail, max_width);
    }
    // 收尾不留悬空空行：块间留白是为了切分内容，不是为了在消息末尾多一行。
    // 调用方（`render.rs`）还会自己补一行作为消息间距，这里留着就是两行。
    while lines.last().is_some_and(|l| l.spans.is_empty()) {
        lines.pop();
    }
}

/// 切出"最后一个围栏还没闭合"的那段文本。
///
/// pulldown-cmark 在 EOF 时会把未闭合围栏当成**已闭合**的 CodeBlock 产出，
/// 事件流与真闭合完全同形——所以判定只能落在文本层：数围栏行的开合配对。
/// 返回 `(可正常解析的部分, 未闭合的尾巴)`。
fn split_unclosed_fence(text: &str) -> (&str, Option<&str>) {
    // (起始字节偏移, 围栏字符, 该围栏最少要几个字符才闭合)
    let mut open: Option<(usize, char, usize)> = None;
    let mut offset = 0usize;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        // CommonMark：围栏缩进不得超过 3 个空格
        if line.len() - trimmed.len() <= 3 {
            if let Some(marker) = trimmed.chars().next().filter(|c| *c == '`' || *c == '~') {
                let run = trimmed.chars().take_while(|c| *c == marker).count();
                let info = trimmed[run..].trim();
                // 反引号围栏的 info string 里不能再出现反引号
                let openable = marker == '~' || !info.contains('`');
                if openable {
                    open = match open {
                        // 关闭围栏：同字符、长度不短于开启长度、且 info 为空
                        Some((_, c, len)) if c == marker && run >= len && info.is_empty() => None,
                        _ => Some((offset, marker, run)),
                    };
                }
            }
        }
        offset += line.len();
    }
    match open {
        Some((start, ..)) => (&text[..start], Some(&text[start..])),
        None => (text, None),
    }
}

/// 未闭合的围栏按 **dim 纯文本**渲染：不画 `╭─ lang ─╮` 框，也不做语法高亮。
///
/// 理由：框一旦画出来，用户就认为代码块已经完成，接下来内容会在框里逐行
/// 填充，闭合瞬间框体再整体重排一次——那个"框凭空出现又变形"的过程比
/// 纯文本刺眼得多。纯文本在流式期间稳定，闭合时一次性切换成完整框。
fn render_unclosed_fence(lines: &mut Vec<Line<'static>>, tail: &str, max_width: usize) {
    for raw in tail.lines() {
        for wrapped in wrap_dw(raw, max_width.saturating_sub(4).max(1)) {
            lines.push(Line::from(Span::styled(
                format!("  {wrapped}"),
                Theme::text_dim(),
            )));
        }
    }
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
        // span 布局 [indent, "│ ", 正文, pad+"│"]：indent 是继承自列表 / 引用的
        // 结构前缀（顶层恒为 2 空格），它是 Span::raw，不该带边框色。
        assert_eq!(lines[1].spans[0].style, Style::default());
        assert_eq!(lines[1].spans[1].style, Theme::border());
        assert_eq!(lines[1].spans[2].style, Theme::code_body());
        assert_eq!(lines[1].spans.last().unwrap().style, Theme::border());
        assert_ne!(Theme::border(), Theme::code_body());
    }

    /// 时间线（无语言标签的围栏里装 `HH:MM  内容`）与 markdown 表格共用一套网格。
    /// 数据行之间**不画横线**——旧实现每行一条，三行数据就是四条横线，竖线被切成
    /// 一格一格的小格子，噪音比它承载的信息量大。
    #[test]
    fn fenced_timeline_uses_the_same_grid_and_row_separators_as_tables() {
        let text =
            "```\n09:00  半导体能不能追？\n09:15  苏州招商能不能建？\n09:30  海康能不能加仓？\n```";
        let mut lines = vec![];
        render_markdown(&mut lines, text, 80);
        let rendered = rendered_text(&lines);

        assert_eq!(rendered.len(), 5, "顶线/3 行/底线（数据行之间无横线）");
        assert!(rendered[0].trim_start().starts_with('┌') && rendered[0].contains('┬'));
        assert!(rendered[4].trim_start().starts_with('└') && rendered[4].contains('┴'));
        for row in &rendered[1..4] {
            assert!(
                !row.contains('├') && !row.contains('┼'),
                "数据行之间不该有横线：{row:?}"
            );
        }

        let first_row = &lines[1];
        assert_eq!(first_row.spans[0].style, Style::default());
        assert_eq!(first_row.spans[1].style, Theme::border());
        assert_eq!(first_row.spans[2].style, Theme::code_body());
        assert_eq!(first_row.spans[3].style, Theme::border());
        // 第二列是普通正文，走 `Theme::body()` 而不是透传终端前景色
        assert_eq!(first_row.spans[4].style, Theme::body());
        assert_eq!(first_row.spans.last().unwrap().style, Theme::border());
    }

    /// P0-1：表格单元格里的行内代码、脚注引用、链接 URL 必须落进单元格。
    ///
    /// 三者过去都直接 `cur.push(...)`，绕开了 `push_text` 里的 `in_table` 分支；
    /// 而 `emit_table` 是 `&self` 不会顺手 flush，文本就滞留在 `cur` 里，等表格
    /// 结束后和下一段粘成一行——表现是「单元格空白 + 表格后面多出一串代码」。
    #[test]
    fn inline_code_footnotes_and_links_stay_inside_table_cells() {
        let text = "| 项 | 值 |\n|---|---|\n| 阈值 | `MAX_DIFF_LINES` |\n\
                   | 引用 | 见 [仓库](https://example.com/x) |\n| 脚注 | 详见[^1] |\n\n收尾段落。\n\n[^1]: 脚注内容。\n";
        let mut lines = vec![];
        render_markdown(&mut lines, text, 100);
        let rendered = rendered_text(&lines);
        let joined = rendered.join("\n");

        assert!(joined.contains("`MAX_DIFF_LINES`"), "{joined}");
        assert!(joined.contains("https://example.com/x"), "{joined}");
        assert!(joined.contains("[^1]"), "{joined}");
        // 收尾段落必须自成一行，且往前最近的一行是表格底线——不能被表格里
        // 漏出来的文本粘连。
        let tail = rendered
            .iter()
            .position(|l| l.contains("收尾段落"))
            .expect("收尾段落");
        let prev = rendered[..tail]
            .iter()
            .rev()
            .find(|l| !l.trim().is_empty())
            .expect("表格底线");
        assert!(
            prev.trim_start().starts_with('└'),
            "收尾段落前应隔一个空行、然后是表格底线：{rendered:?}"
        );
        assert!(
            !rendered[tail].contains('`'),
            "行内代码不该泄漏到表格外：{rendered:?}"
        );
    }

    /// P0-2：列数超出宽度预算时退化成缩进列表，而不是把每列压到 1~2 个字符。
    ///
    /// 旧实现会照画网格，然后被 `Paragraph::wrap` 从中间二次折行——24 列的六列表
    /// 右边框 `┐` `┘` 各占一整行，竖线彻底错位。退化后**逐列信息全保留**，
    /// 只失去列对齐。
    #[test]
    fn overwide_table_degrades_to_an_indented_list_instead_of_a_broken_grid() {
        let text = "| 列一 | 列二 | 列三 | 列四 | 列五 | 列六 |\n|---|---|---|---|---|---|\n\
                   | aaaa | bbbb | cccc | dddd | eeee | ffff |\n";
        for width in [24usize, 30, 40] {
            let mut lines = vec![];
            render_markdown(&mut lines, text, width);
            let rendered = rendered_text(&lines);
            for line in &rendered {
                assert!(
                    !line.contains('┌') && !line.contains('│'),
                    "{width} 列下退化形态不该再出现网格：{line}"
                );
                assert!(display_width(line) <= width, "{width} 列下溢出：{line}");
            }
            // 表头行与数据行都在，且**逐个列值**一个不少（退化形态不保证
            // 分隔符不被折行拆开，所以只能逐值查，不能查整行）。
            let joined = rendered.join("\n");
            for value in [
                "列一", "列二", "列三", "列四", "列五", "列六", "aaaa", "bbbb", "cccc", "dddd",
                "eeee", "ffff",
            ] {
                assert!(
                    joined.contains(value),
                    "{width} 列下丢了列值 {value}：{joined}"
                );
            }
        }
    }

    /// 列数够用时**仍然走网格**——退化是窄屏兜底，不是有意改成列表。
    #[test]
    fn wide_enough_table_keeps_the_grid() {
        let text = "| 名称 | 令牌 | 成本 |\n|---|---|---|\n| 输入 | 289,112 | ~$0.0867 |\n";
        let mut lines = vec![];
        render_markdown(&mut lines, text, 60);
        let rendered = rendered_text(&lines);

        assert!(rendered[0].trim_start().starts_with('┌'));
        assert!(rendered[2].trim_start().starts_with('├'));
        assert!(rendered[4].trim_start().starts_with('└'));
    }

    /// P1-1：markdown 层自己做宽度折行，**续行重放结构前缀**。
    ///
    /// 折行交给 `Paragraph::wrap` 时，WordWrapper 按空白分词（中文整段是一个
    /// 「词」，只能逐字硬切），且续行不重放任何前缀——引用块的 `│` 消失、列表的
    /// leader 消失、段落的 2 格缩进消失。层级在第二行起整体塌陷。
    #[test]
    fn wrapped_continuation_lines_replay_the_structural_prefix() {
        let long = "这是一段足够长的中文内容，用来看看窄屏折行之后引用竖线和列表缩进还能不能留在原处，从而判断续行是不是继承了首行的结构前缀。";
        let md = format!("> {long}\n\n- {long}\n");
        let mut lines = vec![];
        render_markdown(&mut lines, &md, 40);
        let rendered = rendered_text(&lines);

        let quote_lines: Vec<&String> = rendered.iter().filter(|l| l.starts_with('│')).collect();
        assert!(quote_lines.len() >= 3, "引用应折成多行：{rendered:?}");
        for line in &quote_lines {
            assert!(
                line.starts_with("│ "),
                "引用块的每一物理行都必须带竖线：{line}"
            );
        }

        let item_lines: Vec<&String> = rendered
            .iter()
            .filter(|l| l.starts_with("  •") || l.starts_with("    "))
            .collect();
        assert!(item_lines.len() >= 3, "列表项应折成多行：{rendered:?}");
        for line in item_lines.iter().skip(1) {
            assert_eq!(
                line.chars().take_while(|c| *c == ' ').count(),
                4,
                "列表项续行应缩进到正文起点（leader 2 + bullet 2）：{line}"
            );
        }
    }

    /// 任何 markdown 输出行都不得超过 `max_width`——`Paragraph::wrap` 因此
    /// 对 assistant 正文基本闲置，它剩下的职责只有非 markdown 行（工具输出等）。
    #[test]
    fn no_rendered_line_exceeds_the_given_width() {
        let md = "普通段落中文内容相当长需要在窄屏折行，另外加一句英文 mixed content to test \
                   word wrapping with very long identifiers like MAX_DIFF_LINES_VERY_LONG_NAME here.\n\n\
                   | 现象 | 根因 | 修复 | 优先级 |\n|---|---|---|---|\n| 一个很长的中文单元格内容 | bb | cc | dd |\n\n\
                   - 列表项很长很长很长很长很长很长很长很长很长很长很长\n\n\
                   > 引用也很长很长很长很长很长很长很长很长很长很长很长很长\n\n\
                   ```rust\nlet some_very_long_name = another_function(argument_one, argument_two, three);\n```\n";
        for width in [20usize, 24, 30, 40, 60, 80, 120] {
            let mut lines = vec![];
            render_markdown(&mut lines, md, width);
            for line in rendered_text(&lines) {
                assert!(
                    display_width(line.trim_end()) <= width,
                    "宽度 {width} 下溢出：{line}"
                );
            }
        }
    }

    /// P1-1 附带：中文禁则——收尾标点不落行首，开括号 / 开引号不落行尾。
    #[test]
    fn cjk_punctuation_never_starts_a_wrapped_line() {
        let md = "这是一个测试中文折行禁则处理的段落，句号、逗号、顿号、右括号，还有引号\
                   “测试”，它们都不应该出现在行首位置，同样，开引号和左括号也不应该落在行尾。\
                   请在窄终端下观察效果，如果某一行以逗号或者句号开头，那就是禁则没有生效。";
        for width in [30usize, 40, 48, 56, 64] {
            let mut lines = vec![];
            render_markdown(&mut lines, md, width);
            for line in rendered_text(&lines) {
                let trimmed = line.trim_start();
                if trimmed.is_empty() {
                    continue;
                }
                let first = trimmed.chars().next().expect("非空行");
                assert!(
                    !is_forbidden_line_start(first),
                    "宽度 {width}下行首出现收尾标点 {first:?}：{line}"
                );
                assert!(
                    !trimmed.ends_with('（') && !trimmed.ends_with('“'),
                    "宽度 {width} 下行尾出现开引号类字符：{line}"
                );
            }
        }
    }

    /// 成对重复符号（`——`）不被劈成两行：它被合并成单个原子，
    /// 断点只会落在它整体的前后。
    #[test]
    fn paired_repeat_symbols_are_not_split_across_lines() {
        let md = "这是一段测试破折号的文字，我们需要确认连续两个破折号不会被拆到两行里去，\
                   否则读者会看到一行以破折号开头，非常莫名其妙的一个排版事故现场。";
        for width in [30usize, 40, 50, 60] {
            let mut lines = vec![];
            render_markdown(&mut lines, md, width);
            for line in rendered_text(&lines) {
                assert!(
                    !line.trim_start().starts_with('—'),
                    "宽度 {width} 下破折号被劈到行首：{line}"
                );
            }
        }
    }

    /// P2-1：代码块 / 表格嵌在列表项里时必须跟着 bullet 一起缩进。
    #[test]
    fn code_blocks_and_tables_inside_list_items_inherit_the_leader_indent() {
        let md = "1. 第一步\n\n   ```rust\n   fn main() {}\n   ```\n\n   | a | b |\n   |---|---|\n   | 1 | 2 |\n";
        let mut lines = vec![];
        render_markdown(&mut lines, md, 80);
        let rendered = rendered_text(&lines);

        for marker in ["╭─ rust", "┌───", "└───"] {
            let line = rendered
                .iter()
                .find(|l| l.contains(marker))
                .unwrap_or_else(|| panic!("找不到 {marker}：{rendered:?}"));
            assert!(
                line.starts_with("  "),
                "{marker} 应缩进到列表 leader（2 格）而不是顶格：{line:?}"
            );
        }
    }

    /// P2-2：图片 / HTML 不再被静默吞掉。
    ///
    /// `Tag::Image` 与 `Event::Html` 过去都落进 `_ => {}`：图片只剩 alt 文本、
    /// URL 消失（用户没法打开），HTML 块整块不见。静默丢失内容比降级更糟。
    #[test]
    fn images_html_and_definition_lists_are_rendered_not_swallowed() {
        let mut lines = vec![];
        render_markdown(
            &mut lines,
            "![架构图](https://example.com/a.png)\n\n\
             <div align=\"center\">装饰标签</div>\n\n术语\n: 术语的说明\n",
            80,
        );
        let rendered = rendered_text(&lines);
        let joined = rendered.join("\n");

        assert!(joined.contains("[图片]"), "{joined}");
        assert!(joined.contains("架构图"), "{joined}");
        assert!(
            joined.contains("https://example.com/a.png"),
            "图片 URL 必须留下，否则用户没法打开：{joined}"
        );
        assert!(joined.contains("<div align=\"center\">"), "{joined}");
        assert!(joined.contains("术语"), "{joined}");
        // 术语 2 格、说明 4 格，用缩进区分两者
        let term = rendered
            .iter()
            .position(|l| l.contains("术语") && !l.contains("说明"));
        let desc = rendered.iter().position(|l| l.contains("说明"));
        assert!(term.is_some() && desc.is_some(), "{joined}");
        let indent = |l: &String| l.chars().take_while(|c| *c == ' ').count();
        assert_eq!(indent(&rendered[term.unwrap()]), 2, "{joined}");
        assert_eq!(indent(&rendered[desc.unwrap()]), 4, "{joined}");
    }

    /// 任务列表：`☐`/`☑` 必须取代 bullet 出现，且勾选框宽度与 `• ` 同为 2，
    /// 否则列表文字会随「有序/无序」左右错位一格。
    #[test]
    fn task_lists_render_checkboxes_instead_of_bullets() {
        let mut lines = vec![];
        render_markdown(&mut lines, "- [ ] 待办一\n- [x] 待办二\n- 普通项", 80);
        let rendered = rendered_text(&lines);

        assert!(rendered[0].contains('\u{2610}'), "{rendered:?}");
        assert!(rendered[0].contains("待办一"), "{rendered:?}");
        assert!(rendered[1].contains('\u{2611}'), "{rendered:?}");
        assert!(rendered[2].contains('•'), "{rendered:?}");
        // 三行的正文起点列必须一致
        let text_cols: Vec<usize> = rendered
            .iter()
            .map(|l| l.find("待").or_else(|| l.find("普")).unwrap_or(usize::MAX))
            .collect();
        assert_eq!(
            text_cols[0], text_cols[1],
            "勾选框宽度与 bullet 不一致会导致文字错位: {rendered:?}"
        );
    }

    /// 删除线此前**开着选项却没有分支**：文本保留、无任何修饰符，读者完全看不出
    /// 那是删除线。这里断言修饰符真的落到了 span 上。
    #[test]
    fn strikethrough_applies_the_crossed_out_modifier() {
        let mut lines = vec![];
        render_markdown(&mut lines, "保留 ~~划掉~~ 保留", 80);

        let struck: Vec<&str> = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .filter(|s| s.style.add_modifier.contains(Modifier::CROSSED_OUT))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(struck, vec!["划掉"]);
    }

    /// 围栏判定的边界：`~~~`、缩进 4 空格（那是缩进代码块，不是围栏）、
    /// 闭合围栏比开启时更长、以及「关掉的围栏又开了新的」。
    ///
    /// 这些都是模型真实会写出来的形状；判错的后果是**流式期间画框/不画框反了**，
    /// 用户看着一个已经完成的代码块在最后一行突然变形。
    #[test]
    fn fence_detection_handles_tilde_indent_and_relength() {
        // `~~~` 围栏同样配平
        assert_eq!(split_unclosed_fence("~~~rust\nx\n~~~\n").1, None);
        assert!(split_unclosed_fence("~~~rust\nx\n").1.is_some());
        // 缩进 4 空格：三反引号在这种缩进下是缩进代码块的一部分，不能当围栏开
        assert_eq!(
            split_unclosed_fence("    ```rust\n    x\n    ```\n").1,
            None,
            "4 空格缩进的 ``` 不该被当成围栏"
        );
        // 闭合围栏比开启的更长：合法（CommonMark 允许）
        assert_eq!(split_unclosed_fence("```\nx\n`````\n").1, None);
        // 关闭围栏不能带 info string：带了就仍然算「开」
        assert!(split_unclosed_fence("```\nx\n``` rust\n").1.is_some());
        // 关掉一个、再开一个且没关：尾巴从第二个算起
        let (head, tail) = split_unclosed_fence("```\na\n```\n\n```\nb\n");
        assert_eq!(head, "```\na\n```\n\n");
        assert_eq!(tail, Some("```\nb\n"));
    }

    /// 首行为空行的代码块：行号必须从 1 连续编下去，空行也要占一个号。
    #[test]
    fn code_block_line_numbers_survive_blank_lines() {
        let mut lines = vec![];
        render_markdown(&mut lines, "```rust\n\nfn main() {}\n\n", 80);
        let rendered = rendered_text(&lines);

        let numbered: Vec<String> = rendered
            .iter()
            .filter(|l| l.contains('│'))
            .map(|l| l.trim_start_matches("  │ ").to_string())
            .collect();
        // 首行的空行与末行的空行**都要占行号**：模型在闭合围栏前多打一个换行，
        // 就是"代码块里有一个空行"，VS Code / git diff 也是这么显示的。
        assert_eq!(numbered.len(), 3, "{rendered:?}");
        assert!(numbered[0].starts_with(" 1 "), "{numbered:?}");
        assert!(numbered[1].starts_with(" 2 "), "{numbered:?}");
        assert!(numbered[2].starts_with(" 3 "), "{numbered:?}");
    }

    /// 行号列宽按 `max(2, 总行数位数)` 取，但**右边线必须仍然闭合**——行号变宽
    /// 吃掉的是正文可用宽度，不是边框位置。1/2/3 位都试一遍。
    #[test]
    fn code_block_gutter_growth_does_not_break_the_frame() {
        for (n, expect_width) in [(9usize, 2usize), (10, 2), (99, 2), (100, 3)] {
            let body: String = (0..n).map(|i| format!("l{i};\n")).collect();
            let mut lines = vec![];
            render_markdown(&mut lines, &format!("```rust\n{body}```"), 60);
            let rendered = rendered_text(&lines);

            // 行号列宽 = 行号 span 的长度（数字右对齐 + 1 个分隔空格）。
            // span 布局是 [indent, "│ ", 行号, 正文…, pad+"│"]——前两项是继承
            // 自列表 / 引用的结构前缀（见 `Ctx::block_indent`），在顶层场景恒为
            // 2 空格 + 竖线。
            let gutter = lines
                .iter()
                .filter_map(|l| l.spans.get(2))
                .map(|s| s.content.as_ref().len())
                .next()
                .unwrap_or(0);
            assert_eq!(
                gutter,
                expect_width + 1,
                "{n} 行时行号列宽应为 {expect_width} 位：{rendered:?}"
            );
            for line in &lines {
                let w: usize = line
                    .spans
                    .iter()
                    .map(|s| display_width(s.content.as_ref()))
                    .sum();
                assert_eq!(w, 60, "{n} 行时右边线不齐：{rendered:?}");
            }
        }
    }

    /// 代码块的每一行显示宽度必须**恰好**等于 `max_width`——右边线对齐是框
    /// 「看起来闭合」的唯一依据，差一列就会在长行或 CJK 混排处裂开一道缝。
    /// 这一条同时验证了三件事：行号 gutter 宽度算对、折行按**显示宽度**而非
    /// 字节数切、中文与 ASCII 混排时的 pad 补偿正确。
    #[test]
    fn code_block_right_edge_closes_exactly_at_max_width() {
        let cases = [
            "```rust\nfn main() {}\n```",
            "```rust\nlet a = 1; // 中文注释也要参与宽度计算\n```",
            "```\n纯中文内容没有任何 ASCII 字符\n```",
            "```rust\nlet x = \"一个非常非常长的中文字符串字面量用来触发折行\";\n```",
        ];
        for width in [20usize, 40, 60, 100, 200] {
            for case in cases {
                let mut lines = vec![];
                render_markdown(&mut lines, case, width);
                for line in &lines {
                    let w: usize = line
                        .spans
                        .iter()
                        .map(|s| display_width(s.content.as_ref()))
                        .sum();
                    assert_eq!(
                        w, width,
                        "width={width} 时代码块右边线不齐（{w} != {width}）：\n输入 {case:?}\n渲染 {lines:?}"
                    );
                }
            }
        }
    }

    /// 脚注：引用处出 `[^1]`，定义块出缩进的 `[^1]: 正文`。
    #[test]
    fn footnotes_render_reference_and_definition_distinctly() {
        let mut lines = vec![];
        render_markdown(&mut lines, "见引用[^1]\n\n[^1]: 这是脚注正文", 80);
        let rendered = rendered_text(&lines);

        assert!(rendered[0].contains("[^1]"), "{rendered:?}");
        assert!(
            rendered.iter().any(|l| l.contains("[^1]: 这是脚注正文")),
            "{rendered:?}"
        );
    }

    /// 流式中间态：围栏还没闭合时**不画框**。
    ///
    /// 这是"内容在眼前变形"的根源——pulldown 对未闭合围栏照样产出完整的
    /// CodeBlock 事件流，框一开就画出来，随后每帧在框里长内容，闭合时再整体
    /// 重排一次。定稿路径不受影响，仍是完整的框 + 语法高亮。
    #[test]
    fn streaming_unclosed_fence_renders_as_plain_text_until_it_closes() {
        let partial = "看这段：\n\n```rust\nfn main() {\n    let x = 42;\n";
        let mut lines = vec![];
        render_streaming_markdown(&mut lines, partial, 80);
        let streaming = rendered_text(&lines);

        assert!(
            !streaming.iter().any(|l| l.contains('╭') || l.contains('╰')),
            "未闭合围栏不该画框：{streaming:?}"
        );
        assert!(
            streaming.iter().any(|l| l.contains("let x = 42;")),
            "代码内容仍要可见：{streaming:?}"
        );

        // 闭合之后（同一条消息定稿）恢复完整形态
        let closed = format!("{partial}}}\n```");
        let mut lines = vec![];
        render_markdown(&mut lines, &closed, 80);
        let final_form = rendered_text(&lines);
        assert!(
            final_form.iter().any(|l| l.contains("╭─ rust")),
            "闭合后应画出带语言标签的框：{final_form:?}"
        );
    }

    /// `split_unclosed_fence` 的配平判定：成对的围栏、中间嵌套、以及
    /// ``` 内部含反引号的 info string（那种反引号不是围栏）。
    #[test]
    fn fence_pairing_detects_only_the_trailing_unclosed_one() {
        assert_eq!(split_unclosed_fence("```\na\n```\n").1, None);
        assert_eq!(split_unclosed_fence("普通段落").1, None);
        // 前两个闭合、第三个没闭合 → 尾巴从第三个开始
        let text = "```\na\n```\n\n```rust\nb\n";
        let (head, tail) = split_unclosed_fence(text);
        assert_eq!(head, "```\na\n```\n\n");
        assert_eq!(tail, Some("```rust\nb\n"));
        // ``` 内出现反引号：那行不是关闭围栏
        let (head, tail) = split_unclosed_fence("```rust\nlet s = \"`\";\n");
        assert!(head.is_empty(), "{head:?}");
        assert!(tail.is_some());
    }

    /// 流式中间态：正在逐行写的表格按行渲染，不被合并成一整行。
    ///
    /// 段内换行在 Markdown 语义里是 SoftBreak（渲染成空格），但对正在输出的
    /// 表格来说那等于把三行压成一行巨长文本——分隔行一到又整体重排。
    #[test]
    fn streaming_half_written_table_keeps_one_row_per_line() {
        let mut lines = vec![];
        render_streaming_markdown(&mut lines, "| 名称 | 值 |\n| 输入 | 1 |\n", 80);
        let rendered = rendered_text(&lines);

        assert_eq!(
            rendered.len(),
            2,
            "两行表格应占两行而不是合并成一行：{rendered:?}"
        );
        assert!(rendered[0].contains("名称"), "{rendered:?}");
        assert!(rendered[1].contains("输入"), "{rendered:?}");
    }

    /// 定稿路径**不**受流式规则影响：完整表格仍走表格渲染（框线对齐）。
    #[test]
    fn finalized_table_still_renders_as_a_table() {
        let mut lines = vec![];
        render_markdown(
            &mut lines,
            "| 名称 | 值 |\n| --- | ---: |\n| 输入 | 1 |\n",
            80,
        );
        let rendered = rendered_text(&lines);

        // 表格整体缩进 2 空格与正文对齐，所以要从 trim 后的行首找框线。
        assert!(
            rendered.iter().any(|l| l.trim_start().starts_with('┌')),
            "{rendered:?}"
        );
        assert!(rendered.iter().any(|l| l.contains('┤')), "{rendered:?}");
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

    /// 块级元素之间**留一行空**。
    ///
    /// 这条断言曾经反过来（「零空行」），旧实现把段落 / 列表 / 表格 / 引用块 /
    /// 分隔线全部挤在一起，一屏长回复读起来是一堵连续的灰字墙，段落边界只能靠
    /// 句号去猜。对照 glow / glamour：每个 block 都配 `block_prefix: "\n"`；
    /// NN/g 的扫描阅读研究里「用空行把内容切成块」是长内容可扫读的前提。
    ///
    /// 注意**只留一行**：补多行会把折叠态的工具结果撑高，而一行已足够让边界可见。
    /// 另外消息首尾不留悬空空行——那是块**之间**的间距，不是消息的开头与结尾。
    #[test]
    fn blocks_are_separated_by_exactly_one_blank_line() {
        let mut lines = vec![];
        render_markdown(&mut lines, "第一段\n\n第二段\n\n## 标题\n\n正文", 80);
        let rendered = rendered_text(&lines);

        assert_eq!(
            rendered.len(),
            7,
            "两段一段一标题一段，各自之间应恰好一行空：{rendered:?}"
        );
        assert!(rendered[0].contains("第一段"));
        assert!(rendered[1].trim().is_empty());
        assert!(rendered[2].contains("第二段"));
        assert!(rendered[3].trim().is_empty());
        assert!(rendered[4].contains("标题"));
        assert!(rendered[5].trim().is_empty());
        assert!(rendered[6].contains("正文"));
        // 块间距只出现在块**之间**：绝不连着来两行空，也不在开头 / 结尾留空。
        assert!(
            rendered
                .windows(2)
                .all(|w| !(w[0].trim().is_empty() && w[1].trim().is_empty())),
            "不应出现连续空行：{rendered:?}"
        );
    }

    /// 列表内部保持紧凑：loose list（项与项之间本来就空一行）若每项再补一行，
    /// 三项的清单会散成六段，反而看不出这是一个列表。
    #[test]
    fn list_items_stay_compact_inside_one_block() {
        let mut lines = vec![];
        render_markdown(&mut lines, "引子\n\n- 甲\n- 乙\n- 丙\n\n收尾", 80);
        let rendered = rendered_text(&lines);

        assert_eq!(rendered.len(), 7, "列表块内部不该有空行：{rendered:?}");
        assert!(rendered[1].trim().is_empty(), "列表整体与前一段隔一行");
        assert!(rendered[2].contains("甲"));
        assert!(rendered[3].contains("乙"));
        assert!(rendered[4].contains("丙"));
        assert!(rendered[5].trim().is_empty(), "列表整体与后一段隔一行");
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
    fn heading_joined_to_table_header_is_recovered() {
        let text = "## 📌 当前持仓快照| 标的 | 数量 | 均价 | 现价 | 浮盈 | 风险敞口 | 触发红线 |\n|---|---|---|---|---|---|---|\n| 海康威视 | 200 | 36.635 | 36.37 | **-¥53** | ¥541 (0.78%) | 亏损方向加仓 ✗ |";
        let mut lines = vec![];
        render_markdown(&mut lines, text, 160);
        let rendered = rendered_text(&lines);

        assert!(rendered[0].contains("📌 当前持仓快照"));
        assert!(!rendered[0].contains("| 标的"));
        // 标题与表格之间有一个块间空行，表格整体缩进 2 空格
        assert!(
            rendered[1].trim().is_empty(),
            "标题与表格之间应留一行空：{rendered:?}"
        );
        assert!(rendered[2].trim_start().starts_with('┌'));
        assert!(rendered[3].trim_start().starts_with('│') && rendered[3].contains("标的"));
        assert!(rendered[4].trim_start().starts_with('├') && rendered[4].contains('┼'));
        assert!(rendered.iter().any(|line| line.contains("海康威视")));
        assert!(
            rendered.iter().all(|line| !line.contains("|---|")),
            "分隔行不应作为原始 Markdown 文本显示"
        );
    }

    #[test]
    fn numbered_section_heading_joined_to_table_header_is_recovered() {
        let text = "### 4. 本周总体仓位规划| 标的 | 建议仓位 | 理由 |\n|---|---|---|\n| 华天科技 | 20-25% | 主线，进攻仓位 |\n| **现金** | **35-60%** | 当前0仓，留弹药 |";
        let mut lines = vec![];
        render_markdown(&mut lines, text, 120);
        let rendered = rendered_text(&lines);

        assert!(rendered[0].contains("4. 本周总体仓位规划"));
        assert!(!rendered[0].contains("| 标的"));
        assert!(
            rendered[1].trim().is_empty(),
            "标题与表格之间应留一行空：{rendered:?}"
        );
        assert!(rendered[2].trim_start().starts_with('┌'));
        assert!(rendered[3].trim_start().starts_with('│') && rendered[3].contains("建议仓位"));
        assert!(rendered.iter().any(|line| line.contains("华天科技")));
        assert!(rendered.iter().any(|line| line.contains("35-60%")));
        assert!(rendered.iter().all(|line| !line.contains("|---|")));
    }

    /// 表格只在「表头下」画一条横线，数据行之间**不画**。
    ///
    /// 旧实现每条正文记录都配一条，3 行数据就是 4 条横线，竖线被切成一格一格的
    /// 小格子——横线承载的信息量是零（列已经由竖线分开了），视觉噪音却最大。
    #[test]
    fn table_keeps_one_separator_under_the_header_and_none_between_body_rows() {
        let text = "| 标的 | 现持仓 |\n| --- | ---: |\n| 兴蓉 | 800 股 |\n| 海康 | 200 股 |\n| 总仓位 | 18.8% |";
        let mut lines = vec![];
        render_markdown(&mut lines, text, 80);
        let rendered = rendered_text(&lines);

        assert_eq!(rendered.len(), 7, "顶/表头/表头线/3 行/底");
        assert!(rendered[0].trim_start().starts_with('┌'));
        assert!(rendered[2].trim_start().starts_with('├') && rendered[2].contains('┼'));
        for index in [3, 4, 5] {
            assert!(
                !rendered[index].contains('├') && !rendered[index].contains('┼'),
                "正文行之间不该有横线: {}",
                rendered[index]
            );
        }
        assert!(rendered[6].trim_start().starts_with('└'));
    }

    #[test]
    fn table_uses_subdued_borders_without_dimming_cell_content() {
        let mut lines = vec![];
        render_markdown(
            &mut lines,
            "| 标的 | 仓位 |\n| --- | --- |\n| 兴蓉 | 800 |",
            80,
        );

        // span 布局 [indent(2 空格), "│ ", 单元格…, pad+"│"]
        let body = &lines[3];
        assert_eq!(body.spans[0].style, Style::default());
        assert_eq!(body.spans[1].style, Theme::border());
        // 单元格是正文层，颜色由主题决定，不随终端前景色漂移
        assert_eq!(body.spans[2].style, Theme::body());
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
            if line.trim_start().starts_with('│') {
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
            .take_while(|line| !line.trim_start().starts_with('└'))
            .filter(|line| line.trim_start().starts_with('│'))
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

    /// 嵌套 list 对齐回归：bullet 宽度可变（`•` 2 列、`1.` 3 列、`10.` 4 列），
    /// 子层 bullet 起始位置应对齐到父层文字起始列之后,而不是按嵌套层数 ×2。
    ///
    /// 修复前:
    ///   - 嵌套 unordered 子项 bullet 起点错位 1 列,与兄弟项视觉不齐;
    ///   - List start 时没 flush 上层 Item 内容,导致父项文本和子项 bullet 拼到一行
    ///     ("      • 盘中观察子项 A")。
    ///
    /// 修复后:每层 leader = 父层 leader + 父层 bullet 宽度 + 1,
    /// 嵌套 list 父项 / 子项 / 后续兄弟项 各自独立一行。
    #[test]
    fn nested_list_text_columns_align_across_bullet_widths() {
        let text = "1. 🟢 开盘前准备\n2. 盘中观察\n   - 子项 A\n   - 子项 B\n3. 收盘";
        let mut lines = vec![];
        render_markdown(&mut lines, text, 80);
        let rendered = rendered_text(&lines);

        // L1 ordered "1. "/ "2. "/ "3. " 宽 3 列,前缀 2 空格缩进。
        assert!(
            rendered[0].starts_with("  1. "),
            "L1 ordered 第 1 项应有 2 空格缩进 + '1. ': {:?}",
            rendered[0]
        );
        assert!(rendered[0].contains("开盘前准备"));
        assert!(
            rendered[1].starts_with("  2. "),
            "L1 ordered 第 2 项不应被嵌套 list 吞掉: {:?}",
            rendered[1]
        );
        assert!(rendered[1].contains("盘中观察"));
        assert!(
            rendered.last().unwrap().starts_with("  3. "),
            "L1 ordered 第 3 项('收盘')应独立一行: {:?}",
            rendered
        );

        // L2 unordered 子项 bullet 起点列 = L1 leader(2) + L1 bullet 宽(3) + 1
        // = 6,文字起始列 = 6 + 2 = 8。
        let l2_lines: Vec<&String> = rendered
            .iter()
            .filter(|line| line.contains("子项"))
            .collect();
        assert_eq!(
            l2_lines.len(),
            2,
            "嵌套子项不应与父项合并,实际行: {:?}",
            rendered
        );
        for line in &l2_lines {
            let leader_cols: usize = line.chars().take_while(|c| *c == ' ').count();
            assert_eq!(
                leader_cols, 6,
                "L2 子项 bullet 应在列 6(= 父 leader 2 + 父 bullet 宽 3 + 1): {:?}",
                line
            );
        }
    }

    /// 多级嵌套时 leader 逐层累积。L2 子层嵌套一个 L3 unordered 子层,
    /// L3 bullet 起点 = L2 leader + L2 bullet 宽 + 1 = 6 + 2 + 1 = 9。
    /// 验证连续嵌套下 leader 公式正确,不会丢失任何一级缩进。
    #[test]
    fn triple_nested_list_leader_accumulates_per_level() {
        let text = "1. L1-A\n   - L2-A\n     - L3-A\n2. L1-B";
        let mut lines = vec![];
        render_markdown(&mut lines, text, 100);
        let rendered = rendered_text(&lines);

        assert!(rendered
            .iter()
            .any(|l| l.starts_with("  1. ") && l.contains("L1-A")));
        assert!(rendered
            .iter()
            .any(|l| l.starts_with("      ") && l.contains("L2-A")));
        assert!(
            rendered
                .iter()
                .any(|l| l.starts_with("         ") && l.contains("L3-A")),
            "L3 bullet 应在列 9(= 6 + 2 + 1),实际行: {:?}",
            rendered
        );
        assert!(
            rendered
                .iter()
                .any(|l| l.starts_with("  2. ") && l.contains("L1-B")),
            "L1-B 应独立一行,实际行: {:?}",
            rendered
        );
    }
}
