//! 多行输入框状态

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

#[derive(Default)]
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
        while pos > 0 && !chars[pos - 1].is_alphanumeric() {
            pos -= 1;
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
        while pos < chars.len() && !chars[pos].is_alphanumeric() {
            pos += 1;
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
        self.lines[self.cursor_row].truncate(byte);
    }

    /// Ctrl+U — 删掉行首到光标
    pub fn kill_to_start_of_line(&mut self) {
        if self.cursor_col == 0 {
            return;
        }
        let byte = self.cursor_byte_offset();
        let rest = self.lines[self.cursor_row].split_off(byte);
        self.lines[self.cursor_row] = rest;
        self.cursor_col = 0;
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
        let content = self.lines.join("\n");
        *self = Self::new();
        content
    }

    pub fn is_empty(&self) -> bool {
        self.lines.iter().all(|l| l.is_empty())
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

    /// 单行的视觉行数（考虑终端折行，width=0 时按 1 行算）
    fn line_visual_rows(line: &str, width: usize) -> usize {
        if width == 0 {
            return 1;
        }
        let disp: usize = line.chars().map(char_width).sum();
        if disp == 0 {
            1
        } else {
            (disp + width - 1) / width
        }
    }

    /// 所有内容折行后的总视觉行数（用于动态调整输入框高度）
    pub fn visual_height(&self, width: usize) -> usize {
        self.lines
            .iter()
            .map(|l| Self::line_visual_rows(l, width))
            .sum::<usize>()
            .max(1)
    }

    /// 光标的视觉位置 (visual_row, visual_col)，考虑折行
    pub fn cursor_visual_pos(&self, width: usize) -> (usize, usize) {
        if width == 0 {
            return (self.cursor_row, self.cursor_display_col());
        }
        let prev_vis_rows: usize = (0..self.cursor_row)
            .map(|r| Self::line_visual_rows(&self.lines[r], width))
            .sum();
        let disp_col = self.cursor_display_col();
        (prev_vis_rows + disp_col / width, disp_col % width)
    }
}
