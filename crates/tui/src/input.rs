//! 多行输入框状态

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// 结构化附件在编辑器里的内部原子标记。
///
/// 它不会直接渲染，也不会进入发送给模型的正文；渲染层会按附件类型把它替换成
/// [Image #n] / [File: ...]。把标记放进真实编辑序列，才能让占位符跟随粘贴时
/// 的光标位置，并自然参与左右移动、换行和普通文字插入。
pub(crate) const ATTACHMENT_MARKER: char = '\u{fffc}';

/// 被折叠的长文本粘贴在编辑器里的内部原子标记。
///
/// 语义与 [`ATTACHMENT_MARKER`] 完全平行：一个标记 ↔ `pending_pastes` 里的
/// 一段原文，参与左右移动、换行、Ctrl+K/U 的标记保全，并随粘贴位置停在原处。
/// 与附件的差别只在**展开方向**：附件展开成 `[Image #1]` 这种只读占位符就结束
/// （内容走结构化通道），粘贴块必须在**发送时**把原文填回正文——否则模型收到
/// 的就只有一句 `[Pasted text #1]`，等于把用户贴的东西吞了。
///
/// 取 Unicode 私用区 U+E0000：用户实际粘贴的文本几乎不可能命中，
/// 而 `text()` 之后还要按这个字符做过滤，撞上一个真实字符就会静默吃掉内容。
pub(crate) const PASTE_MARKER: char = '\u{e0000}';

/// 这两个标记都必须被 `text()` 过滤掉，且在所有编辑原语里同等地当作
/// 「一个不可分割的原子」。写漏任何一处，占位符就会在正文里漏出控制字符，
/// 或者只能被删掉半个（`[Pasted text #1]` 被退格成 `asted text #1`）。
#[inline]
pub(crate) fn is_atomic_marker(ch: char) -> bool {
    ch == ATTACHMENT_MARKER || ch == PASTE_MARKER
}

/// 保留标记的字符过滤器：Ctrl+K / Ctrl+U 会把整段正文删掉，但标记必须活着，
/// 否则删一次行尾就把已经粘贴进来的附件/长文本从编辑序列里弄丢了。
fn keep_markers_only(ch: char) -> bool {
    is_atomic_marker(ch)
}

/// 光标前一个字符是不是反斜杠 —— Claude Code 用的第三种换行写法
///（行尾打一个 `\` 再按 Enter）。在不支持 Shift+Enter 的终端里，这是唯一
/// 不依赖键盘协议的换行方式。
pub fn trailing_backslash_before_cursor(input: &InputBox) -> bool {
    let row = input.cursor_row;
    let col = input.cursor_col;
    let Some(line) = input.lines.get(row) else {
        return false;
    };
    let chars: Vec<char> = line.chars().collect();
    col.checked_sub(1)
        .and_then(|i| chars.get(i))
        .is_some_and(|c| *c == '\\')
}

