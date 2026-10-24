//! 剪贴板、附件、输入框文本操作。
//!
//! 从 `app.rs` 拆出。

use super::*;

// ── 附件辅助函数 ──────────────────────────────────────────────────────────────

/// 将 arboard 返回的 RGBA 字节编码为 PNG 字节序列
pub(super) fn encode_rgba_to_png(bytes: &[u8], width: u32, height: u32) -> anyhow::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut encoder = png::Encoder::new(&mut buf, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(bytes)?;
    drop(writer);
    Ok(buf)
}

#[derive(Debug)]
pub(super) enum ClipboardPaste {
    Image {
        data: String,
        width: usize,
        height: usize,
    },
    /// 剪贴板里是文件引用（macOS Finder 复制的文件、Windows 资源管理器
    /// 复制的文件）。必须**优先于** [`ClipboardPaste::Image`] 探测：见
    /// [`read_system_clipboard`]。
    Files(Vec<PathBuf>),
    Text(String),
}

/// 直接读取系统剪贴板。
///
/// 终端的 bracketed paste 只会为文本产生 `Event::Paste`；纯图片剪贴板不会触发
/// 该事件，因此 Ctrl+V 必须由应用主动读取剪贴板。
///
/// **探测顺序 = 文件 → 文本 → 图片**，顺序本身就是这个函数唯一重要的部分：
///
/// macOS 从 Finder 复制一个文件时，NSPasteboard 同时写 `public.file-url`（真正的
/// 文件引用）和 `public.tiff`（文件图标的 Quick Look 预览）。arboard 的
/// `get_image()` 在 macOS 读的恰好就是 `NSPasteboardTypeTIFF`
/// （arboard-3.6.1 `src/platform/osx.rs`）。旧实现把 `get_image()` 放在最前面且
/// 成功即 return，于是**任何**从 Finder 复制的文件都会先命中图标预览、走
/// `Attachment::Image` 分支，输入框里显示成 `[Image #1 16×16]`，而本该走的
/// `try_resolve_path` → `[File: foo.pdf]` 一次都没机会执行。
///
/// 文本排在图片前面同样安全：截图工具、浏览器「复制图片」都只写图像 flavor，
/// 不写 `public.utf8-plain-text`；而不少应用复制图片时会附带 alt 文本，那段文字
/// 才是用户真正想说的。旧注释写的「兼容同时暴露图片与文本 flavor 的剪贴板」
/// 对**文件**恰好是反的。
pub(super) fn read_system_clipboard() -> Option<ClipboardPaste> {
    let mut clipboard = arboard::Clipboard::new().ok()?;

    // 1) 文件引用：arboard 自带 `file_list()`，macOS 侧读 NSURL 并带
    //    `NSURLURLReadingFileURLsOnlyKey`。此前从未接线，能力在依赖里躺着。
    if let Ok(files) = clipboard.get().file_list() {
        if !files.is_empty() {
            return Some(ClipboardPaste::Files(files));
        }
    }

    // 2) 纯文本
    if let Ok(text) = clipboard.get_text() {
        if !text.is_empty() {
            return Some(ClipboardPaste::Text(text));
        }
    }

    // 3) 图片（剪贴板里只有像素时）
    let image = clipboard.get_image().ok()?;
    let png_bytes =
        encode_rgba_to_png(&image.bytes, image.width as u32, image.height as u32).ok()?;
    use base64::Engine as _;
    Some(ClipboardPaste::Image {
        data: base64::engine::general_purpose::STANDARD.encode(png_bytes),
        width: image.width,
        height: image.height,
    })
}

/// bracketed paste 已经携带可靠的文本内容；这里只额外探测同一剪贴板里是否还有
/// **文件引用**（同样只认 `file_list()`，不去碰 `get_image()`——文本已经拿到了，
/// 剩下的 flavor 不该反过来把它顶掉）。
pub(super) fn read_clipboard_files() -> Option<ClipboardPaste> {
    let mut clipboard = arboard::Clipboard::new().ok()?;
    let files = clipboard.get().file_list().ok()?;
    (!files.is_empty()).then_some(ClipboardPaste::Files(files))
}

pub(super) fn read_clipboard_text() -> Option<String> {
    // 测试期可注入：直接读真实剪贴板的测试在开发机上会随机失败（CI 空剪贴板
    // 过、本机有内容就红），这种「本地红 CI 绿」的测试比稳定失败更糟——没人会
    // 再信任它。见 `with_clipboard_text_override`。
    #[cfg(test)]
    if let Some(forced) = TEST_CLIPBOARD_TEXT.with(|slot| slot.borrow().clone()) {
        return forced;
    }
    let mut clipboard = arboard::Clipboard::new().ok()?;
    clipboard.get_text().ok().filter(|text| !text.is_empty())
}

// `None` = 未注入（读真实剪贴板）；`Some(x)` = 钉死为 `x`（`Some(None)`
// 即"剪贴板为空"）。外层 Option 用来区分这两者。
#[cfg(test)]
thread_local! {
    static TEST_CLIPBOARD_TEXT: std::cell::RefCell<Option<Option<String>>> =
        const { std::cell::RefCell::new(None) };
}

/// 在 `f` 执行期间把剪贴板内容钉死为 `text`（`None` 表示剪贴板为空）。
#[cfg(test)]
pub(super) fn with_clipboard_text_override<R>(text: Option<&str>, f: impl FnOnce() -> R) -> R {
    TEST_CLIPBOARD_TEXT.with(|slot| *slot.borrow_mut() = Some(text.map(str::to_owned)));
    let out = f();
    TEST_CLIPBOARD_TEXT.with(|slot| *slot.borrow_mut() = None);
    out
}

pub(super) fn is_clipboard_paste_key(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('v' | 'V'))
        && (key.modifiers.contains(KeyModifiers::CONTROL)
            || key.modifiers.contains(KeyModifiers::SUPER))
}

/// 应用内选区的复制键：`Cmd+C`（macOS）或 `Ctrl+Shift+C`（其余平台终端惯例）。
///
/// **刻意不含裸 `Ctrl+C`**——它在本程序里是「中断回合 / 清空草稿 / 二次确认
/// 退出」，抢过来会让长回合无法中断。
///
/// 这个判定必须排在主循环 `state.clear_selection()` **之前**：选区正是要复制的
/// 东西，晚一步就只剩一个空选区。
pub(super) fn is_clipboard_copy_key(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('c' | 'C'))
        && (key.modifiers.contains(KeyModifiers::SUPER)
            || (key.modifiers.contains(KeyModifiers::CONTROL)
                && key.modifiers.contains(KeyModifiers::SHIFT)))
}

/// 从输入框进入内容区的快捷键：`Shift+↑`。
///
/// 用 `contains(SHIFT)` + 排除其余修饰键，而不是 `modifiers == SHIFT`：部分
/// 终端（macOS Terminal.app 在开启鼠标报告后、iTerm2 的某些配置）会把 Shift
/// 与 Caps Lock/Num Lock 之类的锁定位一起报上来，严格相等会把这类事件漏掉，
/// 表现为「按了没反应」。带 Ctrl/Alt/Cmd 的组合仍不触发，免得抢系统快捷键。
pub(super) fn is_content_focus_key(key: KeyEvent) -> bool {
    key.code == KeyCode::Up
        && key.modifiers.contains(KeyModifiers::SHIFT)
        && !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
}

/// 单行文字直接留在输入框即可；只有真正包含换行的批量粘贴才需要瞬时提示。
pub(super) fn pasted_text_needs_hint(text: &str) -> bool {
    text.contains('\n')
}

/// 判断粘贴文本是否像文件路径（绝对路径或以 ~/ ./ 开头，单行）
pub(super) fn looks_like_file_path(s: &str) -> bool {
    let s = s.trim();
    !s.contains('\n') && (s.starts_with('/') || s.starts_with("~/") || s.starts_with("./"))
}

/// 展开 ~/ 前缀并检查文件是否存在，仅当是普通文件时返回 Some
pub(super) fn try_resolve_path(s: &str) -> Option<std::path::PathBuf> {
    let s = s.trim();
    if !looks_like_file_path(s) {
        return None;
    }
    let path = if let Some(rest) = s.strip_prefix("~/") {
        let home = std::env::var("HOME").ok()?;
        std::path::PathBuf::from(format!("{home}/{rest}"))
    } else {
        std::path::PathBuf::from(s)
    };
    if path.exists() && path.is_file() {
        Some(path)
    } else {
        None
    }
}

/// 粘贴后在输入框光标处显示的瞬时提示
#[derive(Debug, Clone)]
pub(crate) struct PasteHint {
    pub(crate) text: String,
    pub(crate) expires_at: Instant,
    pub(crate) cursor_row: usize,
    pub(crate) cursor_col: usize,
}

/// 选区所在的区域。
///
/// 拖选只在**起手区域**内进行：聊天区与输入框的文本来源不同（一个是渲染后的
/// 屏幕快照，一个是输入框的折行结果），允许跨区拖选就得回答「拼接处算不算
/// 输入框边框」这类没有一致答案的问题。限制在起手区域，复制出来的永远是
/// 单一区域的纯文本。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SelectionArea {
    /// 内容区（聊天流 / 子 Agent 详情）。
    Chat,
    /// 输入框正文（不含边框）。
    Composer,
}

/// 应用内文本选区，坐标是**屏幕绝对坐标**（含宽字符）。
///
/// 为什么必须有它：一旦开启鼠标报告，终端就不会再处理拖选——事件全部进应用，
/// 终端自己的选区缓冲区是空的。Claude Code 的 fullscreen 模式也是这么做的：
/// 应用内选、松开即写系统剪贴板。反过来「不开鼠标报告」则滚轮在 Terminal.app
/// 下彻底失效（它不支持 DEC 1007）。两者不可兼得，只能在应用内把选择补回来。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TextSelection {
    /// 选区所属区域，决定夹取边界、高亮范围与取文字的屏幕快照。
    pub area: SelectionArea,
    /// 按下左键的锚点
    pub anchor: (u16, u16),
    /// 当前拖动到的位置（含）
    pub cursor: (u16, u16),
    /// 建立选区时所在区域的纵向滚动偏移。
    ///
    /// 坐标本身是屏幕坐标，滚动会让文字移走而坐标不动。记下建立时的偏移，
    /// 渲染与复制时按 `当前偏移 - scroll_anchor` 反向平移，选区就能**跟着文字
    /// 走**——滚轮不再需要清空选区。
    pub scroll_anchor: usize,
    /// 拖选是否仍在进行（松开后为 false，选区保留用于高亮与复制）
    pub dragging: bool,
}

impl TextSelection {
    pub(super) fn new(area: SelectionArea, anchor: (u16, u16), scroll_anchor: usize) -> Self {
        Self {
            area,
            anchor,
            cursor: anchor,
            scroll_anchor,
            dragging: true,
        }
    }

