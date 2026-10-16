//! 共享 diff 生成 — 用 similar crate 计算行级 diff
//!
//! 供 Edit / Write 工具把代码改动以 unified-diff 风格输出，
//! 便于 TUI 直接带 `+`/`-` 配色渲染，也便于 AI 在下轮对话中看到具体改动。

use similar::{DiffTag, TextDiff};

/// 每个变更块前后保留的未改动上下文行数。
///
/// 上下文只是「改动落在哪」的定位锚点，对齐 `git diff -U3` / Claude Code。
/// 取 3 而不是更多：Edit 改一行就把整个函数（十几行）原样倒出来，和「全部
/// 展示」没有本质区别，只是把噪音从 500 行缩到 20 行。
const CONTEXT_LINES: usize = 3;

/// 生成行级 diff 文本。
///
/// 格式：每个变更块前有一行 `@@ -old_start,old_len +new_start,new_len @@`，上下文行以两个
/// 空格开头，删除行以 `- ` 开头，新增行以 `+ ` 开头。**只输出含改动的块**，块之间未被
/// 任何改动触及的连续未改动行折成一行 `… N unchanged lines`。
///
/// - `old` / `new` 为原始文本（按行切分）。
/// - `max_lines` 限制输出行数（含 `@@` 头与截断提示行），避免超长 diff 撑爆输出。
///   超限时末尾追加 `…(truncated, N more lines)`。
///
/// ## 为什么输出 `@@` 头
///
/// 渲染层要显示「旧侧/新侧双列行号」，而**行号只能从块头推算**：第一个变更块之前
/// 被丢掉的未改动内容没有留下任何标记（`… N unchanged lines` 只出现在块**之间**），
/// 所以渲染端从 1 开始数会把第 100 行的改动标成第 1 行。块头一给出绝对起点，每块都能
/// 独立重置，多 hunk 文件也就不会一路错位到底。
///
/// 用 `@@` 而不是自创格式还有两个理由：它是 unified diff 的既有语法，**模型认识**
/// （看到 `@@ -12,7 +12,9 @@` 比看到无头的裸 diff 更清楚改动落在哪），且每个块只多
/// 二十来个字符——相比"每行都加行号前缀"（200 行 diff 多约 400 token），成本可忽略。
///
/// ## 为什么不能遍历「全部行」
///
/// 旧实现用 `diff.iter_all_changes()`，那会**逐行输出 old/new 的每一行**，
/// 只有前缀不同：改一行 → 回执里塞进整个文件。三处后果：
///
/// 1. TUI 里用户看到整份文件而非「改了什么」（对齐 Claude Code 的第一诉求）。
/// 2. `tool_result` 原样进下一轮对话，改一行白烧几百 token，且大片未改动代码
///    会稀释模型对真实改动的注意力。
/// 3. 最严重的是 `max_lines` 按**总行数**截断：文件超过 200 行时，改动若落在
///    文件后半部分，diff 会在**走到改动点之前**就 `…(truncated)` 掉——用户和
///    模型都根本看不到这次改了什么。按块输出后输出量只与改动量成正比，
///    局部编辑永远不会触发截断。
pub fn make_diff(old: &str, new: &str, max_lines: usize) -> String {
    let diff = TextDiff::from_lines(old, new);
    let old_lines = diff.old_slices();
    let new_lines = diff.new_slices();

    // 先摊平成 (前缀, 行文本) 序列；块间跳过的未改动行在此处折成一行。
    // `grouped_ops` 按 `CONTEXT_LINES` 把 op 聚成变更块并**剔除无改动的块**，
    // 因此每组至少含一个非 Equal op（old == new 时 groups 为空 → 输出空串）。
    let mut rows: Vec<(&str, String)> = Vec::new();
    let mut prev_old_end = 0usize;
    for group in diff.grouped_ops(CONTEXT_LINES) {
        let (Some(first), Some(last)) = (group.first(), group.last()) else {
            continue;
        };
        // 两个块之间未被触及的连续未改动行：第一块之前不算（前面没有「跳过」）。
        if !rows.is_empty() {
            let skipped = first.old_range().start.saturating_sub(prev_old_end);
            if skipped > 0 {
                rows.push(("", format!("… {skipped} unchanged lines")));
            }
        }
        rows.push(("@@", hunk_header(&group)));
        for op in &group {
            let (tag, old_range, new_range) = op.as_tag_tuple();
            match tag {
                DiffTag::Equal => {
                    rows.extend(old_lines[old_range].iter().map(|l| ("  ", l.to_string())))
                }
                DiffTag::Delete => {
                    rows.extend(old_lines[old_range].iter().map(|l| ("- ", l.to_string())))
                }
                DiffTag::Insert => {
                    rows.extend(new_lines[new_range].iter().map(|l| ("+ ", l.to_string())))
                }
                // similar 把「删一段 + 插一段」合并成 Replace；展开成 unified diff
                // 的惯例顺序：先全部 `-` 再全部 `+`。
                DiffTag::Replace => {
                    rows.extend(old_lines[old_range].iter().map(|l| ("- ", l.to_string())));
                    rows.extend(new_lines[new_range].iter().map(|l| ("+ ", l.to_string())));
                }
            }
        }
        prev_old_end = last.old_range().end;
    }

    let mut out = String::new();
    for (idx, (prefix, text)) in rows.iter().enumerate() {
        if idx >= max_lines {
            out.push_str(&format!("…(truncated, {} more lines)\n", rows.len() - idx));
            break;
        }
        out.push_str(prefix);
        out.push_str(text);
        // similar 按行切，slice 已含尾部换行（末行可能没有），保证行尾
        if !text.ends_with('\n') {
            out.push('\n');
        }
    }

    // 移除末尾多余换行
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

/// 一个变更块的头行：`@@ -old_start,old_len +new_start,new_len @@`
///
/// 起点取 `first` 的 range 起点（+1 转成 1-based），长度取整个块的跨度。
/// 纯 Insert 块的 `old_range` 是空区间，此时起点表示「插入点落在旧文件第几行之后」——
/// 渲染层不需要区分这种情况，它只把这行当作"从这里开始数"的锚点。
fn hunk_header(group: &[similar::DiffOp]) -> String {
    let (Some(first), Some(last)) = (group.first(), group.last()) else {
        return String::new();
    };
    let old_start = first.old_range().start + 1;
    let new_start = first.new_range().start + 1;
    let old_len = last.old_range().end.saturating_sub(first.old_range().start);
    let new_len = last.new_range().end.saturating_sub(first.new_range().start);
    format!(" -{old_start},{old_len} +{new_start},{new_len} @@")
}

/// 新文件专用：整份内容作为纯新增（全行 `+`），用于 Write 创建新文件的场景。
pub fn make_added(content: &str, max_lines: usize) -> String {
    let mut out = String::new();
    let mut emitted = 0usize;
    let total = content.lines().count();

    for (emitted_idx, line) in content.lines().enumerate() {
        if emitted >= max_lines {
            let rest = total.saturating_sub(emitted_idx);
            out.push_str(&format!("…(truncated, {rest} more lines)\n"));
            break;
        }
        out.push_str("+ ");
        out.push_str(line);
        out.push('\n');
        emitted = emitted_idx + 1;
    }

    if out.ends_with('\n') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_basic_change() {
        let d = make_diff("a\nb\nc\n", "a\nB\nc\n", 200);
        let lines: Vec<&str> = d.lines().collect();
        assert!(lines.contains(&"  a"));
        assert!(lines.contains(&"- b"));
        assert!(lines.contains(&"+ B"));
        assert!(lines.contains(&"  c"));
    }

    #[test]
    fn diff_insert_only() {
        let d = make_diff("a\n", "a\nb\n", 200);
        let lines: Vec<&str> = d.lines().collect();
        assert!(lines.contains(&"  a"));
        assert!(lines.contains(&"+ b"));
        assert!(!lines.iter().any(|l| l.starts_with("- ")));
    }

    #[test]
    fn diff_truncation() {
        let old = (0..50)
            .map(|i| format!("old{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let new = (0..50)
            .map(|i| format!("new{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let d = make_diff(&old, &new, 10);
        assert!(d.contains("…(truncated"));
    }

    #[test]
    fn added_all_plus() {
        let d = make_added("hello\nworld\n", 200);
        let lines: Vec<&str> = d.lines().collect();
        assert_eq!(lines, vec!["+ hello", "+ world"]);
    }

    #[test]
    fn added_truncation() {
        let content = (0..30)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let d = make_added(&content, 5);
        assert!(d.contains("…(truncated"));
        let emitted = d.lines().filter(|l| l.starts_with("+ ")).count();
        assert_eq!(emitted, 5);
    }

    #[test]
    fn diff_identical_empty_output() {
        let d = make_diff("same\n", "same\n", 200);
        // 无改动 = 无块 = 无输出。「遍历全部行」的实现会在这里吐出 `  same`。
        assert_eq!(d, "");
    }

    /// 回归钉子：大文件里改 1 行，不得把整份文件倒进回执。
    #[test]
    fn diff_only_emits_changed_blocks_not_the_whole_file() {
        let old = (0..500)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let new = old.replacen("line250", "line250_CHANGED", 1);
        let d = make_diff(&old, &new, 200);
        let lines: Vec<&str> = d.lines().collect();
        assert!(lines.contains(&"- line250"), "{d}");
        assert!(lines.contains(&"+ line250_CHANGED"), "{d}");
        assert!(
            !lines.contains(&"  line0"),
            "未改动的行不该被逐行倒出: {lines:?}"
        );
        assert!(
            !d.contains("truncated"),
            "改动量远低于上限，不该被截断: {d}"
        );
        // 单个变更块：一行 `@@` 头 + 前后各 3 行上下文 + 1 组 -/+，其余未改动内容不出现。
        // 没有「跳过」提示是正确的——文件里只有这一处改动，没有东西被跳过。
        assert_eq!(lines.len(), 9, "{lines:?}");
        assert!(!d.contains("unchanged lines"), "{d}");
        // 块头必须给出**绝对**起点：改的是第 250 行，上下文是 247..253，
        // 所以头行是 -248（248 起、共 7 行）。渲染层靠它把行号钉在源文件位置上，
        // 从 1 开始数会把这次改动标成第 3 行。
        assert_eq!(lines[0], "@@ -248,7 +248,7 @@", "{d}");
    }

    /// 多 hunk 文件里**每块各自**给出自己的起点，渲染层据此逐块重置行号。
    #[test]
    fn diff_hunk_headers_carry_absolute_positions_per_block() {
        let old = (0..40)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let new = old
            .replacen("line5", "line5_X", 1)
            .replacen("line30", "line30_Y", 1);
        let d = make_diff(&old, &new, 200);
        let headers: Vec<&str> = d.lines().filter(|l| l.starts_with("@@")).collect();
        assert_eq!(headers.len(), 2, "{d}");
        // 第一块：改动在第 6 行，上下文 3..9 → 起点 3、共 7 行
        assert_eq!(headers[0], "@@ -3,7 +3,7 @@", "{d}");
        // 第二块：改动在第 31 行，上下文 28..34 → 起点 28、共 7 行
        assert_eq!(headers[1], "@@ -28,7 +28,7 @@", "{d}");
    }

    /// 在文件末尾追加一行：similar 把它与末行并成一个 Replace 组，头行的新侧
    /// 长度因此比旧侧多 1。头行必须如实反映这个差异——渲染层要靠它决定新侧行号
    /// 从哪开始、递增几步。
    #[test]
    fn diff_hunk_header_reflects_unequal_old_and_new_lengths() {
        let old = (0..10)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let new = format!("{old}\nappended");
        let d = make_diff(&old, &new, 200);
        let header = d.lines().find(|l| l.starts_with("@@")).expect("应有块头");
        assert_eq!(header, "@@ -7,4 +7,5 @@", "{d}");
        assert!(d.contains("+ appended"), "{d}");
    }

    /// 回归钉子：`max_lines` 曾按**总行数**计费，导致文件后部的改动在走到
    /// 改动点之前就被截断——用户和模型都看不到这次改了什么。
    #[test]
    fn diff_change_near_end_of_large_file_survives_truncation() {
        let old = (0..500)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let new = old.replacen("line499", "line499_CHANGED", 1);
        let d = make_diff(&old, &new, 200);
        assert!(d.contains("+ line499_CHANGED"), "改动被截断吞掉: {d}");
        assert!(!d.contains("truncated"), "{d}");
    }

    #[test]
    fn diff_multiple_changes_get_context_between_them() {
        let old = (0..40)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let new = old
            .replacen("line5", "line5_X", 1)
            .replacen("line30", "line30_Y", 1);
        let d = make_diff(&old, &new, 200);
        assert!(d.contains("- line5\n"), "{d}");
        assert!(d.contains("+ line30_Y"), "{d}");
        // 两处改动之间隔着 20+ 行未改动内容，必须折行而不是原样输出。
        assert_eq!(
            d.matches("unchanged lines").count(),
            1,
            "两处改动之间应只有一行跳过提示: {d}"
        );
    }

    /// 首尾不得出现「跳过」提示——那里并没有跳过任何东西。
    #[test]
    fn diff_does_not_emit_leading_or_trailing_skip_markers() {
        let old = (0..30)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let new = old.replacen("l15", "l15_NEW", 1);
        let d = make_diff(&old, &new, 200);
        let first = d.lines().next().unwrap();
        assert!(
            !first.contains("unchanged lines"),
            "首个变更块之前不该有跳过提示: {first}"
        );
    }

    #[test]
    fn diff_truncation_reports_remaining_rows() {
        let old = (0..80)
            .map(|i| format!("old{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let new = (0..80)
            .map(|i| format!("new{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let d = make_diff(&old, &new, 5);
        assert_eq!(d.lines().count(), 6, "5 行正文 + 1 行截断提示: {d}");
        assert!(
            d.lines().last().unwrap().starts_with("…(truncated, "),
            "{d}"
        );
    }
}