/// 纯文本编辑按键的**统一实现**：主输入框与所有弹窗的自由文本输入共用。
///
/// 之前这两套按键表是各写各的，弹窗只有 Char / Backspace / Left / Right，
/// 连 Delete、Home、End、Ctrl+U/A/E/K/W、Alt+←→、Shift+Enter 都没有——同一个
/// 输入动作在主输入框能按、在弹窗里按不动，是「交互反人类」的主要来源。
/// 既然 [`InputBox`] 的原语（`move_word_*` / `kill_*_of_line` /
/// `delete_word_backward` 等）本来就齐全，这里只做**接线**，不重复造原语。
///
/// 返回 `true` 表示该按键已被当作编辑操作消费，调用方不要再走自己的分支。
///
/// **刻意不含** `Enter` / `Esc` / `Tab` / `↑` `↓` / 粘贴：这几类各有应用层
/// 语义（提交、取消、翻页、历史、附件），由调用方自己分发。唯一例外是
/// `Shift+Enter` —— 它在所有输入场景里都只表示"换行"，没有别的含义。
pub fn apply_edit_key(input: &mut InputBox, key: &KeyEvent) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Backspace => {
            if alt {
                input.delete_word_backward();
            } else {
                input.backspace();
            }
            true
        }
        KeyCode::Delete => {
            input.delete_char_forward();
            true
        }
        KeyCode::Left => {
            if ctrl || alt {
                input.move_word_backward();
            } else {
                input.move_left();
            }
            true
        }
        KeyCode::Right => {
            if ctrl || alt {
                input.move_word_forward();
            } else {
                input.move_right();
            }
            true
        }
        KeyCode::Home => {
            input.move_to_start_of_line();
            true
        }
        KeyCode::End => {
            input.move_to_end_of_line();
            true
        }
        // 换行的三种等价写法。Shift+Enter 只有终端上报 modifyOtherKeys /
        // kitty 键盘协议时才区分得出来（Ghostty/Kitty/iTerm2/WezTerm 可以，
        // tmux + 默认设置不行，会原样报成普通 Enter 直接提交），所以必须给出
        // 到处都能用的兜底 —— 这也是 Claude Code 文档里推荐的三种写法。
        KeyCode::Enter
            if key
                .modifiers
                .intersects(KeyModifiers::SHIFT | KeyModifiers::CONTROL) =>
        {
            input.insert_newline();
            true
        }
        // Ctrl+J 有时被终端报成 Char('j')+CONTROL
        KeyCode::Char('j' | 'J') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            input.insert_newline();
            true
        }
        KeyCode::Char(c) if ctrl => match c {
            // readline 传统别名，和 Claude Code 文档里的键表一致
            'b' | 'B' => {
                input.move_left();
                true
            }
            'f' | 'F' => {
                input.move_right();
                true
            }
            'h' | 'H' => {
                input.backspace();
                true
            }
            'd' | 'D' => {
                input.delete_char_forward();
                true
            }
            'a' | 'A' => {
                input.move_to_start_of_line();
                true
            }
            'e' | 'E' => {
                input.move_to_end_of_line();
                true
            }
            'k' | 'K' => {
                input.kill_to_end_of_line();
                true
            }
            'u' | 'U' => {
                input.kill_to_start_of_line();
                true
            }
            'w' | 'W' => {
                input.delete_word_backward();
                true
            }
            _ => false,
        },
        _ => false,
    }
}