    /// 按纵向滚动差量平移后的选区。
    ///
    /// `delta` = 当前滚动偏移 − 建立时的偏移；滚动偏移增大意味着内容上移，
    /// 所以屏幕行要减去同一个量。平移可能把行推到区域外（负值），调用方在
    /// 绘制与取文时按区域矩形裁剪即可。
    pub(crate) fn shifted(&self, delta: i32) -> ((i32, i32), (i32, i32)) {
        let (start, end) = self.ordered();
        (
            (start.0 as i32, start.1 as i32 - delta),
            (end.0 as i32, end.1 as i32 - delta),
        )
    }

    /// 归一化后的 `(起, 止)`：无论从下往上还是从上往下拖，取的都是同一段。
    pub(super) fn ordered(&self) -> ((u16, u16), (u16, u16)) {
        let (ax, ay) = self.anchor;
        let (cx, cy) = self.cursor;
        if (ay, ax) <= (cy, cx) {
            ((ax, ay), (cx, cy))
        } else {
            ((cx, cy), (ax, ay))
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.anchor == self.cursor
    }
}

pub(super) const PASTE_HINT_DURATION: Duration = Duration::from_millis(1500);
/// 复制提示在状态栏停留的时长。
pub(super) const SELECTION_NOTICE_DURATION: Duration = Duration::from_millis(1500);
/// 双击判定窗口：同一位置两次按下间隔小于它就按「选词」处理。
pub(super) const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(400);

/// 滚动条控制的是哪一块滚动状态。
///
/// 内容区在不同焦点下画的是不同的东西（聊天流 / 子 Agent 详情），两者各有独立
/// 的偏移与跟随标志，拖动时必须落到对应那一份上。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScrollbarTarget {
    /// 聊天流（`chat_scroll`）。
    Chat,
    /// 子 Agent 详情（`sub_agent_detail_scroll`）。
    SubAgentDetail,
}

/// 滚动条拖动所需的几何信息，由渲染层每帧回写。
///
/// 拖动比例换算只依赖这几个数，与具体是聊天区还是详情面板无关——所以两边
/// 共用同一份结构，渲染侧各填各的。
#[derive(Debug, Clone, Copy)]
pub(crate) struct ScrollbarMetrics {
    /// 轨道矩形（宽 1 列）。
    pub area: ratatui::layout::Rect,
    /// 滑块顶部相对轨道顶部的行偏移。
    pub thumb_top: usize,
    /// 滑块长度（至少 1）。
    pub thumb_len: usize,
    /// 可滚动最大偏移。
    pub max_scroll: usize,
    /// 拖动落到哪一份滚动状态上。
    pub target: ScrollbarTarget,
}

impl ScrollbarMetrics {
    /// 复制一份并指向另一个滚动目标（渲染侧先建通用几何，再标注归属）。
    pub fn for_target(self, target: ScrollbarTarget) -> Self {
        Self { target, ..self }
    }

    /// 落在 `y` 行时滑块顶部应该在的位置（按比例映射，夹进轨道）。
    pub fn thumb_top_for(&self, y: u16) -> usize {
        let track = self.area.height as usize;
        (y.saturating_sub(self.area.y) as usize).min(track.saturating_sub(1))
    }

    /// 滑块顶部 → 滚动偏移。
    ///
    /// 滑块可移动的行程是 `轨道长 − 滑块长`，对应偏移行程 `0..=max_scroll`。
    /// 行程为 0（内容刚好装下或滑块占满轨道）时恒为 0。
    pub fn scroll_for_thumb_top(&self, thumb_top: usize) -> usize {
        let track = self.area.height as usize;
        let travel = track.saturating_sub(self.thumb_len);
        if travel == 0 || self.max_scroll == 0 {
            return 0;
        }
        let top = thumb_top.min(travel);
        // 四舍五入，避免最后一格差一点到不了底。
        (top * self.max_scroll + travel / 2) / travel
    }
}

/// 正在进行的滚动条拖动。
///
/// 按下时记下「按下点在滑块内的相对位置」，拖动时按 `y - grab_offset` 换算目标
/// 偏移。**不能**直接用按下点当滑块中心：滑块有长度，用户往往抓在它中间偏上
/// 或偏下的位置，用按下点定位会让滑块在第一次拖动时「跳一下」。
#[derive(Debug, Clone, Copy)]
pub(crate) struct ScrollbarDrag {
    /// 按下点在滑块内的相对位置（0 = 滑块顶部）。
    pub grab_offset: usize,
    /// 按下时的几何快照；拖动期间不随渲染刷新，保证换算稳定。
    pub metrics: ScrollbarMetrics,
}

/// 一屏聊天区文本快照的一行。
///
/// **每行只存一个 `String`**，而不是「每格一个 String」：内容区 120×40 就是
/// 4800 次分配，而主循环空闲时也按 50ms 轮询重绘——这个量级会直接体现在
/// 空闲 CPU 上。这里 `starts` 记下每格在 `text` 里的起始字节偏移，复制时按
/// 字节区间切片即可。
#[derive(Debug, Clone, Default)]
pub(crate) struct ScreenRow {
    pub y: u16,
    pub text: String,
    /// `(屏幕列, 该格在 `text` 中的起始字节偏移)`；宽字符的续格不占条目。
    pub starts: Vec<(u16, usize)>,
}

impl ScreenRow {
    /// 取 `[from, to]` 列区间内的文字（含两端）。
    ///
    /// 起点 = 第一个 `x >= from` 的格；终点 = 第一个 `x > to` 的格的起点（即
    /// 最后一格覆盖内容的右边界）。宽字符只登记起始列，落在它的续格上会顺延
    /// 到下一格——半个宽字符没有意义，宁可少选一个。
    pub(super) fn slice(&self, from: u16, to: u16) -> &str {
        let start = self
            .starts
            .iter()
            .find(|(x, _)| *x >= from)
            .map(|(_, offset)| *offset);
        let Some(start) = start else {
            return "";
        };
        let end = self
            .starts
            .iter()
            .find(|(x, _)| *x > to)
            .map(|(_, offset)| *offset)
            .unwrap_or(self.text.len());
        if start >= end {
            return "";
        }
        self.text.get(start..end).unwrap_or("")
    }
}

/// 双击选词的边界字符：把路径、URL、`path:line:col` 当作一个整体。
pub(super) fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric()
        || matches!(
            ch,
            '_' | '-' | '.' | '/' | '\\' | ':' | '@' | '#' | '%' | '+' | '=' | '~' | '&' | '?'
        )
}

/// 把选区还原成文字。
///
/// `delta` 是「当前滚动偏移 − 选区建立时的偏移」：滚动后选区要跟着文字走，
/// 所以取文时先把选区按同一个差量平移，再按平移后的屏幕行取快照。传 0 表示
/// 选区建立后没滚动过（绝大多数情况）。
///
/// 三条容易写错的地方：
/// 1. **跨行只在选区覆盖处换行**：首行从起点取到行尾、末行从行首取到终点。
///    整行整行地拼会把左右两侧的留白也复制进去。
/// 2. **行尾空白要 trim_end**：内容区右边距是补出来的空格，不裁掉的话每行都
///    拖一串尾巴，粘进终端/编辑器里全是多余空白。
/// 3. **单行选区不 trim_start**：用户可能只想复制缩进后的片段（代码块），
///    左端是他亲手点出来的边界。
pub(crate) fn selection_text(sel: &TextSelection, rows: &[ScreenRow], delta: i32) -> String {
    let ((sx, sy), (ex, ey)) = sel.shifted(delta);
    let mut out = Vec::new();
    for row in rows
        .iter()
        .filter(|r| (r.y as i32) >= sy && (r.y as i32) <= ey)
    {
        let from = if row.y as i32 == sy {
            sx.max(0) as u16
        } else {
            0
        };
        let to = if row.y as i32 == ey {
            ex.clamp(0, u16::MAX as i32) as u16
        } else {
            u16::MAX
        };
        if from > to {
            continue;
        }
        out.push(row.slice(from, to).trim_end().to_string());
    }
    // 末尾的纯空行没有信息量（用户拖到内容区底部时会带进来），去掉。
    while out.last().is_some_and(|line| line.is_empty()) {
        out.pop();
    }
    out.join("\n")
}

/// 双击时把落点所在的「词」扩展成选区。
///
/// 词边界用 [`is_word_char`]：它把 `src/app.rs:123`、URL、`--flag=value` 都算
/// 一个整体，与 iTerm2 的词边界取向一致。落在空白上时退化成该格本身，不让
/// 双击变成「什么都没选中」。
pub(super) fn word_bounds_at(rows: &[ScreenRow], x: u16, y: u16) -> ((u16, u16), (u16, u16)) {
    let Some(row) = rows.iter().find(|r| r.y == y) else {
        return ((x, y), (x, y));
    };
    let idx = match row.starts.iter().position(|(cx, _)| *cx == x) {
        Some(i) => i,
        None => return ((x, y), (x, y)),
    };
    // 每格在 `text` 里的切片就是它自己的字符（宽字符也只占一个 entry）。
    let char_at = |i: usize| -> Option<char> {
        let (_, start) = *row.starts.get(i)?;
        let end = row
            .starts
            .get(i + 1)
            .map(|(_, offset)| *offset)
            .unwrap_or(row.text.len());
        row.text.get(start..end)?.chars().next()
    };
    if !char_at(idx).is_some_and(is_word_char) {
        return ((x, y), (x, y));
    }
    let mut start = idx;
    while start > 0 && char_at(start - 1).is_some_and(is_word_char) {
        start -= 1;
    }
    let mut end = idx;
    while char_at(end + 1).is_some_and(is_word_char) {
        end += 1;
    }
    ((row.starts[start].0, y), (row.starts[end].0, y))
}

/// 超过任一阈值就把粘贴的长文本折叠成 `[Pasted text #n +N lines]` 占位符。
///
/// 取「200 字符 / 5 行」是用户选定的口径：日常敲的字、几行路径、一次报错片段
/// 都不受影响，一旦是真日志/真源码就立刻折叠。真正的判据不是数值本身，而是
/// 「会不会把 8 行的可视区灌满」——超过就一定会看不见。
pub(super) const PASTE_FOLD_MIN_CHARS: usize = 200;
pub(super) const PASTE_FOLD_MIN_LINES: usize = 5;

pub(super) fn paste_text_should_fold(text: &str) -> bool {
    text.chars().count() >= PASTE_FOLD_MIN_CHARS
        || text.lines().filter(|l| !l.trim().is_empty()).count() >= PASTE_FOLD_MIN_LINES
}

/// 把一段长文本折叠成占位符。返回 `false` 表示不需要折叠（调用方按普通文本插入）。
pub(super) fn fold_paste(state: &mut AppState, input: &mut InputBox, text: &str) -> bool {
    if !paste_text_should_fold(text) {
        return false;
    }
    let line_count = text.lines().count().max(1);
    let index = input.paste_insertion_index();
    state.pending_pastes.insert(
        index,
        PasteBlock {
            text: text.to_string(),
            line_count,
        },
    );
    input.insert_paste_marker();
    true
}

