//! 多行输入框状态

/// 结构化附件在编辑器里的内部原子标记。
///
/// 它不会直接渲染，也不会进入发送给模型的正文；渲染层会按附件类型把它替换成
/// [Image #n] / [File: ...]。把标记放进真实编辑序列，才能让占位符跟随粘贴时
/// 的光标位置，并自然参与左右移动、换行和普通文字插入。
pub(crate) const ATTACHMENT_MARKER: char = '\u{fffc}';

/// 终端列宽：CJK 全角字符=2，其余=1
pub fn char_width(c: char) -> usize {
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

#[derive(Default, Clone)]
pub struct InputBox {
    pub lines: Vec<String>,
    pub cursor_row: usize,
    pub cursor_col: usize,
}

impl InputBox {
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            cursor_row: 0,
            cursor_col: 0,
        }
    }

    /// cursor_col 是字符索引（Unicode scalar 数量），此方法转换为字节偏移量
    fn cursor_byte_offset(&self) -> usize {
        let line = &self.lines[self.cursor_row];
        line.char_indices()
            .nth(self.cursor_col)
            .map(|(i, _)| i)
            .unwrap_or(line.len())
    }

    pub fn insert_char(&mut self, c: char) {
        if self.cursor_row >= self.lines.len() {
            self.lines.push(String::new());
        }
        let byte_offset = self.cursor_byte_offset();
        self.lines[self.cursor_row].insert(byte_offset, c);
        self.cursor_col += 1;
    }

    /// 批量插入文本（bracketed paste 使用）：'\n' 当换行处理，'\r' 过滤，其余字符逐一插入
    pub fn insert_text(&mut self, text: &str) {
        for c in text.chars() {
            match c {
                '\n' => self.insert_newline(),
                '\r' => {}
                c => self.insert_char(c),
            }
        }
    }

    /// 在当前光标处插入一个结构化附件原子。
    pub(crate) fn insert_attachment_marker(&mut self) {
        self.insert_char(ATTACHMENT_MARKER);
    }

    fn attachment_marker_count(&self) -> usize {
        self.lines
            .iter()
            .map(|line| line.chars().filter(|ch| *ch == ATTACHMENT_MARKER).count())
            .sum()
    }

    /// 清理附件列表已经移除后遗留的内部标记，并为旧状态中缺少标记的附件补一个
    /// 行首兼容锚点。正常的新粘贴路径始终是一份附件对应一个标记。
    pub(crate) fn reconcile_attachment_markers(&mut self, attachment_count: usize) {
        while self.attachment_marker_count() > attachment_count {
            self.remove_attachment_marker(attachment_count);
        }

        let missing = attachment_count.saturating_sub(self.attachment_marker_count());
        for _ in 0..missing {
            self.lines[0].insert(0, ATTACHMENT_MARKER);
            if self.cursor_row == 0 {
                self.cursor_col += 1;
            }
        }
    }

    fn attachment_marker_ordinal_at(&self, row: usize, col: usize) -> Option<usize> {
        let marker = self.lines.get(row)?.chars().nth(col)?;
        if marker != ATTACHMENT_MARKER {
            return None;
        }

        Some(
            self.lines[..row]
                .iter()
                .map(|line| line.chars().filter(|ch| *ch == ATTACHMENT_MARKER).count())
                .sum::<usize>()
                + self.lines[row]
                    .chars()
                    .take(col)
                    .filter(|ch| *ch == ATTACHMENT_MARKER)
                    .count(),
        )
    }

    pub(crate) fn attachment_marker_before_cursor(&self) -> Option<usize> {
        (self.cursor_col > 0)
            .then(|| self.attachment_marker_ordinal_at(self.cursor_row, self.cursor_col - 1))
            .flatten()
    }

    pub(crate) fn attachment_marker_at_cursor(&self) -> Option<usize> {
        self.attachment_marker_ordinal_at(self.cursor_row, self.cursor_col)
    }

    /// 新附件应插入结构化附件向量的位置。顺序按编辑器中的可视位置计算，而不是
    /// 一律追加到末尾，保证用户把光标移到前文后再粘贴时，标记、编号、删除和发送
    /// 顺序仍指向同一个附件。
    pub(crate) fn attachment_insertion_index(&self) -> usize {
        self.lines[..self.cursor_row]
            .iter()
            .map(|line| line.chars().filter(|ch| *ch == ATTACHMENT_MARKER).count())
            .sum::<usize>()
            + self.lines[self.cursor_row]
                .chars()
                .take(self.cursor_col)
                .filter(|ch| *ch == ATTACHMENT_MARKER)
                .count()
    }

    fn remove_attachment_marker(&mut self, ordinal: usize) -> bool {
        let mut seen = 0usize;
        for row in 0..self.lines.len() {
            let marker_col = self.lines[row].chars().enumerate().find_map(|(col, ch)| {
                if ch != ATTACHMENT_MARKER {
                    return None;
                }
                if seen == ordinal {
                    Some(col)
                } else {
                    seen += 1;
                    None
                }
            });
            let Some(col) = marker_col else {
                continue;
            };
            let byte = self.lines[row]
                .char_indices()
                .nth(col)
                .map(|(byte, _)| byte)
                .unwrap_or(self.lines[row].len());
            self.lines[row].remove(byte);
            if self.cursor_row == row && self.cursor_col > col {
                self.cursor_col -= 1;
            }
            return true;
        }
        false
    }

    pub fn insert_newline(&mut self) {
        let byte_offset = self.cursor_byte_offset();
        let rest = self.lines[self.cursor_row].split_off(byte_offset);
        self.lines.insert(self.cursor_row + 1, rest);
        self.cursor_row += 1;
        self.cursor_col = 0;
    }

    pub fn backspace(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
            let byte_offset = self.cursor_byte_offset();
            self.lines[self.cursor_row].remove(byte_offset);
        } else if self.cursor_row > 0 {
            let current = self.lines.remove(self.cursor_row);
            self.cursor_row -= 1;
            self.cursor_col = self.lines[self.cursor_row].chars().count();
            self.lines[self.cursor_row].push_str(&current);
        }
    }

    pub fn move_left(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        } else if self.cursor_row > 0 {
            self.cursor_row -= 1;
            self.cursor_col = self.lines[self.cursor_row].chars().count();
        }
    }

    pub fn move_right(&mut self) {
        let line_char_count = self.lines[self.cursor_row].chars().count();
        if self.cursor_col < line_char_count {
            self.cursor_col += 1;
        } else if self.cursor_row + 1 < self.lines.len() {
            self.cursor_row += 1;
            self.cursor_col = 0;
        }
    }

    /// Up — 光标移到上一逻辑行相同列（跨行，与视觉折行无关）；已在首行时不做任何事
    pub fn move_cursor_up(&mut self) {
        if self.cursor_row == 0 {
            return;
        }
        self.cursor_row -= 1;
        let line_len = self.lines[self.cursor_row].chars().count();
        self.cursor_col = self.cursor_col.min(line_len);
    }

    /// Down — 光标移到下一逻辑行相同列
    pub fn move_cursor_down(&mut self) {
        if self.cursor_row + 1 >= self.lines.len() {
            return;
        }
        self.cursor_row += 1;
        let line_len = self.lines[self.cursor_row].chars().count();
        self.cursor_col = self.cursor_col.min(line_len);
    }

    /// Ctrl+A / Home — 跳到行首
    pub fn move_to_start_of_line(&mut self) {
        self.cursor_col = 0;
    }

    /// Ctrl+E / End — 跳到行尾
    pub fn move_to_end_of_line(&mut self) {
        self.cursor_col = self.lines[self.cursor_row].chars().count();
    }

    /// Ctrl+Left / Alt+Left — 向左跳一个单词（跨行则移到上行末）
    pub fn move_word_backward(&mut self) {
        if self.cursor_col == 0 {
            if self.cursor_row > 0 {
                self.cursor_row -= 1;
                self.cursor_col = self.lines[self.cursor_row].chars().count();
            }
            return;
        }
        let chars: Vec<char> = self.lines[self.cursor_row].chars().collect();
        let mut pos = self.cursor_col;
        while pos > 0 && chars[pos - 1] != ATTACHMENT_MARKER && !chars[pos - 1].is_alphanumeric() {
            pos -= 1;
        }
        if pos > 0 && chars[pos - 1] == ATTACHMENT_MARKER {
            self.cursor_col = pos;
            return;
        }
        while pos > 0 && chars[pos - 1].is_alphanumeric() {
            pos -= 1;
        }
        self.cursor_col = pos;
    }

    /// Ctrl+Right / Alt+Right — 向右跳一个单词（跨行则移到下行首）
    pub fn move_word_forward(&mut self) {
        let line_len = self.lines[self.cursor_row].chars().count();
        if self.cursor_col >= line_len {
            if self.cursor_row + 1 < self.lines.len() {
                self.cursor_row += 1;
                self.cursor_col = 0;
            }
            return;
        }
        let chars: Vec<char> = self.lines[self.cursor_row].chars().collect();
        let mut pos = self.cursor_col;
        while pos < chars.len() && chars[pos] != ATTACHMENT_MARKER && !chars[pos].is_alphanumeric()
        {
            pos += 1;
        }
        if pos < chars.len() && chars[pos] == ATTACHMENT_MARKER {
            self.cursor_col = pos;
            return;
        }
        while pos < chars.len() && chars[pos].is_alphanumeric() {
            pos += 1;
        }
        self.cursor_col = pos;
    }

    /// Ctrl+W / Alt+Backspace — 删掉光标左侧一个词（跨行则合并上一行）
    pub fn delete_word_backward(&mut self) {
        if self.cursor_col == 0 {
            if self.cursor_row > 0 {
                let cur = self.lines.remove(self.cursor_row);
                self.cursor_row -= 1;
                self.cursor_col = self.lines[self.cursor_row].chars().count();
                self.lines[self.cursor_row].push_str(&cur);
            }
            return;
        }
        let saved = self.cursor_col;
        self.move_word_backward();
        let start = self.cursor_col;
        let chars: Vec<char> = self.lines[self.cursor_row].chars().collect();
        self.lines[self.cursor_row] = chars[..start].iter().chain(chars[saved..].iter()).collect();
    }

    /// Ctrl+K — 删掉光标到行尾（已在行尾则合并下一行）
    pub fn kill_to_end_of_line(&mut self) {
        let line_len = self.lines[self.cursor_row].chars().count();
        if self.cursor_col >= line_len {
            if self.cursor_row + 1 < self.lines.len() {
                let next = self.lines.remove(self.cursor_row + 1);
                self.lines[self.cursor_row].push_str(&next);
            }
            return;
        }
        let byte = self.cursor_byte_offset();
        let markers = self.lines[self.cursor_row][byte..]
            .chars()
            .filter(|ch| *ch == ATTACHMENT_MARKER)
            .collect::<String>();
        self.lines[self.cursor_row].truncate(byte);
        self.lines[self.cursor_row].push_str(&markers);
    }

    /// Ctrl+U — 删掉行首到光标
    pub fn kill_to_start_of_line(&mut self) {
        if self.cursor_col == 0 {
            return;
        }
        let byte = self.cursor_byte_offset();
        let markers = self.lines[self.cursor_row][..byte]
            .chars()
            .filter(|ch| *ch == ATTACHMENT_MARKER)
            .collect::<String>();
        let rest = self.lines[self.cursor_row].split_off(byte);
        self.lines[self.cursor_row] = format!("{markers}{rest}");
        self.cursor_col = markers.chars().count();
    }

    /// Delete 键 — 向前删一个字符（已在行尾则合并下一行）
    pub fn delete_char_forward(&mut self) {
        let line_len = self.lines[self.cursor_row].chars().count();
        if self.cursor_col < line_len {
            let byte = self.cursor_byte_offset();
            self.lines[self.cursor_row].remove(byte);
        } else if self.cursor_row + 1 < self.lines.len() {
            let next = self.lines.remove(self.cursor_row + 1);
            self.lines[self.cursor_row].push_str(&next);
        }
    }

    /// 取出内容并重置
    pub fn take(&mut self) -> String {
        let content = self.text();
        *self = Self::new();
        content
    }

    /// 整体替换输入框内容（历史导航整体替换时用）：先清空再复用 insert_text 的换行/字符处理
    pub fn set_text(&mut self, text: &str) {
        let anchors = self.attachment_marker_positions();
        *self = Self::new();
        self.insert_text(
            &text
                .chars()
                .filter(|ch| *ch != ATTACHMENT_MARKER)
                .collect::<String>(),
        );
        for (row, col) in anchors {
            self.insert_attachment_marker_at_text_position(row, col);
        }
        self.cursor_row = self.lines.len().saturating_sub(1);
        self.cursor_col = self.lines[self.cursor_row].chars().count();
    }

    pub fn is_empty(&self) -> bool {
        self.lines
            .iter()
            .all(|line| line.chars().all(|ch| ch == ATTACHMENT_MARKER))
    }

    /// 返回纯文字内容；内部附件原子永远不会泄漏到历史记录或 Provider 正文。
    pub(crate) fn text(&self) -> String {
        self.lines
            .iter()
            .map(|line| {
                line.chars()
                    .filter(|ch| *ch != ATTACHMENT_MARKER)
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn attachment_marker_positions(&self) -> Vec<(usize, usize)> {
        let mut positions = Vec::new();
        for (row, line) in self.lines.iter().enumerate() {
            let mut text_col = 0usize;
            for ch in line.chars() {
                if ch == ATTACHMENT_MARKER {
                    positions.push((row, text_col));
                } else {
                    text_col += 1;
                }
            }
        }
        positions
    }

    fn insert_attachment_marker_at_text_position(&mut self, row: usize, col: usize) {
        let row = row.min(self.lines.len().saturating_sub(1));
        let text_len = self.lines[row]
            .chars()
            .filter(|ch| *ch != ATTACHMENT_MARKER)
            .count();
        let target = col.min(text_len);
        let mut text_col = 0usize;
        let mut byte = self.lines[row].len();
        for (offset, ch) in self.lines[row].char_indices() {
            if ch == ATTACHMENT_MARKER {
                continue;
            }
            if text_col == target {
                byte = offset;
                break;
            }
            text_col += 1;
        }
        self.lines[row].insert(byte, ATTACHMENT_MARKER);
    }

    /// 返回光标相对于输入框的绝对显示列偏移（用于渲染光标）
    ///
    /// 注意：返回的是**终端显示列宽**（CJK 全角=2，其余=1），
    /// 不是字符索引。计算时遍历光标左侧所有字符按 `char_width` 求和。
    pub fn cursor_display_col(&self) -> usize {
        let line = &self.lines[self.cursor_row];
        line.chars().take(self.cursor_col).map(char_width).sum()
    }

    pub fn display_lines(&self) -> &[String] {
        &self.lines
    }

    /// 按显示宽度贪心切分一行为多个视觉行的字符边界（首尾含 0 和总字符数）。
    /// 遇到全角字符在当前行放不下时整体换到下一行，不会从字符中间断开——
    /// 这个规则必须与实际渲染时的折行方式完全一致，否则光标位置计算
    /// （由本函数驱动）会和屏幕上真实换行的位置产生偏差，导致光标错位、
    /// 看起来像输入内容错乱。
    fn wrap_boundaries(line: &str, width: usize) -> Vec<usize> {
        let mut bounds = vec![0usize];
        if width == 0 {
            bounds.push(line.chars().count());
            return bounds;
        }
        let mut col = 0usize;
        let mut idx = 0usize;
        for c in line.chars() {
            let w = char_width(c);
            if col + w > width && col > 0 {
                bounds.push(idx);
                col = 0;
            }
            col += w;
            idx += 1;
        }
        bounds.push(idx);
        bounds
    }

    /// 单行的视觉行数（考虑终端折行，width=0 时按 1 行算）
    fn line_visual_rows(line: &str, width: usize) -> usize {
        if width == 0 {
            return 1;
        }
        (Self::wrap_boundaries(line, width).len() - 1).max(1)
    }

    /// 所有内容折行后的总视觉行数（用于动态调整输入框高度）
    pub fn visual_height(&self, width: usize) -> usize {
        self.lines
            .iter()
            .map(|l| Self::line_visual_rows(l, width))
            .sum::<usize>()
            .max(1)
    }

    /// 任意 (row, col) 的视觉位置 (visual_row, visual_col)，考虑折行
    pub fn visual_pos_for(&self, row: usize, col: usize, width: usize) -> (usize, usize) {
        if width == 0 {
            let line = self.lines.get(row).unwrap_or(&self.lines[self.cursor_row]);
            let col = col.min(line.chars().count());
            let display_col = line.chars().take(col).map(char_width).sum();
            return (row, display_col);
        }
        let row = row.min(self.lines.len().saturating_sub(1));
        let prev_vis_rows: usize = (0..row)
            .map(|r| Self::line_visual_rows(&self.lines[r], width))
            .sum();
        let line = &self.lines[row];
        let col = col.min(line.chars().count());
        let bounds = Self::wrap_boundaries(line, width);
        let last_seg = bounds.len().saturating_sub(2);
        let mut seg = last_seg;
        for (i, end) in bounds[1..].iter().enumerate() {
            if col < *end || i == last_seg {
                seg = i;
                break;
            }
        }
        let seg_start = bounds[seg];
        let display_col: usize = line
            .chars()
            .skip(seg_start)
            .take(col.saturating_sub(seg_start))
            .map(char_width)
            .sum();
        (prev_vis_rows + seg, display_col)
    }

    /// 光标的视觉位置 (visual_row, visual_col)，考虑折行
    pub fn cursor_visual_pos(&self, width: usize) -> (usize, usize) {
        self.visual_pos_for(self.cursor_row, self.cursor_col, width)
    }

    /// 按显示宽度手动折行，供渲染使用：必须与 [`cursor_visual_pos`] 用同一套算法，
    /// 不依赖 ratatui 内置 `Wrap`（其按空格分词，对无空格的中日韩文本不适用，
    /// 且与本文件独立的宽度计算容易产生偏差）。
    pub fn wrap_for_render(line: &str, width: usize) -> Vec<String> {
        if width == 0 {
            return vec![line.to_string()];
        }
        let bounds = Self::wrap_boundaries(line, width);
        let chars: Vec<char> = line.chars().collect();
        (0..bounds.len() - 1)
            .map(|i| chars[bounds[i]..bounds[i + 1]].iter().collect())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 回归测试：99 列终端（内容区宽度 97，奇数）下输入 60 个全角字符，
    /// 光标应落在渲染出的折行文字之后，而不是叠在最后一两个字符上——
    /// 此前 cursor_visual_pos 用 `disp_col / width` 朴素除法计算折行，
    /// 假设每一视觉行都恰好塞满 width 列，但全角字符在奇数宽度下每行会
    /// 留一列空白，实测 tmux 复现出光标偏左 2 列、叠在文字上的错位。
    #[test]
    fn cursor_visual_pos_matches_rendered_wrap_for_wide_chars() {
        let mut ib = InputBox::new();
        for _ in 0..30 {
            ib.insert_char('测');
            ib.insert_char('试');
        }
        let width = 97;
        let rendered = InputBox::wrap_for_render(&ib.lines[0], width);
        let (vis_row, vis_col) = ib.cursor_visual_pos(width);

        assert_eq!(vis_row, rendered.len() - 1, "光标应在最后一个渲染行上");
        let last_line_width: usize = rendered[vis_row].chars().map(char_width).sum();
        assert_eq!(
            vis_col, last_line_width,
            "光标列应正好在最后渲染行内容之后，而非叠在文字中间"
        );
    }

    #[test]
    fn wrap_for_render_never_splits_wide_char_across_lines() {
        let line: String = "测试".repeat(30);
        let segs = InputBox::wrap_for_render(&line, 97);
        for seg in &segs {
            let w: usize = seg.chars().map(char_width).sum();
            assert!(w <= 97, "每个视觉行不应超过可用宽度");
        }
        let rejoined: String = segs.concat();
        assert_eq!(rejoined, line, "折行不应丢字符或产生乱码");
    }

    #[test]
    fn line_visual_rows_matches_wrap_for_render_len() {
        let line: String = "a测b试c".repeat(10);
        let width = 13;
        assert_eq!(
            InputBox::line_visual_rows(&line, width),
            InputBox::wrap_for_render(&line, width).len()
        );
    }
}