/// 该按键是不是「往输入框里敲一个字」。
///
/// 唯一的判据是**修饰位**：带 Ctrl / Alt / Cmd(`SUPER`) / `META` / `HYPER`
/// 的字符键都有应用层含义或属于系统快捷键（`Cmd+C` 复制、`Cmd+V` 粘贴、
/// `Alt+←` 按词移动…），一律不许当正文插入。`SHIFT` 刻意不在此列——它已经
/// 体现在 `KeyCode` 里（Shift+a 上报的就是 `Char('A')`），排除它会让大写
/// 字母全打不进去。
///
/// 缺了这层判定的后果是实打实的：macOS 上选中内容区后按 `Cmd+C`，Kitty
/// 键盘协议把它报成 `Char('c') + SUPER`（见 `app::enter_terminal_screen`
/// 压的 `DISAMBIGUATE_ESCAPE_CODES`），主循环只认 `CONTROL`，于是 `c` 被
/// 原样插进输入框，看上去像「复制把 c 打进去了」。
pub fn plain_char(key: &KeyEvent) -> Option<char> {
    if key.modifiers.intersects(
        KeyModifiers::CONTROL
            | KeyModifiers::ALT
            | KeyModifiers::SUPER
            | KeyModifiers::META
            | KeyModifiers::HYPER,
    ) {
        return None;
    }
    match key.code {
        KeyCode::Char(c) => Some(c),
        _ => None,
    }
}

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

    /// 在当前光标处插入一个「已折叠的长文本粘贴」原子。
    pub(crate) fn insert_paste_marker(&mut self) {
        self.insert_char(PASTE_MARKER);
    }

    /// 输入框内当前有多少个附件原子标记。**每个标记对应 `pending_attachments`
    /// 里的前 N 项**（粘贴路径保证一一对应，`reconcile_attachment_markers` 负责
    /// 修复旧状态漂移），因此 `attachments[N..]` 就是「没有占位符、只能在消息
    /// 末尾单列」的那些。
    pub(crate) fn attachment_marker_count(&self) -> usize {
        self.count_markers(ATTACHMENT_MARKER)
    }

    fn count_markers(&self, marker: char) -> usize {
        self.lines
            .iter()
            .map(|line| line.chars().filter(|ch| *ch == marker).count())
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
        self.marker_ordinal_at(row, col, ATTACHMENT_MARKER)
    }

    /// 某个原子标记在整段编辑序列里的序号（0 起），非标记处返回 `None`。
    /// 附件与粘贴块各自从 0 独立编号，各自对应自己那个 Vec 的下标。
    fn marker_ordinal_at(&self, row: usize, col: usize, marker: char) -> Option<usize> {
        if self.lines.get(row)?.chars().nth(col)? != marker {
            return None;
        }

        Some(
            self.lines[..row]
                .iter()
                .map(|line| line.chars().filter(|ch| *ch == marker).count())
                .sum::<usize>()
                + self.lines[row]
                    .chars()
                    .take(col)
                    .filter(|ch| *ch == marker)
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

    pub(crate) fn paste_marker_before_cursor(&self) -> Option<usize> {
        (self.cursor_col > 0)
            .then(|| self.marker_ordinal_at(self.cursor_row, self.cursor_col - 1, PASTE_MARKER))
            .flatten()
    }

    pub(crate) fn paste_marker_at_cursor(&self) -> Option<usize> {
        self.marker_ordinal_at(self.cursor_row, self.cursor_col, PASTE_MARKER)
    }

    /// 新附件应插入结构化附件向量的位置。顺序按编辑器中的可视位置计算，而不是
    /// 一律追加到末尾，保证用户把光标移到前文后再粘贴时，标记、编号、删除和发送
    /// 顺序仍指向同一个附件。
    pub(crate) fn attachment_insertion_index(&self) -> usize {
        self.marker_insertion_index(ATTACHMENT_MARKER)
    }

    /// 新粘贴块应插入 `pending_pastes` 的位置，与 [`Self::attachment_insertion_index`]
    /// 同理：按编辑序列里的真实位置算，发送时的展开顺序才不会和屏幕上看到的不一致。
    pub(crate) fn paste_insertion_index(&self) -> usize {
        self.marker_insertion_index(PASTE_MARKER)
    }

    fn marker_insertion_index(&self, marker: char) -> usize {
        self.lines[..self.cursor_row]
            .iter()
            .map(|line| line.chars().filter(|ch| *ch == marker).count())
            .sum::<usize>()
            + self.lines[self.cursor_row]
                .chars()
                .take(self.cursor_col)
                .filter(|ch| *ch == marker)
                .count()
    }

    fn remove_attachment_marker(&mut self, ordinal: usize) -> bool {
        self.remove_marker(ordinal, ATTACHMENT_MARKER)
    }

    /// 按序号删掉一个原子标记，并把落在它右侧的同标记光标列号左移一格。
    fn remove_marker(&mut self, ordinal: usize, marker: char) -> bool {
        let mut seen = 0usize;
        for row in 0..self.lines.len() {
            let marker_col = self.lines[row].chars().enumerate().find_map(|(col, ch)| {
                if ch != marker {
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
        while pos > 0 && !is_atomic_marker(chars[pos - 1]) && !chars[pos - 1].is_alphanumeric() {
            pos -= 1;
        }
        if pos > 0 && is_atomic_marker(chars[pos - 1]) {
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
        while pos < chars.len() && !is_atomic_marker(chars[pos]) && !chars[pos].is_alphanumeric() {
            pos += 1;
        }
        if pos < chars.len() && is_atomic_marker(chars[pos]) {
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
            .filter(|ch| keep_markers_only(*ch))
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
            .filter(|ch| keep_markers_only(*ch))
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
        let anchors = self.atom_marker_positions();
        *self = Self::new();
        self.insert_text(
            &text
                .chars()
                .filter(|ch| !is_atomic_marker(*ch))
                .collect::<String>(),
        );
        for (row, col, marker) in anchors {
            self.insert_marker_at_text_position(row, col, marker);
        }
        self.cursor_row = self.lines.len().saturating_sub(1);
        self.cursor_col = self.lines[self.cursor_row].chars().count();
    }

    pub fn is_empty(&self) -> bool {
        self.lines
            .iter()
            .all(|line| line.chars().all(is_atomic_marker))
    }

    /// 返回纯文字内容；内部原子标记永远不会泄漏到历史记录或 Provider 正文。
    ///
    /// **折叠的粘贴块在这一步也只会消失、不在这里展开**——`text()` 拿不到
    /// `pending_pastes`。凡是发往模型或存进历史的正文都必须走
    /// [`Self::text_with_pastes`]；误用这里的后果是用户贴的三百行日志变成空消息。
    pub(crate) fn text(&self) -> String {
        self.lines
            .iter()
            .map(|line| {
                line.chars()
                    .filter(|ch| !is_atomic_marker(*ch))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 返回发往模型的正文：折叠的粘贴块按其在编辑序列里的位置展开回**原文**。
    ///
    /// 编号从 0 起，与 `pending_pastes` 的下标一一对应。展开是纯文本拼接，
    /// 不引入任何占位符残留——模型必须看到用户真正贴进来的内容。数量对不上
    /// （理论上不会发生，`apply_clipboard_paste` 一次插一个标记）时多余的标记
    /// 仍然按 `text()` 的规则被丢掉，而不是漏成控制字符。
    pub(crate) fn text_with_pastes(&self, pastes: &[crate::app::PasteBlock]) -> String {
        let mut out = String::new();
        let mut idx = 0usize;
        for (row, line) in self.lines.iter().enumerate() {
            if row > 0 {
                out.push('\n');
            }
            for ch in line.chars() {
                match ch {
                    ATTACHMENT_MARKER => {}
                    PASTE_MARKER => {
                        if let Some(block) = pastes.get(idx) {
                            out.push_str(&block.text);
                        }
                        idx += 1;
                    }
                    c => out.push(c),
                }
            }
        }
        out
    }

    /// 记录全部原子标记在**纯文字**坐标系下的位置与类型，供 `set_text`
    /// 重建——位置记的是字符序号而不是字节偏移，重建时才不必再看旧字符串。
    fn atom_marker_positions(&self) -> Vec<(usize, usize, char)> {
        let mut positions = Vec::new();
        for (row, line) in self.lines.iter().enumerate() {
            let mut text_col = 0usize;
            for ch in line.chars() {
                if is_atomic_marker(ch) {
                    positions.push((row, text_col, ch));
                } else {
                    text_col += 1;
                }
            }
        }
        positions
    }

    fn insert_marker_at_text_position(&mut self, row: usize, col: usize, marker: char) {
        let row = row.min(self.lines.len().saturating_sub(1));
        let text_len = self.lines[row]
            .chars()
            .filter(|ch| !is_atomic_marker(*ch))
            .count();
        let target = col.min(text_len);
        let mut text_col = 0usize;
        let mut byte = self.lines[row].len();
        for (offset, ch) in self.lines[row].char_indices() {
            if is_atomic_marker(ch) {
                continue;
            }
            if text_col == target {
                byte = offset;
                break;
            }
            text_col += 1;
        }
        self.lines[row].insert(byte, marker);
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

    /// 光标是否停在「本行恰好填满可视宽度」的行尾。
    ///
    /// 终端把一行写满最后一列后，下一个插入点已经落在**下一行首列**；此时
    /// 光标所在的视觉行与文字所在的视觉行不是同一行。若仍按「最后一段的
    /// 显示宽度」给列坐标，它就是 `width`——超出可绘制范围，渲染层只能 `min`
    /// 回最后一格，光标叠在最后一个字符上，用户以为「打了字但没换行」。
    ///
    /// 判据三条缺一不可：宽度非零、光标在本行行尾、本行显示宽度恰好等于
    /// `width`。少最后一列或光标在行中时本来就有空位可放，不成立。
    pub fn cursor_fills_line(&self, width: usize) -> bool {
        if width == 0 {
            return false;
        }
        let line = &self.lines[self.cursor_row];
        self.cursor_col == line.chars().count()
            && line.chars().map(char_width).sum::<usize>() == width
    }

    /// [`Self::visual_height`] 加上光标**自己**占的那一格。
    ///
    /// 只有光标那一行落在所有文本之后时才多出一行（最后一条内容行被填满）；
    /// 上文还有别的逻辑行时，那一行本来就已经算在总高度里。渲染高度、视口
    /// 总行数、滚轮滚动上限都必须用这个口径，否则「文字占满一行」时算出的
    /// 总量比真实绘制少一行，光标被钳回上一行。
    pub fn visual_height_with_cursor(&self, width: usize) -> usize {
        let (row, _) = self.cursor_visual_pos(width);
        self.visual_height(width).max(row + 1)
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

    /// 光标的视觉位置 (visual_row, visual_col)，考虑折行。
    ///
    /// 与 [`Self::visual_pos_for`] 的唯一差别是**行尾填满**的处理：文字正好
    /// 占满最后一列时，终端把插入点换到下一行首列，而不是停在最后一格。这里
    /// 让出那一行（`+1, 0`），渲染层才有一格可放光标；否则 `draw_input` 只能
    /// 把列坐标钳回 `width - 1`，光标叠在最后一个字符上，看起来像「打了字但
    /// 没换行」。判据集中在 [`Self::cursor_fills_line`]。
    pub fn cursor_visual_pos(&self, width: usize) -> (usize, usize) {
        let (row, col) = self.visual_pos_for(self.cursor_row, self.cursor_col, width);
        if self.cursor_fills_line(width) {
            (row + 1, 0)
        } else {
            (row, col)
        }
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

    /// [`Self::visual_pos_for`] 的**反函数**：视觉坐标 → 逻辑 `(row, col)`。
    ///
    /// 鼠标点选输入框时用它把屏幕格子换算成编辑位置。`visual_row` 超出内容末尾
    /// 时夹到最后一个字符之后；`visual_col` 落在某折行段的中间时取最近的字符
    /// 边界（宽字符占两列，点右半格仍算命中该字符之前/之后由取整方向决定）。
    ///
    /// 注意 `col` 是**字符索引**，不是显示列——与 `cursor_col` 同一套语义。
    pub fn text_pos_for_visual(
        &self,
        visual_row: usize,
        visual_col: usize,
        width: usize,
    ) -> (usize, usize) {
        if width == 0 {
            let row = visual_row.min(self.lines.len().saturating_sub(1));
            let col = visual_col.min(self.lines[row].chars().count());
            return (row, col);
        }
        let mut remaining = visual_row;
        for (row, line) in self.lines.iter().enumerate() {
            let rows = Self::line_visual_rows(line, width);
            if remaining >= rows {
                remaining -= rows;
                continue;
            }
            let bounds = Self::wrap_boundaries(line, width);
            // 第 `remaining` 个视觉行覆盖 `bounds[remaining]..bounds[remaining+1]`。
            let seg = remaining.min(bounds.len().saturating_sub(2));
            let seg_start = bounds[seg];
            let seg_end = bounds[seg + 1];
            let chars: Vec<char> = line.chars().collect();
            let mut display_col = 0usize;
            let mut col = seg_start;
            while col < seg_end {
                let w = char_width(chars[col]);
                // 点在该字符的左半边或更左：停在它前面。
                if display_col + w > visual_col {
                    break;
                }
                display_col += w;
                col += 1;
            }
            return (row, col);
        }
        // 落在所有内容之下：定位到最后一个字符之后。
        let row = self.lines.len().saturating_sub(1);
        (
            row,
            self.lines.get(row).map(|l| l.chars().count()).unwrap_or(0),
        )
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

    /// 文字正好填满整行时，光标必须换到下一行首列，而不是叠在最后一个字符上。
    ///
    /// 现场：在一行末尾继续打字，内容刚好占满可视宽度，此时占位符/光标停住
    /// 不动，用户以为输入丢了。根因是 `wrap_for_render` 贪心切分在「正好填满」
    /// 时不产生新段落，`visual_pos_for` 于是给出 `display_col == width`——一个
    /// 画不出来的列，渲染层只能把它钳回 `width - 1`。
    #[test]
    fn cursor_moves_to_the_next_row_when_it_fills_the_line() {
        for (text, width) in [("a".repeat(10), 10), ("测".repeat(5), 10)] {
            let mut ib = InputBox::new();
            ib.insert_text(&text);
            assert!(
                ib.cursor_fills_line(width),
                "{text:?} 应被判为填满第 {width} 列"
            );
            assert_eq!(
                ib.cursor_visual_pos(width),
                (1, 0),
                "填满一行后光标应在下一行首列"
            );
            assert_eq!(ib.visual_height_with_cursor(width), 2, "高度必须多留一行");
        }
    }

    /// 少一列、或光标不在行尾时，都不能多占一行——否则每行下面都会凭空多出
    /// 一条空行，输入框高度来回跳。
    #[test]
    fn cursor_stays_on_its_own_row_when_the_line_is_not_full() {
        let mut ib = InputBox::new();
        ib.insert_text(&"a".repeat(9));
        assert_eq!(ib.cursor_visual_pos(10), (0, 9));
        assert_eq!(ib.visual_height_with_cursor(10), 1);

        let mut ib = InputBox::new();
        ib.insert_text(&"a".repeat(10));
        ib.move_to_start_of_line();
        assert!(!ib.cursor_fills_line(10), "光标不在行尾时不算填满");
        assert_eq!(ib.cursor_visual_pos(10), (0, 0));
        assert_eq!(ib.visual_height_with_cursor(10), 1);
    }

    /// 填满的那一行**不是最后一行**时，多出来的那一行本来就属于下一条逻辑行，
    /// 不能让整体高度再加一。
    #[test]
    fn a_full_line_above_the_last_one_does_not_add_height() {
        let mut ib = InputBox::new();
        ib.insert_text(&"a".repeat(10));
        ib.insert_newline();
        assert_eq!(ib.visual_height_with_cursor(10), 2, "两行文本 = 两行高度");
    }

    /// 鼠标点在新位置必须还能反查到「行尾」，来回换算不能漂。
    #[test]
    fn the_wrapped_cursor_position_still_maps_back_to_the_line_end() {
        let mut ib = InputBox::new();
        ib.insert_text(&"a".repeat(10));
        let (vr, vc) = ib.cursor_visual_pos(10);
        assert_eq!(ib.text_pos_for_visual(vr, vc, 10), (0, 10));
    }

    /// `text_pos_for_visual` 必须是 `visual_pos_for` 的反函数。
    ///
    /// 鼠标点选输入框全靠它把屏幕格子换算成编辑位置；算错一格，光标就落到
    /// 用户没点的地方。逐字符来回换算一遍即可覆盖折行、宽字符与行尾。
    #[test]
    fn text_pos_for_visual_inverts_visual_pos_for() {
        let mut ib = InputBox::new();
        for line in ["hello world", "测试中文换行", "a", ""] {
            if !ib.is_empty() || ib.lines.len() > 1 {
                ib.insert_newline();
            }
            for ch in line.chars() {
                ib.insert_char(ch);
            }
        }
        let width = 7;
        for row in 0..ib.lines.len() {
            let len = ib.lines[row].chars().count();
            for col in 0..=len {
                let (vr, vc) = ib.visual_pos_for(row, col, width);
                assert_eq!(
                    ib.text_pos_for_visual(vr, vc, width),
                    (row, col),
                    "第 {row} 行第 {col} 列往返换算不一致"
                );
            }
        }
    }

    /// 点在宽字符的右半格仍归到该字符之前，不会跳到下一个字符。
    #[test]
    fn text_pos_for_visual_keeps_wide_chars_whole() {
        let mut ib = InputBox::new();
        ib.insert_char('中');
        ib.insert_char('文');
        assert_eq!(ib.text_pos_for_visual(0, 0, 20), (0, 0));
        // 列 1 是「中」的右半格 → 仍停在「中」之前。
        assert_eq!(ib.text_pos_for_visual(0, 1, 20), (0, 0));
        assert_eq!(ib.text_pos_for_visual(0, 2, 20), (0, 1));
        // 超出内容末尾：夹到最后一个字符之后。
        assert_eq!(ib.text_pos_for_visual(9, 99, 20), (0, 2));
    }

    /// `Cmd+C` 绝不能是「往输入框敲一个 c」。
    ///
    /// 现场：鼠标选中内容区后按 Command+C，输入框里凭空多出一个 `c`。macOS 的
    /// Kitty 键盘协议把它报成 `Char('c') + SUPER`（`app::enter_terminal_screen`
    /// 压的 `DISAMBIGUATE_ESCAPE_CODES`），而主循环只过滤了 `CONTROL`。
    #[test]
    fn modified_char_keys_are_never_plain_text() {
        assert_eq!(
            plain_char(&KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)),
            Some('c')
        );
        // Shift 已体现在 KeyCode 里，不能排除，否则大写字母全打不进去
        assert_eq!(
            plain_char(&KeyEvent::new(KeyCode::Char('C'), KeyModifiers::SHIFT)),
            Some('C')
        );
        for modifiers in [
            KeyModifiers::CONTROL,
            KeyModifiers::ALT,
            KeyModifiers::SUPER, // macOS 的 Cmd
            KeyModifiers::META,
            KeyModifiers::HYPER,
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            KeyModifiers::SUPER | KeyModifiers::SHIFT,
        ] {
            assert_eq!(
                plain_char(&KeyEvent::new(KeyCode::Char('c'), modifiers)),
                None,
                "{modifiers:?} + c 是快捷键，不是正文"
            );
        }
        // 非字符键一律不是正文
        assert_eq!(
            plain_char(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            None
        );
    }
}