/// 剪贴板里的一批文件 → 每个文件一个 `Attachment::File`，在光标处按顺序插标记。
pub(super) fn attach_files(
    state: &mut AppState,
    input: &mut InputBox,
    files: &[PathBuf],
) -> Option<PasteHint> {
    for path in files {
        if state.pending_attachments.iter().any(
            |attachment| matches!(attachment, Attachment::File { path: existing } if existing == path),
        ) {
            continue;
        }
        input.reconcile_attachment_markers(state.pending_attachments.len());
        let insertion_index = input.attachment_insertion_index();
        state
            .pending_attachments
            .insert(insertion_index, Attachment::File { path: path.clone() });
        input.insert_attachment_marker();
    }
    // 文件由输入框内的持久占位符确认，不需要瞬时提示。
    None
}

pub(super) fn apply_clipboard_paste(
    state: &mut AppState,
    input: &mut InputBox,
    paste: ClipboardPaste,
) -> Option<PasteHint> {
    let expires_at = Instant::now() + PASTE_HINT_DURATION;
    match paste {
        ClipboardPaste::Image {
            data,
            width,
            height,
        } => {
            let already_attached = state.pending_attachments.iter().any(|attachment| {
                matches!(attachment, Attachment::Image { data: existing, .. } if existing == &data)
            });
            if !already_attached {
                input.reconcile_attachment_markers(state.pending_attachments.len());
                let insertion_index = input.attachment_insertion_index();
                state.pending_attachments.insert(
                    insertion_index,
                    Attachment::Image {
                        media_type: "image/png".to_string(),
                        data,
                        preview_label: format!("{width}×{height}"),
                    },
                );
                input.insert_attachment_marker();
            }
            // 图片会以持久的 `[Image #n]` 占位显示在输入框里，不再叠加瞬时提示。
            None
        }
        ClipboardPaste::Files(files) => attach_files(state, input, &files),
        ClipboardPaste::Text(pasted) => {
            if let Some(path) = try_resolve_path(pasted.trim()) {
                return attach_files(state, input, std::slice::from_ref(&path));
            }
            // 长文本折叠优先于直接插入：占位符已经自我说明（`[Pasted text #1
            // +250 lines]`），不需要再叠一层 1.5 秒就消失的提示。
            if fold_paste(state, input, &pasted) {
                return None;
            }
            input.insert_text(&pasted);
            pasted_text_needs_hint(&pasted).then(|| PasteHint {
                text: format!(
                    "[{}]",
                    wyj_i18n::tr_fmt(
                        "input.paste_text",
                        &[("count", &pasted.chars().count().to_string())]
                    )
                ),
                expires_at,
                cursor_row: input.cursor_row,
                cursor_col: input.cursor_col,
            })
        }
    }
}

pub(super) fn composer_has_content(state: &AppState, input: &InputBox) -> bool {
    !input.is_empty() || !state.pending_attachments.is_empty() || !state.pending_pastes.is_empty()
}

/// 取出输入框里「不该发给模型之前就丢掉」的东西：折叠的粘贴块必须先按位置
/// 展开成**原文**，否则 `InputBox::text()` 只会把标记过滤掉，用户贴的三百行
/// 日志会变成一条空消息。
///
/// 返回值第二项把粘贴块**交回给调用方**，不留在 `state` 里：调用方紧接着要用
/// 「发送瞬间的输入框快照」把它们投影成 `[Pasted text #n +N lines]`
/// （`render::composer_display_text`），而标记在 `*input = InputBox::new()` 时
/// 已经没了。在函数内部清掉会让展示投影拿到空列表，聊天区里一个占位符都不剩。
///
/// 三处提交路径（主发送 / 忙碌时排队 / 斜杠命令展开）都必须走这里，不要各自
/// 调 `input.take()`——`take()` 拿不到 `pending_pastes`。
pub(super) fn take_composer_text(
    state: &mut AppState,
    input: &mut InputBox,
) -> (String, Vec<PasteBlock>) {
    let text = input.text_with_pastes(&state.pending_pastes);
    let pastes = std::mem::take(&mut state.pending_pastes);
    *input = InputBox::new();
    (text, pastes)
}

/// 删除输入框光标左侧的内容。若紧邻光标的是一个原子标记（附件或折叠的粘贴块），
/// 则**整体**删除并同步删掉对应的结构化数据；否则保持普通文本 Backspace 语义。
///
/// 「整体」是硬要求：`[Pasted text #1 +250 lines]` 若允许逐字退格，
/// 用户会得到一个删掉开头、看起来毫无意义的残片，还误以为内容本来就这么点。
pub(super) fn backspace_composer(state: &mut AppState, input: &mut InputBox) {
    input.reconcile_attachment_markers(state.pending_attachments.len());
    if let Some(index) = input.paste_marker_before_cursor() {
        input.backspace();
        state.pending_pastes.remove(index);
        return;
    }
    if let Some(index) = input.attachment_marker_before_cursor() {
        input.backspace();
        state.pending_attachments.remove(index);
        return;
    }
    input.backspace();
}

pub(super) fn delete_composer_forward(state: &mut AppState, input: &mut InputBox) {
    input.reconcile_attachment_markers(state.pending_attachments.len());
    if let Some(index) = input.paste_marker_at_cursor() {
        input.delete_char_forward();
        state.pending_pastes.remove(index);
        return;
    }
    if let Some(index) = input.attachment_marker_at_cursor() {
        input.delete_char_forward();
        state.pending_attachments.remove(index);
        return;
    }
    input.delete_char_forward();
}

pub(super) fn delete_word_backward_composer(state: &mut AppState, input: &mut InputBox) {
    input.reconcile_attachment_markers(state.pending_attachments.len());
    // Alt+Backspace / Ctrl+W：命中原子标记时退化成整体删除，而不是把占位符
    // 当普通单词删出 `[Pasted text` 这种半截残片。
    if input.paste_marker_before_cursor().is_some()
        || input.attachment_marker_before_cursor().is_some()
    {
        backspace_composer(state, input);
    } else {
        input.delete_word_backward();
    }
}

pub(super) fn clear_composer(state: &mut AppState, input: &mut InputBox) {
    *input = InputBox::new();
    state.pending_attachments.clear();
    state.pending_pastes.clear();
    state.paste_hint = None;
    state.slash_completions.clear();
    state.file_completions.clear();
    state.history_idx = None;
    state.history_draft = None;
}

#[cfg(test)]
mod clipboard_paste_tests {
    use super::*;
    use wyj_tools::SubAgentHub;

    fn make_state() -> AppState {
        AppState::new(
            PathBuf::from("/tmp"),
            "test-model".to_string(),
            200_000,
            AgentMode::Auto,
            Config::default(),
            Arc::new(SubAgentHub::new()),
        )
    }

    /// 主循环取 session 锁必须**永不阻塞**。
    ///
    /// 回合 task（`spawn_agent_turn` / 两处内联 turn task）会持锁跑完**整个**
    /// agent 回合——LLM 往返加全部工具调用，动辄几分钟。主循环一旦在这把锁上
    /// `.await`，界面就停在那一帧：不重绘、不读按键，连排在保存分支上游的
    /// `abort_stalled_turn()` 看门狗都跑不到下一轮，ESC/Ctrl+C 也失效。
    /// 超时兜底：哪天有人把它改回 `lock().await`，这条会挂在 timeout 上失败，
    /// 而不是静默通过。
    #[tokio::test]
    async fn try_session_lock_never_waits_for_a_running_turn() {
        let session = Arc::new(Mutex::new(Session::new()));
        let holder = session.clone();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let _guard = holder.lock().await; // 模拟回合 task 持锁
            let _ = release_rx.await;
        });
        tokio::task::yield_now().await; // 让 holder 先跑到持锁

        let unavailable = tokio::time::timeout(Duration::from_millis(200), async {
            try_session_lock(&session).is_none()
        })
        .await
        .expect("try_session_lock 不得阻塞等待回合释放锁");
        assert!(unavailable, "锁被回合持有时必须立刻返回 None");

        let _ = release_tx.send(());
    }

    /// 排队消息必须在**提交那一刻**就出现在聊天流里，且只在 `AgentEvent::Injected`
    ///（Agent 真正排空时）出队、不再画第二遍。
    ///
    /// 回显原本挂在 `AgentEvent::Injected` 上，而排空点在 Agent 的工具轮边界——
    /// 前台子 Agent / 长 Bash 期间可能几十分钟不排空，用户那边看就是「我输入的
    /// 字不见了」。
    #[test]
    fn queued_message_is_echoed_on_submit_and_not_duplicated_on_drain() {
        let mut state = make_state();
        let before = state.messages.len();

        state.enqueue_message(OutgoingMessage::plain("顺便看一下成交量"));

        assert_eq!(state.pending_queue.len(), 1);
        assert_eq!(
            state.messages.len(),
            before + 1,
            "入队当场就要回显，不能等 Agent 排空"
        );
        assert_eq!(state.messages.last().unwrap().content, "顺便看一下成交量");

        // Agent 排空：只出队，不再回显（否则同一条消息显示两遍）
        state.apply_agent_event(AgentEvent::Injected);
        assert!(state.pending_queue.is_empty());
        assert_eq!(state.messages.len(), before + 1, "排空时不得重复回显");
    }

    /// 前台子 Agent 取走排队消息时**不会**发 `Injected`（它没有 UI 事件通道），
    /// 因此镜像会停在旧水位；回合一结束，兜底重发就会把这条已经投递给子 Agent
    /// 的消息**再当一条全新用户消息发一遍**——同一条要求子 Agent 干一遍、主
    /// Agent 再干一遍，token 双倍。
    ///
    /// 这条钉子守的是共享 steer 队列最容易踩的坑：只测「Agent 层只投递一次」
    /// （`shared_steer_queue_delivers_to_exactly_one_agent`）是不够的，UI 侧
    /// 的镜像必须跟着真实水位一起走。
    #[test]
    fn subagent_drained_steerage_is_not_resent_as_a_new_user_message() {
        let mut state = make_state();
        let queue = Arc::new(wyj_core::SteerQueue::new());
        state.injector = Some(queue.clone());

        // 用户在子 Agent 运行期间补充两条要求：UI 镜像 + 共享队列各加两条
        state.enqueue_message(OutgoingMessage::plain("先看成交量"));
        queue.push(
            vec![ContentBlock::Text {
                text: "先看成交量".into(),
            }],
            wyj_core::InjectionKind::UserMessage,
        );
        state.enqueue_message(OutgoingMessage::plain("再看资金流"));
        queue.push(
            vec![ContentBlock::Text {
                text: "再看资金流".into(),
            }],
            wyj_core::InjectionKind::UserMessage,
        );
        assert_eq!(state.pending_queue.len(), 2);

        // 子 Agent 在自己的工具轮边界把两条都取走（不发 Injected）
        let taken = queue.drain_user_messages();
        assert_eq!(taken.len(), 2);
        assert_eq!(state.pending_queue.len(), 2, "此时镜像还停在旧水位");

        // 回合结束：对齐后镜像必须清空，兜底重发才不会把它们再发一遍
        state.is_thinking = false;
        state.reconcile_pending_queue();
        assert!(
            state.pending_queue.is_empty(),
            "已被子 Agent 取走的排队消息不得再被重发"
        );
    }

    /// 反向钉子：**确实没被取走**的消息必须留在镜像里，否则真正的竞态兜底
    /// （提交时机晚于本轮最后一次排空）会把用户的输入静默吞掉。
    #[test]
    fn undelivered_steerage_survives_reconciliation_for_the_race_fallback() {
        let mut state = make_state();
        let queue = Arc::new(wyj_core::SteerQueue::new());
        state.injector = Some(queue.clone());

        state.enqueue_message(OutgoingMessage::plain("继续"));
        queue.push(
            vec![ContentBlock::Text {
                text: "继续".into(),
            }],
            wyj_core::InjectionKind::UserMessage,
        );
        // 队列里还有一条后台结果通知（SystemReminder），它从不进 pending_queue
        queue.push(
            vec![ContentBlock::Text {
                text: "后台 agent 完成".into(),
            }],
            wyj_core::InjectionKind::SystemReminder,
        );

        state.reconcile_pending_queue();
        assert_eq!(
            state.pending_queue.len(),
            1,
            "未被取走的用户消息必须保留给兜底重发"
        );
        assert_eq!(state.pending_queue[0].text, "继续");
    }

    #[tokio::test]
    async fn try_session_lock_succeeds_when_no_turn_is_running() {
        let session = Arc::new(Mutex::new(Session::new()));
        let guard = tokio::time::timeout(Duration::from_millis(200), async {
            try_session_lock(&session)
        })
        .await
        .expect("空闲时取锁不应超时");
        assert!(guard.is_some(), "没有回合在跑时必须能直接拿到锁");
    }

    /// 终态事件（Usage / TurnDone / Error）走的是**阻塞式** send（通道容量
    /// 256），而 UI 收到 TurnDone 后会把 `save_needed` 置位、下一帧就去
    /// `session.lock().await` 保存。若 turn task 发事件时**仍持着锁**：
    /// UI 等锁、turn task 等 UI 排空通道 → 环形等待，界面永久冻结，且排在
    /// 保存分支上游的看门狗 `abort_stalled_turn()` 也跑不到。
    /// 三处 turn task 都必须先放锁再发——静态钉子，谁把 `drop(sess)` 删掉或
    /// 挪到发送之后，这条立刻变红。
    #[test]
    fn every_turn_task_releases_the_session_lock_before_terminal_events() {
        // 发送点按「含 `AgentEvent::TurnDone {` 且随后几行内出现 `.await;`」识别：
        // 事件带了 `truncated` 字段后是多行结构体字面量，不能再按整行匹配。
        const SEND: &str = "AgentEvent::TurnDone {";
        let source = include_str!("../app.rs");
        let lines: Vec<&str> = source.lines().collect();

        let mut checked = 0usize;
        let mut segment_start = 0usize; // 当前 turn task 在源码中的起始行
        for (idx, line) in lines.iter().enumerate() {
            if line.contains("let task = spawn_turn_task(async move {")
                || line.contains("fn spawn_agent_turn(")
            {
                segment_start = idx;
            }
            if !line.contains(SEND) || line.trim_start().starts_with("//") {
                continue;
            }
            // 真实发送点：后续 4 行内必须出现整行以 `.await;` 结尾的语句。
            // （按结构匹配，避免把本测试源码里的字符串字面量当成发送点。）
            let is_send = lines[idx..lines.len().min(idx + 5)]
                .iter()
                .any(|l| l.trim_end().ends_with(".await;"));
            if !is_send {
                continue;
            }
            let segment = lines[segment_start..idx].join("\n");
            let usage_at = segment
                .find("let usage = AgentEvent::Usage")
                .unwrap_or_else(|| {
                    panic!("终态 Usage 事件应在锁内先取好快照（免得放锁后再读 sess）")
                });
            let drop_at = segment
                .rfind("drop(sess)")
                .unwrap_or_else(|| panic!("turn task 在发终态事件前没有释放 session 锁"));
            assert!(
                usage_at < drop_at,
                "顺序必须是「锁内取 Usage 快照 → drop(sess) → 发事件」"
            );
            checked += 1;
        }
        assert_eq!(checked, 3, "三处 turn task 都应被这条钉子覆盖");
    }

    /// 主循环碰不到 session 锁时**不能 `continue`**——静态钉子。
    ///
    /// `save_needed` 分支位于主循环的**无条件区段**（不在 `event::poll()`
    /// 的按键处理块里）。这里的 `continue` 跳回的是整个循环，被跳过的正是
    /// 紧随其后的 `ui_ask_rx` 消费与 `event::poll()`：按键、权限确认弹窗、
    /// AskQuestion、计划批准全部停摆。而 `terminal.draw()` 在本行之前，
    /// 画面照常刷新 —— 用户看到的是「界面活着但按键全哑」，ESC/Ctrl+C 也
    /// 救不回来（真正终止回合的是排在 `continue` **上游**的回合心跳看门狗，
    /// 因此现场更像「莫名其妙卡住几分钟后自己恢复」）。
    ///
    /// 触发极其常见：`TurnDone` 置位 `save_needed` 与自动唤醒 / 排队兜底
    /// 的 spawn 发生在同一帧，所以**每一条排队消息触发的自动续跑都会踩中**，
    /// 整个新回合全程失灵。
    ///
    /// 正确的形状是「拿不到锁就只置位回退，流程自然落到下面继续消费输入
    /// 层」——存盘晚一帧没有代价，等锁才有。
    #[test]
    fn autosave_never_continues_past_the_input_layer() {
        let source = include_str!("../app.rs");
        let lines: Vec<&str> = source.lines().collect();
        // 按「整行 trim 后正好等于」匹配，避免命中本测试源码里那句字符串
        // 字面量（`include_str!` 把测试自己也包进来了）。
        let start = lines
            .iter()
            .position(|l| l.trim() == "if state.save_needed {")
            .expect("主循环应有 autosave 分支");
        // 分支体内不得出现 `continue`：从 `if state.save_needed {` 起，找到
        // 缩进回落到该层级为止的整个块。
        let indent = lines[start].len() - lines[start].trim_start().len();
        let body = lines[start + 1..]
            .iter()
            .take_while(|l| {
                let ind = l.len() - l.trim_start().len();
                l.trim().is_empty() || ind > indent
            })
            .collect::<Vec<_>>();
        assert!(
            !body.iter().any(|l| l.trim() == "continue;"),
            "autosave 分支不得 `continue`：那会跳过 ui_ask 消费与 event::poll()，\
             整个输入层在回合运行期间全程停摆"
        );
        // 存盘必须 fire-and-forget：`save` 是同步 IO + 跨进程文件锁（最坏
        // 等 30 秒），主循环 `.await` 它等于把整帧钉住——与直接调 `save`
        // 的现场相同，只是换了个线程在等。
        //
        // 判据是「`save_async` 的 await 是否落在 `tokio::spawn` 的闭包内」：
        // 闭包里的 await 在独立 task 上跑，不阻塞主循环；闭包外则是主循环在等。
        // 只看非注释行——注释里必然会提到这两个词来解释原因。
        let mut in_spawn = 0i32;
        let mut violating = false;
        for line in body.iter().filter(|l| !l.trim_start().starts_with("//")) {
            if line.contains("tokio::spawn") {
                in_spawn += 1;
            }
            if line.contains("save_async") && line.contains(".await") && in_spawn == 0 {
                violating = true;
            }
        }
        assert!(
            !violating,
            "autosave 不得在主循环里 await 存盘：界面会停在当前帧直到文件锁释放"
        );
    }

    /// 取出 `tui_main` 的函数体行（不含签名与收尾大括号）。
    ///
    /// 签名是多行的，结束的 `) -> Result<...> {` 顶格，必须先跳过它再按缩进
    /// 收块，否则 take_while 会在签名末尾就把整个函数体截断。
    fn tui_main_body() -> Vec<String> {
        let source = include_str!("../app.rs");
        let lines: Vec<&str> = source.lines().collect();
        let signature = lines
            .iter()
            .position(|l| l.trim() == "async fn tui_main(")
            .expect("app.rs 里应有 tui_main");
        let body_start = (signature + 1..)
            .find(|offset| lines[*offset].ends_with('{'))
            .map(|offset| offset + 1)
            .expect("tui_main 签名应以 `{` 收尾");
        let indent = lines[body_start - 1].len() - lines[body_start - 1].trim_start().len();
        let body = lines[body_start..]
            .iter()
            .take_while(|l| l.trim().is_empty() || (l.len() - l.trim_start().len()) > indent)
            .map(|l| l.to_string())
            .collect::<Vec<_>>();
        assert!(
            body.len() > 1000,
            "tui_main 提取出的函数体过短，多半是收块失败"
        );
        body
    }

    /// 取 `match` 分支的**函数体**（不含模式本身）。
    ///
    /// 模式可能跨多行（`Ok(CommandResult::Rewind {` 后面跟着三个绑定、闭括号
    /// 回到与模式同缩进），直接从模式行往后 take_while 只能拿到绑定列表，
    /// 拿不到 `=> { ... }` 里的逻辑。这里先定位真正的 `=> {` 行，再按
    /// 「缩进大于模式行」收块。
    fn match_arm_body<'a>(body: &'a [String], marker: &str) -> Vec<&'a str> {
        let arm = body
            .iter()
            .position(|l| l.contains(marker))
            .unwrap_or_else(|| panic!("主循环应能定位到 {marker} 分支"));
        let indent = body[arm].len() - body[arm].trim_start().len();
        let start = (arm..)
            .find(|offset| body[*offset].trim_end().ends_with("=> {"))
            .unwrap_or_else(|| panic!("{marker} 分支应是空分支或格式异常"));
        body[start + 1..]
            .iter()
            .take_while(|l| l.trim().is_empty() || (l.len() - l.trim_start().len()) > indent)
            .map(|l| l.as_str())
            .collect()
    }

    /// 核心不变量（`crates/tui/CLAUDE.md` 第 19 条）：**主循环里不得出现
    /// `session.lock().await`**。
    ///
    /// 主循环是同时负责重绘与读按键的**同一个 task**；在它上面等 session 锁，
    /// 等于把「界面重绘 + 事件轮询 + 回合看门狗」全部停摆——用户看到的是整个
    /// 界面冻死、ESC / Ctrl+C 全部读不到，只能 kill。拿不到锁就该 `try_lock`
    /// 立刻放弃并落回输入层。
    #[test]
    fn main_loop_never_awaits_the_session_lock() {
        for (offset, line) in tui_main_body().iter().enumerate() {
            // 注释里讨论这条规则的说明文字不算
            if line.trim_start().starts_with("//") {
                continue;
            }
            let compact = line.split("//").next().unwrap_or(line);
            assert!(
                !compact.contains("session.lock().await"),
                "tui_main 第 {} 行（源码）出现 `session.lock().await`：主循环绝不能等 session 锁，\
                 改成 try_lock 或把活丢给后台 task。",
                offset + 1
            );
        }
    }

    /// `/compact` 曾是整个类别的典型样本：一次完整的 LLM 往返被 inline await 在
    /// 主循环上。摘要输出预算 `min(cw/10, 16000)`，再叠 `RetryPolicy::default()`
    /// 的 5 次指数退避，端点不稳时能冻住界面数分钟且毫无反馈。
    ///
    /// 这里钉的是「必须交给 `spawn_turn_task`」而不是「不许出现 compact_context」
    /// ——后者会把正常的后台实现也一起禁掉。
    #[test]
    fn compact_runs_in_a_background_task_not_on_the_ui_thread() {
        let body = tui_main_body();
        let inner = match_arm_body(&body, "CommandResult::CompactHistory");
        let text = inner.join("\n");
        assert!(
            text.contains("spawn_turn_task(") && text.contains("begin_background_task("),
            "/compact 必须在后台 task 里跑并点亮忙碌态：inline await 会让重绘、\
             event::poll() 和 Ctrl+C 全部停摆"
        );
        assert!(
            text.contains("AgentEvent::CompactFinished"),
            "/compact 的结果必须经 AgentEvent::CompactFinished 回传（锁必须先放再发）"
        );
    }

    /// 同类钉子：工作区快照（git 管道 / 全树哈希）是秒级同步 IO，压在主循环上
    /// 一样是"界面按住不动、按键读不到"。`/checkpoint`、`/rewind`、`/branch`、
    /// `/evolve` 四条路径都必须先出后台 task。
    #[test]
    fn workspace_snapshot_commands_run_in_background_tasks() {
        let body = tui_main_body();
        for arm_marker in [
            "CommandResult::CreateCheckpoint {",
            "CommandResult::Rewind {",
            "CommandResult::BranchSession {",
        ] {
            let text = match_arm_body(&body, arm_marker).join("\n");
            assert!(
                text.contains("begin_background_task("),
                "{arm_marker} 分支必须调用 begin_background_task：工作区快照是秒级同步 IO，\
                 压在主循环上等于界面无响应"
            );
        }

        // /evolve 的确认动作没有 CommandResult 走同一个 match，只能按
        // 「execute_evolution_action 的调用点必须与 spawn / 忙碌标记同窗」来钉。
        let call_at = body
            .iter()
            .position(|l| l.contains("execute_evolution_action("))
            .expect("主循环应有 execute_evolution_action 调用");
        let window_start = call_at.saturating_sub(15);
        let window_end = (call_at + 15).min(body.len() - 1);
        let window = body[window_start..=window_end].join("\n");
        assert!(
            window.contains("spawn_turn_task(") && window.contains("\"/evolve\""),
            "/evolve 的确认动作必须在后台 task 里跑：激活候选前要建工作区快照"
        );
    }

    /// `/bug`、`/pr-comments` 的执行体是子进程（`git` / `gh` / `open`），完成时间
    /// 不可预期，必须由 TUI 丢后台 task。这里钉住「声明集合」本身：新增阻塞型
    /// 命令时忘了标 `needs_background`，或者误标了纯本地命令，都会被拦下。
    #[test]
    fn only_process_spawning_commands_are_marked_background() {
        let registry = wyj_commands::builtin::standard_registry();
        assert!(
            registry.needs_background("bug"),
            "/bug 会拉起系统浏览器，必须后台执行"
        );
        assert!(
            registry.needs_background("pr-comments"),
            "/pr-comments 会跑两次 gh 子进程，必须后台执行"
        );
        // 这些是纯内存 / 纯配置操作，标 background 只会白白多一次 task 往返
        for name in ["help", "compact", "model", "status", "rewind", "checkpoint"] {
            assert!(
                !registry.needs_background(name),
                "/{name} 是本地操作，不应声明 needs_background"
            );
        }
    }

    #[test]
    fn command_name_extraction_handles_slash_inputs() {
        use wyj_commands::CommandRegistry;
        assert_eq!(CommandRegistry::command_name("/bug 崩了"), Some("bug"));
        assert_eq!(
            CommandRegistry::command_name("  /pr-comments  "),
            Some("pr-comments")
        );
        assert_eq!(CommandRegistry::command_name("/"), None);
        assert_eq!(CommandRegistry::command_name("/   "), None);
        assert_eq!(CommandRegistry::command_name("hello"), None);
    }

    #[test]
    fn flush_thinking_records_elapsed_and_clears_the_timer() {
        // `thinking_started` 自 ThinkingDelta 首块写入后一直只有写点、零读点，
        // 固化时直接丢弃会让折叠头永远拿不到耗时
        let mut state = make_state();
        state.thinking_buf.push_str("第一段\n第二段");
        state.thinking_started =
            Some(std::time::Instant::now() - std::time::Duration::from_millis(1200));

        state.flush_thinking();

        assert!(state.thinking_buf.is_empty());
        assert!(state.thinking_started.is_none());
        let thinking = state
            .messages
            .iter()
            .find(|m| m.role == MessageRole::Thinking)
            .expect("thinking 应已固化为消息");
        assert_eq!(thinking.content, "第一段\n第二段");
        let secs = thinking
            .thinking_elapsed_secs
            .expect("固化时必须把耗时落到消息上");
        assert!((1.1..5.0).contains(&secs), "secs={secs}");
    }

    #[test]
    fn output_expanded_defaults_to_auto_mode_and_resets_with_session() {
        let mut state = make_state();
        assert!(
            !state.output_expanded,
            "默认进自动模式：执行中的步骤自动展开，历史保持折叠"
        );

        state.output_expanded = true;
        state.reset_for_new_session();
        assert!(!state.output_expanded, "新会话回到自动模式");
    }

    #[test]
    fn live_tool_output_only_lands_while_the_call_is_in_flight() {
        let mut state = make_state();
        let chunk = ToolOutputChunk {
            tool_use_id: "toolu_1".to_string(),
            tail: "first".to_string(),
        };

        // ToolStart 之前：还没有调用登记，快照无处可挂（也无处可显）
        state.is_thinking = true;
        state.apply_tool_output_chunk(chunk.clone());
        assert!(state.live_tool_out.is_empty(), "未登记的调用不得产生实时块");

        // ToolStart 之后
        state.tool_info.insert("toolu_1".into(), ("Bash".into(), 0));
        state.apply_tool_output_chunk(chunk.clone());
        assert_eq!(
            state.live_tool_out.get("toolu_1").map(String::as_str),
            Some("first")
        );

        // 后续快照原地替换（快照语义：每帧自带完整累计输出）
        state.apply_tool_output_chunk(ToolOutputChunk {
            tool_use_id: "toolu_1".to_string(),
            tail: "first\nsecond".to_string(),
        });
        assert_eq!(
            state.live_tool_out.get("toolu_1").map(String::as_str),
            Some("first\nsecond")
        );

        // ToolEnd 收工
        let tail = "first\nsecond".to_string();
        state.apply_agent_event(AgentEvent::ToolEnd {
            id: "toolu_1".to_string(),
            output: tail,
            is_error: false,
            elapsed_secs: 0.1,
        });
        assert!(
            state.live_tool_out.is_empty(),
            "调用结束必须摘掉快照，否则哈希表里会一直躺着没人渲染的 8KB"
        );
    }

    #[test]
    fn late_live_output_after_an_interrupted_turn_is_dropped() {
        // ESC 中断 abort 掉回合 future，但 Bash 的 pump 会一直读到管道 EOF，
        // 中断之后还可能推来最后几帧。这些帧若被接收，聊天区会留下一块永远
        // 转圈的幽灵输出。
        let mut state = make_state();
        state.is_thinking = true;
        state.tool_info.insert("toolu_1".into(), ("Bash".into(), 0));

        // 中断：回合结束但 tool_info 未被清理（与真实路径一致）
        state.is_thinking = false;
        state.apply_tool_output_chunk(ToolOutputChunk {
            tool_use_id: "toolu_1".to_string(),
            tail: "late".to_string(),
        });
        assert!(
            state.live_tool_out.is_empty(),
            "回合已结束后到达的快照必须丢弃"
        );
    }

    #[test]
    fn live_tool_output_does_not_mark_unseen_messages() {
        // 实时刷新是同一块区域的原地更新。若点亮「未读」，用户上滚翻历史时
        // 会每帧被计一次新消息。
        let mut state = make_state();
        state.is_thinking = true;
        state.tool_info.insert("toolu_1".into(), ("Bash".into(), 0));
        state.chat_follow_tail = false;
        state.unseen_messages = false;

        state.apply_tool_output_chunk(ToolOutputChunk {
            tool_use_id: "toolu_1".to_string(),
            tail: "noise".to_string(),
        });

        assert!(!state.unseen_messages);
    }

    /// `Cmd+C` 是「复制选区」，**不是**「往输入框敲一个 c」。
    ///
    /// 现场：鼠标选中内容区后按 Command+C，输入框里凭空冒出一个 `c`。根因是主
    /// 循环的字符插入分支只过滤了 `CONTROL`，而 macOS 的 Kitty 键盘协议把 Cmd+C
    /// 报成 `Char('c') + SUPER`。修法两层：① 这里识别出复制键并**先于**
    /// `clear_selection()` 消费掉（选区正是要复制的东西）；② 字符插入统一走
    /// `input::plain_char`，任何带修饰位的字符键都不再当正文（钉子见
    /// `input::tests::modified_char_keys_are_never_plain_text`）。
    #[test]
    fn cmd_c_is_a_copy_shortcut_not_a_typed_c() {
        // macOS 的 Cmd+C
        assert!(is_clipboard_copy_key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::SUPER,
        )));
        assert!(is_clipboard_copy_key(KeyEvent::new(
            KeyCode::Char('C'),
            KeyModifiers::SUPER | KeyModifiers::SHIFT,
        )));
        // 其余平台终端惯例：Ctrl+Shift+C
        assert!(is_clipboard_copy_key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        )));
        // 裸 Ctrl+C 必须留给「中断回合 / 二次确认退出」，不能被复制抢走
        assert!(!is_clipboard_copy_key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )));
        assert!(!is_clipboard_copy_key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::NONE,
        )));
        // 复制键本身也不能被当成要插入的正文
        assert_eq!(
            crate::input::plain_char(&KeyEvent::new(KeyCode::Char('c'), KeyModifiers::SUPER)),
            None
        );
    }

    #[test]
    fn ctrl_or_super_v_is_an_explicit_clipboard_paste() {
        assert!(is_clipboard_paste_key(KeyEvent::new(
            KeyCode::Char('v'),
            KeyModifiers::CONTROL,
        )));
        assert!(is_clipboard_paste_key(KeyEvent::new(
            KeyCode::Char('V'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        )));
        assert!(is_clipboard_paste_key(KeyEvent::new(
            KeyCode::Char('v'),
            KeyModifiers::SUPER,
        )));
        assert!(!is_clipboard_paste_key(KeyEvent::new(
            KeyCode::Char('v'),
            KeyModifiers::NONE,
        )));
        assert!(!is_clipboard_paste_key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )));
    }

    #[test]
    fn text_paste_hint_is_only_created_for_multiple_lines() {
        let mut state = make_state();
        let mut input = InputBox::new();

        let hint = apply_clipboard_paste(
            &mut state,
            &mut input,
            ClipboardPaste::Text("单行文字".to_string()),
        );
        assert!(hint.is_none());
        assert_eq!(input.display_lines(), &["单行文字"]);

        let hint = apply_clipboard_paste(
            &mut state,
            &mut input,
            ClipboardPaste::Text("\n第二行".to_string()),
        );
        assert!(hint.is_some());
        assert_eq!(input.display_lines(), &["单行文字", "第二行"]);
    }

    #[test]
    fn image_paste_creates_an_in_memory_attachment() {
        let mut state = make_state();
        let mut input = InputBox::new();

        let hint = apply_clipboard_paste(
            &mut state,
            &mut input,
            ClipboardPaste::Image {
                data: "encoded-png".to_string(),
                width: 320,
                height: 180,
            },
        );

        assert!(hint.is_none());
        assert!(matches!(
            state.pending_attachments.as_slice(),
            [Attachment::Image {
                media_type,
                data,
                preview_label,
            }] if media_type == "image/png"
                && data == "encoded-png"
                && preview_label == "320×180"
        ));

        let duplicate_hint = apply_clipboard_paste(
            &mut state,
            &mut input,
            ClipboardPaste::Image {
                data: "encoded-png".to_string(),
                width: 320,
                height: 180,
            },
        );
        assert!(duplicate_hint.is_none());
        assert_eq!(state.pending_attachments.len(), 1);
    }

    #[test]
    fn multiple_distinct_image_pastes_are_kept_in_order() {
        let mut state = make_state();
        let mut input = InputBox::new();

        for (data, width, height) in [("first", 320, 180), ("second", 640, 480)] {
            let hint = apply_clipboard_paste(
                &mut state,
                &mut input,
                ClipboardPaste::Image {
                    data: data.to_string(),
                    width,
                    height,
                },
            );
            assert!(hint.is_none());
        }

        assert!(matches!(
            state.pending_attachments.as_slice(),
            [
                Attachment::Image { data: first, .. },
                Attachment::Image { data: second, .. },
            ] if first == "first" && second == "second"
        ));
    }

    #[test]
    fn image_pasted_before_existing_marker_is_inserted_before_it() {
        let mut state = make_state();
        let mut input = InputBox::new();
        input.insert_text("右侧");
        apply_clipboard_paste(
            &mut state,
            &mut input,
            ClipboardPaste::Image {
                data: "right".to_string(),
                width: 320,
                height: 180,
            },
        );
        input.move_to_start_of_line();
        apply_clipboard_paste(
            &mut state,
            &mut input,
            ClipboardPaste::Image {
                data: "left".to_string(),
                width: 640,
                height: 480,
            },
        );

        assert!(matches!(
            state.pending_attachments.as_slice(),
            [
                Attachment::Image { data: left, .. },
                Attachment::Image { data: right, .. },
            ] if left == "left" && right == "right"
        ));
    }

    #[test]
    fn backspace_deletes_text_then_attachments_from_the_right() {
        let mut state = make_state();
        let mut input = InputBox::new();
        input.insert_text("x");
        state.pending_attachments = vec![
            Attachment::Image {
                media_type: "image/png".to_string(),
                data: "first".to_string(),
                preview_label: "1×1".to_string(),
            },
            Attachment::Image {
                media_type: "image/png".to_string(),
                data: "second".to_string(),
                preview_label: "2×2".to_string(),
            },
        ];

        backspace_composer(&mut state, &mut input);
        assert!(input.is_empty());
        assert_eq!(state.pending_attachments.len(), 2);

        backspace_composer(&mut state, &mut input);
        assert!(matches!(
            state.pending_attachments.as_slice(),
            [Attachment::Image { data, .. }] if data == "first"
        ));

        backspace_composer(&mut state, &mut input);
        assert!(state.pending_attachments.is_empty());
    }

    #[test]
    fn inline_image_is_deleted_only_when_backspace_reaches_its_position() {
        let mut state = make_state();
        let mut input = InputBox::new();
        input.insert_text("前");
        apply_clipboard_paste(
            &mut state,
            &mut input,
            ClipboardPaste::Image {
                data: "encoded-png".to_string(),
                width: 320,
                height: 180,
            },
        );
        input.insert_text("后");

        backspace_composer(&mut state, &mut input);
        assert_eq!(input.text(), "前");
        assert_eq!(state.pending_attachments.len(), 1);

        backspace_composer(&mut state, &mut input);
        assert_eq!(input.text(), "前");
        assert!(state.pending_attachments.is_empty());
    }

    #[test]
    fn delete_key_removes_an_inline_image_at_the_cursor() {
        let mut state = make_state();
        let mut input = InputBox::new();
        input.insert_text("前");
        apply_clipboard_paste(
            &mut state,
            &mut input,
            ClipboardPaste::Image {
                data: "encoded-png".to_string(),
                width: 320,
                height: 180,
            },
        );
        input.move_left();

        delete_composer_forward(&mut state, &mut input);

        assert_eq!(input.text(), "前");
        assert!(state.pending_attachments.is_empty());
    }

    #[test]
    fn sending_filters_internal_attachment_markers_from_text() {
        let mut state = make_state();
        let mut input = InputBox::new();
        input.insert_text("请看");
        apply_clipboard_paste(
            &mut state,
            &mut input,
            ClipboardPaste::Image {
                data: "encoded-png".to_string(),
                width: 320,
                height: 180,
            },
        );
        input.insert_text("这张图");

        assert_eq!(input.take(), "请看这张图");
        assert_eq!(state.pending_attachments.len(), 1);
    }

    // ── 长文本粘贴折叠（`[Pasted text #n +N lines]`）─────────────────────────

    /// 一段足够长的日志：行数与字符数都远超阈值。
    fn long_log() -> String {
        (1..=60)
            .map(|i| format!("2026-10-16 12:00:{i:02} INFO worker tick {i}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 用户报的核心症状：粘贴几百行后输入框被灌满、之后打的字完全看不见
    /// （`draw` 把输入框高度 clamp 到 10 行，`draw_input` 的 `Paragraph` 无
    /// 滚动且从第 0 行起画，第 9 行之后的内容永远画不出来）。折叠之后无论
    /// 贴进来多少行，输入框**恒为一行**。
    #[test]
    fn a_long_paste_never_grows_the_input_box() {
        let mut state = make_state();
        let mut input = InputBox::new();

        apply_clipboard_paste(&mut state, &mut input, ClipboardPaste::Text(long_log()));

        assert_eq!(
            input.visual_height(80),
            1,
            "折叠后的占位符必须只占一行，粘贴再多也不会灌满可视区"
        );
        assert_eq!(state.pending_pastes.len(), 1);
    }

    #[test]
    fn short_pastes_stay_inline_and_are_not_folded() {
        let mut state = make_state();
        let mut input = InputBox::new();
        let short = "见 docs/plan/abc.md 第 3 节".to_string();

        apply_clipboard_paste(&mut state, &mut input, ClipboardPaste::Text(short.clone()));

        assert!(state.pending_pastes.is_empty(), "短文本不该被折叠");
        assert_eq!(input.text(), short);
    }

    /// 两个阈值各自独立生效：字符数够就折叠，行数够也折叠。
    #[test]
    fn both_fold_thresholds_trigger_independently() {
        let mut state = make_state();
        // 4 行但很长
        let wide = "x".repeat(200);
        let mut input = InputBox::new();
        apply_clipboard_paste(&mut state, &mut input, ClipboardPaste::Text(wide));
        assert_eq!(state.pending_pastes.len(), 1, "≥200 字符即折叠，与行数无关");

        // 很多行但每行很短
        let mut state2 = make_state();
        let mut input2 = InputBox::new();
        let tall = (1..=6)
            .map(|i| format!("行{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        apply_clipboard_paste(&mut state2, &mut input2, ClipboardPaste::Text(tall));
        assert_eq!(state2.pending_pastes.len(), 1, "≥5 行即折叠，与字符数无关");
    }

    /// 折叠只是换个存法，**一个字符都不能少**。这是整个改动里最不能错的一条：
    /// `text()` 会把 `PASTE_MARKER` 过滤掉，误用 `take()` 就会把用户贴的整段
    /// 日志变成一条空消息。
    #[test]
    fn sending_expands_a_folded_paste_back_to_its_original_text() {
        let mut state = make_state();
        let mut input = InputBox::new();
        let log = long_log();
        input.insert_text("看看这个：");
        apply_clipboard_paste(&mut state, &mut input, ClipboardPaste::Text(log.clone()));
        input.insert_text("就这些");

        let (text, pastes) = take_composer_text(&mut state, &mut input);

        assert_eq!(text, format!("看看这个：{log}就这些"));
        assert_eq!(pastes.len(), 1);
        assert_eq!(pastes[0].text, log);
        assert_eq!(pastes[0].line_count, 60);
        assert!(state.pending_pastes.is_empty());
    }

    /// 占位符是**一个不可分割的原子**：退格要么删掉一个普通字符，要么删掉整个
    /// 占位符，绝不能停在 `asted text #1 +60 lines]` 这种半截残片上——那既
    /// 看不懂，也会让人误以为内容本来就这么点。逐次退格全程都不许出现半截。
    #[test]
    fn a_folded_paste_is_deleted_whole_never_leaving_a_half_placeholder() {
        let mut state = make_state();
        let mut input = InputBox::new();
        input.insert_text("前");
        apply_clipboard_paste(&mut state, &mut input, ClipboardPaste::Text(long_log()));
        input.insert_text("后");

        // 一次退格只能删掉「一个东西」：普通字符或**整个**占位符。
        for expected_pastes in [1, 0, 0] {
            backspace_composer(&mut state, &mut input);
            let text = input.text();
            assert!(
                !text.contains("Pasted") && !text.contains("lines]"),
                "退格留下了半截占位符：{text}"
            );
            assert_eq!(state.pending_pastes.len(), expected_pastes);
        }
        assert!(input.is_empty());
        assert!(state.pending_pastes.is_empty());
    }

    /// Delete 的语义基准点是「光标压在占位符上」：一次前进删除抹掉整个占位符，
    /// 而不是只削掉 `[` 或 `+60 lines]`。
    #[test]
    fn delete_key_removes_the_folded_paste_the_cursor_sits_on() {
        let mut state = make_state();
        let mut input = InputBox::new();
        input.insert_text("前");
        apply_clipboard_paste(&mut state, &mut input, ClipboardPaste::Text(long_log()));
        input.insert_text("后");
        // line = ['前', M, '后']，光标退到 index 1 正好压在占位符上。
        input.move_left();
        input.move_left();

        delete_composer_forward(&mut state, &mut input);

        assert_eq!(input.text(), "前后", "一次删除就抹掉整个占位符");
        assert!(state.pending_pastes.is_empty());
    }

    /// Alt+Backspace / Ctrl+W 的基准点是「光标紧跟在占位符后面」：此时删一个词
    /// 必须退化成删整个占位符，否则会留下 `[Pasted` 这种半截残片。
    #[test]
    fn word_delete_after_a_folded_paste_removes_the_whole_placeholder() {
        let mut state = make_state();
        let mut input = InputBox::new();
        input.insert_text("前");
        apply_clipboard_paste(&mut state, &mut input, ClipboardPaste::Text(long_log()));
        input.insert_text("后");
        input.move_left();

        delete_word_backward_composer(&mut state, &mut input);

        assert_eq!(input.text(), "前后");
        assert!(state.pending_pastes.is_empty());
    }

    /// 删掉中间那个占位符时，编号要跟着重排——否则剩下的会顶着已经
    /// 不存在的编号，用户点开一看少一块，还以为内容被吞了。
    #[test]
    fn deleting_the_middle_of_three_folded_pastes_renumbers_the_rest() {
        let mut state = make_state();
        let mut input = InputBox::new();
        for _ in 0..3 {
            apply_clipboard_paste(&mut state, &mut input, ClipboardPaste::Text(long_log()));
            input.insert_text("|");
        }
        // 三个粘贴块 + 三个分隔符共 6 个字符，光标在行尾（col 6）；
        // 退 4 格落在第二个占位符上（col 2）。
        for _ in 0..4 {
            input.move_left();
        }

        delete_composer_forward(&mut state, &mut input);

        assert_eq!(state.pending_pastes.len(), 2);
        let display = crate::render::composer_display_text(
            &input,
            &state.pending_attachments,
            &state.pending_pastes,
        );
        assert_eq!(
            display.matches("[Pasted text #1 +60 lines]").count(),
            1,
            "实际是：{display}"
        );
        assert_eq!(
            display.matches("[Pasted text #2 +60 lines]").count(),
            1,
            "剩下的必须重排成 #1/#2，实际是：{display}"
        );
    }

    /// 图与长文本可以任意交错，两类编号各自独立：中间插一个粘贴块不能让
    /// `[Image #2]` 跳成 `#3`，反之亦然。
    #[test]
    fn image_and_pasted_text_placeholders_are_numbered_independently() {
        let mut state = make_state();
        let mut input = InputBox::new();
        apply_clipboard_paste(
            &mut state,
            &mut input,
            ClipboardPaste::Image {
                data: "left".to_string(),
                width: 1,
                height: 1,
            },
        );
        apply_clipboard_paste(&mut state, &mut input, ClipboardPaste::Text(long_log()));
        apply_clipboard_paste(
            &mut state,
            &mut input,
            ClipboardPaste::Image {
                data: "right".to_string(),
                width: 2,
                height: 2,
            },
        );

        let display = crate::render::composer_display_text(
            &input,
            &state.pending_attachments,
            &state.pending_pastes,
        );
        assert!(display.contains("[Image #1]"), "{display}");
        assert!(display.contains("[Pasted text #1 +60 lines]"), "{display}");
        assert!(display.contains("[Image #2]"), "{display}");
    }

    // ── 剪贴板 flavor 判定：文件优先于图片 ───────────────────────────────

    /// 用户报的现象：粘贴文件一律变成 `[Image #n]`。根因是 macOS Finder 复制的
    /// 文件同时带 `public.file-url` 和 `public.tiff`（文件图标预览），而 arboard
    /// 的 `get_image()` 恰好读 TIFF。现在文件 flavor 先于图片被认领。
    #[test]
    fn clipboard_files_become_file_attachments_not_images() {
        let mut state = make_state();
        let mut input = InputBox::new();

        let hint = apply_clipboard_paste(
            &mut state,
            &mut input,
            ClipboardPaste::Files(vec![
                PathBuf::from("/tmp/report.pdf"),
                PathBuf::from("/tmp/notes.md"),
            ]),
        );

        assert!(hint.is_none());
        assert!(
            state
                .pending_attachments
                .iter()
                .all(|att| matches!(att, Attachment::File { .. })),
            "文件不能被当成图片，实际是 {:?}",
            state.pending_attachments
        );
        let display = crate::render::composer_display_text(
            &input,
            &state.pending_attachments,
            &state.pending_pastes,
        );
        assert!(display.contains("[File: report.pdf]"), "{display}");
        assert!(display.contains("[File: notes.md]"), "{display}");
    }

    #[test]
    fn duplicate_clipboard_files_are_ignored() {
        let mut state = make_state();
        let mut input = InputBox::new();
        let files = vec![PathBuf::from("/tmp/report.pdf")];

        apply_clipboard_paste(&mut state, &mut input, ClipboardPaste::Files(files.clone()));
        apply_clipboard_paste(&mut state, &mut input, ClipboardPaste::Files(files));

        assert_eq!(state.pending_attachments.len(), 1);
    }

    /// ↑ 翻出来的历史是展开后的完整正文，回填时若不重新折叠就重现
    /// 「输入框被灌满、看不见」的老症状。
    #[test]
    fn recalling_a_long_history_entry_refolds_it() {
        let mut state = make_state();
        let log = long_log();
        state
            .input_history
            .push(format!("帮我看下这段日志：\n{log}"));
        let mut input = InputBox::new();

        assert!(state.navigate_input_or_history(&mut input, -1));

        assert_eq!(input.visual_height(80), 1, "长历史回填后同样只占一行");
        assert_eq!(state.pending_pastes.len(), 1);
        let (text, _) = take_composer_text(&mut state, &mut input);
        assert_eq!(
            text,
            format!("帮我看下这段日志：\n{log}"),
            "原文必须完整回填"
        );
    }

    /// 附件和折叠的粘贴块必须**一起**清掉。只清一半的残留会让下一次粘贴的块
    /// 拿到错误序号（`[Pasted text #2]` 顶着已经删掉的 #1），而 `text_with_pastes`
    /// 按序号回填原文，对不上就是**发错内容**——不是显示瑕疵。
    #[test]
    fn interrupting_clears_attachments_and_folded_pastes_together() {
        let mut state = make_state();
        state.pending_attachments.push(Attachment::File {
            path: PathBuf::from("/tmp/a.txt"),
        });
        state.pending_pastes.push(PasteBlock {
            text: "残留原文".to_string(),
            line_count: 1,
        });

        state.interrupt();

        assert!(state.pending_attachments.is_empty());
        assert!(
            state.pending_pastes.is_empty(),
            "残留的粘贴块会把下一次粘贴的编号顶偏，并按错序号回填原文"
        );
    }

    #[test]
    fn clearing_composer_removes_text_and_attachments_together() {
        let mut state = make_state();
        let mut input = InputBox::new();
        input.insert_text("draft");
        state.pending_attachments.push(Attachment::Image {
            media_type: "image/png".to_string(),
            data: "encoded-png".to_string(),
            preview_label: "320×180".to_string(),
        });

        assert!(composer_has_content(&state, &input));
        clear_composer(&mut state, &mut input);

        assert!(input.is_empty());
        assert!(state.pending_attachments.is_empty());
        assert!(!composer_has_content(&state, &input));
    }

    #[test]
    fn attachment_only_composer_is_sendable() {
        let mut state = make_state();
        let input = InputBox::new();
        state.pending_attachments.push(Attachment::Image {
            media_type: "image/png".to_string(),
            data: "encoded-png".to_string(),
            preview_label: "320×180".to_string(),
        });

        assert!(composer_has_content(&state, &input));
    }

    #[tokio::test]
    async fn multiple_images_are_sent_as_ordered_content_blocks() {
        let attachments = vec![
            Attachment::Image {
                media_type: "image/png".to_string(),
                data: "first".to_string(),
                preview_label: "1×1".to_string(),
            },
            Attachment::Image {
                media_type: "image/jpeg".to_string(),
                data: "second".to_string(),
                preview_label: "2×2".to_string(),
            },
        ];

        let blocks = build_user_blocks("compare".to_string(), attachments).await;

        assert!(matches!(
            blocks.as_slice(),
            [
                ContentBlock::Text { text },
                ContentBlock::Image {
                    media_type: first_type,
                    data: first,
                },
                ContentBlock::Image {
                    media_type: second_type,
                    data: second,
                },
            ] if text == "compare"
                && first_type == "image/png"
                && first == "first"
                && second_type == "image/jpeg"
                && second == "second"
        ));
    }

    // ── 应用内文本选择 ────────────────────────────────────────────────

    fn row(y: u16, text: &str, start_x: u16) -> ScreenRow {
        let mut row = ScreenRow {
            y,
            ..Default::default()
        };
        for (i, ch) in text.chars().enumerate() {
            row.starts.push((start_x + i as u16, row.text.len()));
            row.text.push(ch);
        }
        row
    }

    fn sel(anchor: (u16, u16), cursor: (u16, u16)) -> TextSelection {
        TextSelection {
            area: SelectionArea::Chat,
            anchor,
            cursor,
            scroll_anchor: 0,
            dragging: false,
        }
    }

    /// 单行选区只取覆盖的列，两侧留白不带进来。
    #[test]
    fn selection_text_takes_only_the_covered_columns() {
        let rows = vec![row(0, "hello world", 0)];
        let text = selection_text(&sel((6, 0), (10, 0)), &rows, 0);
        assert_eq!(text, "world");
    }

    /// 跨行时首行从起点到行尾、末行从行首到终点，中间整行全取。
    #[test]
    fn selection_text_splits_lines_at_the_selection_edges() {
        let rows = vec![
            row(0, "alpha beta", 0),
            row(1, "gamma", 0),
            row(2, "delta", 0),
        ];
        let text = selection_text(&sel((6, 0), (3, 2)), &rows, 0);
        assert_eq!(text, "beta\ngamma\ndelt");
    }

    /// 从下往上拖与从上往下拖必须得到同一段文字。
    #[test]
    fn selection_text_is_direction_agnostic() {
        let rows = vec![row(0, "abc", 0), row(1, "def", 0)];
        let forward = selection_text(&sel((1, 0), (1, 1)), &rows, 0);
        let backward = selection_text(&sel((1, 1), (1, 0)), &rows, 0);
        assert_eq!(forward, backward);
        assert_eq!(forward, "bc\nde");
    }

    /// 行尾补白要裁掉：屏幕右侧的空格不是内容。
    #[test]
    fn selection_text_trims_trailing_padding() {
        let rows = vec![row(0, "hi        ", 0)];
        let text = selection_text(&sel((0, 0), (9, 0)), &rows, 0);
        assert_eq!(text, "hi");
    }

    /// 宽字符只登记起始列，落在续格上时顺延到下一格——绝不能切出半个字。
    #[test]
    fn selection_text_never_splits_a_wide_character() {
        // "中文" 在屏幕上占 0,1 / 2,3 四列，但快照里只有 (0,"中") 与 (2,"文")。
        let mut row = ScreenRow {
            y: 0,
            ..Default::default()
        };
        for (x, ch) in [(0u16, '中'), (2, '文')] {
            row.starts.push((x, row.text.len()));
            row.text.push(ch);
        }
        let rows = vec![row];

        // 终点落在 "中" 的续格（列 1）上：仍取完整的一个字。
        assert_eq!(selection_text(&sel((0, 0), (1, 0)), &rows, 0), "中");
        // 起点落在续格上：从下一个字开始，不产生乱码。
        assert_eq!(selection_text(&sel((1, 0), (3, 0)), &rows, 0), "文");
    }

    /// 选区左端不 trim：代码缩进是用户亲手点出来的边界，不能替他裁掉。
    #[test]
    fn selection_text_keeps_leading_indentation() {
        let rows = vec![row(0, "    let x = 1;", 0)];
        let text = selection_text(&sel((4, 0), (14, 0)), &rows, 0);
        assert_eq!(text, "let x = 1;");
    }

    /// 双击把路径/URL 当成一个词，而不是按 `/` `.` 切开。
    #[test]
    fn double_click_selects_a_whole_path() {
        let rows = vec![row(0, "see src/app.rs:123 now", 0)];
        let (start, end) = word_bounds_at(&rows, 7, 0); // 落在 "app"
        let text = selection_text(&sel(start, end), &rows, 0);
        assert_eq!(text, "src/app.rs:123");
    }

    /// 双击落在空白上退化成该格，不能变成空选区。
    #[test]
    fn double_click_on_whitespace_stays_on_that_cell() {
        let rows = vec![row(0, "a b", 0)];
        let (start, end) = word_bounds_at(&rows, 1, 0);
        assert_eq!(start, end);
    }

    /// 拖拽必须被夹在起手区域内：从内容区起手时，鼠标滑到输入框上不能把
    /// 输入框选进来。
    #[test]
    fn drag_is_clamped_to_the_starting_area() {
        let mut state = make_state();
        state.chat_area = ratatui::layout::Rect::new(0, 0, 40, 10);
        state.selection = Some(TextSelection::new(SelectionArea::Chat, (2, 2), 0));

        state.update_selection(100, 100);
        let sel = state.selection.unwrap();
        assert_eq!(sel.cursor, (39, 9));

        state.update_selection(0, 0);
        assert_eq!(state.selection.unwrap().cursor, (0, 0));
    }

    /// 输入框里起手的拖选夹在输入框正文区内，不会把聊天区一起选中。
    #[test]
    fn composer_drag_stays_inside_the_composer() {
        let mut state = make_state();
        state.chat_area = ratatui::layout::Rect::new(0, 0, 40, 10);
        state.composer_area = ratatui::layout::Rect::new(0, 20, 40, 3);
        state.selection = Some(TextSelection::new(SelectionArea::Composer, (5, 21), 0));

        state.update_selection(5, 0);
        assert_eq!(state.selection.unwrap().cursor, (5, 20));

        state.update_selection(5, 100);
        assert_eq!(state.selection.unwrap().cursor, (5, 22));
    }

    /// 内容区之外按下左键不建立选区（状态栏里点击不该出现高亮）。
    #[test]
    fn click_outside_the_selectable_areas_does_not_start_a_selection() {
        let mut state = make_state();
        state.chat_area = ratatui::layout::Rect::new(0, 0, 40, 10);
        state.composer_area = ratatui::layout::Rect::new(0, 20, 40, 3);
        state.begin_selection(5, 30, &[]);
        assert!(state.selection.is_none());
    }

    /// 输入框内按下要返回目标文本位置，供主循环把光标移过去。
    #[test]
    fn clicking_the_composer_reports_the_target_position() {
        let mut state = make_state();
        state.chat_area = ratatui::layout::Rect::new(0, 0, 40, 10);
        state.composer_area = ratatui::layout::Rect::new(0, 20, 40, 3);
        assert_eq!(state.begin_selection(7, 21, &[]), Some((7, 21)));
        assert_eq!(state.selection.unwrap().area, SelectionArea::Composer);
    }

    /// 有浮层时不允许应用内拖选：`frame_rows` 抓的是叠加后的画面，
    /// 拖出来的会是面板边框与提示文字。
    #[test]
    fn modal_overlay_blocks_selection() {
        let mut state = make_state();
        state.chat_area = ratatui::layout::Rect::new(0, 0, 40, 10);
        state.session_picker = Some(SessionPickerState {
            sessions: Vec::new(),
            selected: 0,
        });
        state.begin_selection(5, 5, &[]);
        assert!(state.selection.is_none());
    }

    /// 空白拖拽（按下就松）不产生选区，也不该尝试写剪贴板。
    #[test]
    fn a_click_without_drag_leaves_no_selection() {
        let mut state = make_state();
        state.chat_area = ratatui::layout::Rect::new(0, 0, 40, 10);
        state.begin_selection(5, 5, &[]);
        assert!(state.selection.is_some());
        state.end_selection(&[]);
        assert!(state.selection.is_none());
    }

    /// 滚动之后选区跟着文字走：滚过 `delta` 行，高亮区间整体上移同样的行数。
    #[test]
    fn selection_follows_the_content_when_scrolled() {
        let mut state = make_state();
        state.chat_area = ratatui::layout::Rect::new(0, 0, 40, 10);
        state.chat_max_scroll = 100;
        state.selection = Some(TextSelection::new(SelectionArea::Chat, (2, 4), 0));

        state.chat_scroll = 3;
        let sel = state.selection.unwrap();
        assert_eq!(state.selection_scroll_delta(&sel), 3);
        let ((_, sy), _) = sel.shifted(3);
        assert_eq!(sy, 1, "内容上移 3 行，选区跟着上移到第 1 行");
    }

    /// 滚动条几何换算：滑块顶部 → 偏移，且两端能取到 0 与 max。
    #[test]
    fn scrollbar_metrics_map_thumb_position_to_offset() {
        let metrics = ScrollbarMetrics {
            area: ratatui::layout::Rect::new(79, 0, 1, 20),
            thumb_top: 0,
            thumb_len: 5,
            max_scroll: 60,
            target: ScrollbarTarget::Chat,
        };
        assert_eq!(metrics.scroll_for_thumb_top(0), 0);
        // 行程 = 20 - 5 = 15，滑块到底 = 60。
        assert_eq!(metrics.scroll_for_thumb_top(15), 60);
        // 中途按比例。
        assert_eq!(metrics.scroll_for_thumb_top(5), 20);
    }

    /// 轨道比滑块还短时（内容刚好装下）拖动不产生滚动。
    #[test]
    fn scrollbar_metrics_are_inert_when_content_fits() {
        let metrics = ScrollbarMetrics {
            area: ratatui::layout::Rect::new(79, 0, 1, 5),
            thumb_top: 0,
            thumb_len: 5,
            max_scroll: 0,
            target: ScrollbarTarget::Chat,
        };
        assert_eq!(metrics.scroll_for_thumb_top(3), 0);
    }

    /// 宽字符的续格不能变成空格混进复制结果。
    ///
    /// 现场：在聊天区框选一段中文复制，粘到别处后每个汉字之间都多一个空格
    /// （`中 文 abc`），看上去像是复制时给每个字加了间距。根因在
    /// `capture_area_rows`：ratatui 把宽字符写在第 1 格，把紧随其后的续格
    /// `Cell::reset()` 成 `" "`（`Cell::EMPTY` 的 symbol 就是空格，不是空串），
    /// 而快照当时是**逐列**扫的，于是每个 CJK 字符后面那个占位格都被当成
    /// 一个真实空格搬进了 `text`。现在按 symbol 的显示宽度推进，续格根本
    /// 不会被访问。
    #[test]
    fn frame_capture_never_turns_wide_char_padding_into_spaces() {
        use ratatui::widgets::{Paragraph, Widget};

        let mut state = make_state();
        state.show_scrollbar = false;
        state.chat_area = ratatui::layout::Rect::new(0, 0, 20, 1);
        state.composer_area = ratatui::layout::Rect::default();

        let mut buffer = ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, 20, 1));
        Paragraph::new("中文 abc").render(buffer.area, &mut buffer);
        state.capture_frame_rows(&buffer);

        // 快照按「每格一个字符」记：宽字符只登记起始列，续格不登记。
        // 偏移是字节偏移（`中` 占 3 字节），所以 0 → 3 → 6 之后才逐字 +1。
        assert_eq!(
            state.frame_rows[0].starts[..4],
            [(0, 0), (2, 3), (4, 6), (5, 7)],
            "宽字符只占一个 start 条目"
        );
        assert_eq!(
            selection_text(&sel((0, 0), (8, 0)), &state.frame_rows, 0),
            "中文 abc",
            "复制结果不能带出行间填充空格"
        );
    }

    /// 快照只在滚动条**真的画出来**时跳过最右列。
    ///
    /// 内容装得下时滚动条不画，那一列就是正文——无条件跳过会让最右一个字符
    /// 永远复制不出来（`[ui].scrollbar = true` 只是「允许画」，不是「画了」）。
    #[test]
    fn frame_capture_keeps_the_last_column_when_no_scrollbar_is_drawn() {
        let mut state = make_state();
        state.show_scrollbar = true;
        state.chat_area = ratatui::layout::Rect::new(0, 0, 10, 2);
        state.content_scrollbar = None;

        let buffer = ratatui::buffer::Buffer::with_lines(["abcdefghij", "klmnopqrst"]);
        state.capture_frame_rows(&buffer);
        assert!(
            state.frame_rows.iter().any(|r| r.text.contains('j')),
            "没画滚动条时最右列属于正文: {:?}",
            state.frame_rows.iter().map(|r| &r.text).collect::<Vec<_>>()
        );

        // 画了滚动条：最右列是装饰，必须排除。
        state.content_scrollbar = Some(ScrollbarMetrics {
            area: ratatui::layout::Rect::new(9, 0, 1, 2),
            thumb_top: 0,
            thumb_len: 2,
            max_scroll: 0,
            target: ScrollbarTarget::Chat,
        });
        state.capture_frame_rows(&buffer);
        assert!(
            state.frame_rows.iter().all(|r| !r.text.contains('j')),
            "画了滚动条时最右列不参与选文"
        );
    }

    /// 快照跨帧复用容量时不能残留上一帧的行。
    ///
    /// 两段区域（内容区 + 输入框）各写一段，行数随窗口大小变化；如果只在末尾
    /// `truncate` 而没清旧内容，缩小的那一帧会把上一帧的尾部行留在快照里，
    /// 复制时凭空多出几行已经不在屏幕上的文字。
    #[test]
    fn frame_capture_does_not_leak_rows_from_a_larger_previous_frame() {
        let mut state = make_state();
        state.show_scrollbar = false;
        state.chat_area = ratatui::layout::Rect::new(0, 0, 5, 4);
        state.composer_area = ratatui::layout::Rect::default();

        let tall = ratatui::buffer::Buffer::with_lines(["aaaaa", "bbbbb", "ccccc", "ddddd"]);
        state.capture_frame_rows(&tall);
        assert_eq!(state.frame_rows.len(), 4);

        // 窗口变矮：只剩两行，旧的两行必须消失。
        state.chat_area = ratatui::layout::Rect::new(0, 0, 5, 2);
        state.capture_frame_rows(&tall);
        assert_eq!(state.frame_rows.len(), 2);
        assert_eq!(state.frame_rows[0].text, "aaaaa");
        assert_eq!(state.frame_rows[1].text, "bbbbb");
    }
}
