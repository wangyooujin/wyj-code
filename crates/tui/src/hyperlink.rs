//! 终端超链接支持。
//!
//! ratatui 的 `Cell` 没有原生 hyperlink 元数据；直接把 OSC 8 转义写进
//! `Cell::symbol` 又会污染 Buffer 的宽度计算。这里分成两层：
//!
//! 1. 渲染完成后扫描聊天区，把 URL / 已存在的本地路径登记到坐标表，并只在
//!    Buffer 中追加下划线样式；
//! 2. 自定义 Crossterm backend 在 `Terminal` 已经完成 Buffer diff 之后，才给将要
//!    输出的 Cell 包上 OSC 8。这样换行、宽字符和增量重绘都仍使用干净文本计算。

use ratatui::{
    backend::{Backend, ClearType, CrosstermBackend, WindowSize},
    buffer::{Buffer, Cell},
    layout::{Position, Rect, Size},
    style::{Color, Modifier, Style},
};
use std::{
    collections::HashMap,
    io::{self, Write},
    ops::Range,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, RwLock},
};
use unicode_width::UnicodeWidthStr;

const OSC8_OPEN: &str = "\x1b]8;;";
const OSC8_CLOSE: &str = "\x1b]8;;\x1b\\";
const OSC_TERMINATOR: &str = "\x1b\\";

/// 屏幕坐标到安全 URI 的当前帧映射。Backend 与事件循环共享同一份快照：
/// 终端支持 OSC 8 时由终端原生处理 Command/Ctrl+点击；终端把修饰键点击继续上报
/// 给应用时，事件循环也可以用同一目标兜底打开。
#[derive(Clone, Default)]
pub(crate) struct HyperlinkRegistry {
    cells: Arc<RwLock<HashMap<(u16, u16), String>>>,
}

impl HyperlinkRegistry {
    fn replace(&self, cells: HashMap<(u16, u16), String>) {
        if let Ok(mut current) = self.cells.write() {
            *current = cells;
        }
    }

    fn snapshot(&self) -> HashMap<(u16, u16), String> {
        self.cells
            .read()
            .map(|cells| cells.clone())
            .unwrap_or_default()
    }

    pub(crate) fn target_at(&self, x: u16, y: u16) -> Option<String> {
        self.cells
            .read()
            .ok()
            .and_then(|cells| cells.get(&(x, y)).cloned())
    }
}

/// 在输出阶段注入 OSC 8 的 Crossterm backend。
pub(crate) struct HyperlinkBackend<W: Write> {
    inner: CrosstermBackend<W>,
    links: HyperlinkRegistry,
}

impl<W: Write> HyperlinkBackend<W> {
    pub(crate) fn new(writer: W, links: HyperlinkRegistry) -> Self {
        Self {
            inner: CrosstermBackend::new(writer),
            links,
        }
    }
}

impl<W: Write> Write for HyperlinkBackend<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Write::flush(&mut self.inner)
    }
}

impl<W: Write> Backend for HyperlinkBackend<W> {
    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        // 此处发生在 Terminal 的 Buffer::diff 之后，因此 OSC 8 不会参与 cell 宽度、
        // 换行或无效区域计算。
        let links = self.links.snapshot();
        let cells = content
            .map(|(x, y, cell)| {
                let mut rendered = cell.clone();
                if let Some(target) = links.get(&(x, y)) {
                    let symbol = format!(
                        "{OSC8_OPEN}{target}{OSC_TERMINATOR}{}{OSC8_CLOSE}",
                        cell.symbol()
                    );
                    rendered.set_symbol(&symbol);
                }
                (x, y, rendered)
            })
            .collect::<Vec<_>>();
        self.inner
            .draw(cells.iter().map(|(x, y, cell)| (*x, *y, cell)))
    }

    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.inner.append_lines(n)
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.inner.set_cursor_position(position)
    }

    fn clear(&mut self) -> io::Result<()> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> io::Result<Size> {
        self.inner.size()
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> io::Result<()> {
        Backend::flush(&mut self.inner)
    }

    fn scroll_region_up(
        &mut self,
        region: std::ops::Range<u16>,
        line_count: u16,
    ) -> io::Result<()> {
        self.inner.scroll_region_up(region, line_count)
    }

    fn scroll_region_down(
        &mut self,
        region: std::ops::Range<u16>,
        line_count: u16,
    ) -> io::Result<()> {
        self.inner.scroll_region_down(region, line_count)
    }
}

#[derive(Debug)]
struct VisibleCell {
    x: u16,
    y: u16,
    bytes: Range<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DetectedLink {
    bytes: Range<usize>,
    target: String,
}

/// 扫描最终聊天区 Buffer，给 URL 和已存在的本地文件/目录建立可点击坐标。
/// 连续两行中，前一行占满区域宽度时视为 Paragraph 自动换行，不插入逻辑换行，
/// 因而超长 URL / 路径跨视觉行后仍共享完整目标。
pub(crate) fn linkify_buffer(
    buffer: &mut Buffer,
    area: Rect,
    cwd: &Path,
    registry: &HyperlinkRegistry,
) {
    let area = buffer.area.intersection(area);
    if area.is_empty() {
        registry.replace(HashMap::new());
        return;
    }

    let (text, visible_cells) = visible_text(buffer, area);
    let links = detect_links(&text, cwd);
    let mut targets = HashMap::new();
    let link_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::UNDERLINED);

    for link in links {
        for cell in visible_cells
            .iter()
            .filter(|cell| ranges_overlap(&cell.bytes, &link.bytes))
        {
            buffer[(cell.x, cell.y)].set_style(link_style);
            targets.insert((cell.x, cell.y), link.target.clone());
        }
    }
    registry.replace(targets);
}

fn visible_text(buffer: &Buffer, area: Rect) -> (String, Vec<VisibleCell>) {
    let mut text = String::new();
    let mut visible_cells = Vec::new();

    for y in area.top()..area.bottom() {
        let mut row = String::new();
        let mut row_cells = Vec::new();
        let mut x = area.left();
        let mut last_non_blank = 0usize;
        let mut reaches_right = false;

        while x < area.right() {
            let symbol = buffer[(x, y)].symbol();
            let width = UnicodeWidthStr::width(symbol).max(1) as u16;
            let start = row.len();
            row.push_str(symbol);
            let end = row.len();
            row_cells.push((x, start..end));
            if !symbol.chars().all(char::is_whitespace) {
                last_non_blank = row_cells.len();
                reaches_right = x.saturating_add(width) >= area.right();
            }
            x = x.saturating_add(width.max(1));
        }

        row_cells.truncate(last_non_blank);
        let keep_bytes = row_cells.last().map(|(_, bytes)| bytes.end).unwrap_or(0);
        row.truncate(keep_bytes);
        let row_offset = text.len();
        text.push_str(&row);
        visible_cells.extend(row_cells.into_iter().map(|(x, bytes)| VisibleCell {
            x,
            y,
            bytes: (bytes.start + row_offset)..(bytes.end + row_offset),
        }));

        if !reaches_right {
            text.push('\n');
        }
    }

    (text, visible_cells)
}

fn detect_links(text: &str, cwd: &Path) -> Vec<DetectedLink> {
    let mut links = detect_web_links(text);

    for link in detect_delimited_file_links(text, cwd) {
        if !links
            .iter()
            .any(|existing| ranges_overlap(&existing.bytes, &link.bytes))
        {
            links.push(link);
        }
    }

    for token in path_token_ranges(text) {
        if links.iter().any(|link| ranges_overlap(&link.bytes, &token)) {
            continue;
        }
        let raw = &text[token.clone()];
        let Some((trimmed, leading_bytes, trailing_bytes)) = trim_path_token(raw) else {
            continue;
        };
        let range = (token.start + leading_bytes)..(token.end - trailing_bytes);
        if links.iter().any(|link| ranges_overlap(&link.bytes, &range)) {
            continue;
        }
        if let Some(target) = local_file_target(trimmed, cwd) {
            links.push(DetectedLink {
                bytes: range,
                target,
            });
        }
    }

    links.sort_by_key(|link| link.bytes.start);
    links
}

/// Markdown 行内代码、Markdown 链接目标和普通引号中的路径允许包含空格。
/// Buffer 扫描已经丢失 pulldown-cmark 的 Tag 元数据，因此在普通 token 扫描前
/// 单独恢复这些明确边界内的本地路径。
fn detect_delimited_file_links(text: &str, cwd: &Path) -> Vec<DetectedLink> {
    let mut links = Vec::new();
    for (open, close) in [('`', '`'), ('"', '"'), ('\'', '\''), ('(', ')')] {
        let mut offset = 0usize;
        while let Some(open_rel) = text[offset..].find(open) {
            let open_at = offset + open_rel;
            let content_start = open_at + open.len_utf8();
            let Some(close_rel) = text[content_start..].find(close) else {
                break;
            };
            let close_at = content_start + close_rel;
            let raw = &text[content_start..close_at];
            let leading = raw.len() - raw.trim_start().len();
            let trailing = raw.len() - raw.trim_end().len();
            let start = content_start + leading;
            let end = close_at.saturating_sub(trailing);
            if start < end && !text[start..end].contains('\n') {
                if let Some(target) = local_file_target(&text[start..end], cwd) {
                    links.push(DetectedLink {
                        bytes: start..end,
                        target,
                    });
                }
            }
            offset = close_at + close.len_utf8();
        }
    }
    links
}

fn detect_web_links(text: &str) -> Vec<DetectedLink> {
    let mut links = Vec::new();
    for (prefix, implied_https) in [
        ("https://", false),
        ("http://", false),
        ("file://", false),
        ("www.", true),
    ] {
        let mut offset = 0usize;
        while let Some(found) = text[offset..].find(prefix) {
            let start = offset + found;
            if start > 0
                && text[..start]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_alphanumeric() || matches!(c, '_' | '-'))
            {
                offset = start + prefix.len();
                continue;
            }
            let mut end = start + prefix.len();
            for (rel, ch) in text[end..].char_indices() {
                if is_url_terminator(ch) {
                    break;
                }
                end = start + prefix.len() + rel + ch.len_utf8();
            }
            end = trim_url_end(text, start, end);
            if end <= start + prefix.len() {
                offset = start + prefix.len();
                continue;
            }
            let displayed = &text[start..end];
            let target = if implied_https {
                format!("https://{displayed}")
            } else {
                displayed.to_string()
            };
            if is_safe_target(&target) {
                links.push(DetectedLink {
                    bytes: start..end,
                    target,
                });
            }
            offset = end;
        }
    }
    links.sort_by(|a, b| {
        a.bytes
            .start
            .cmp(&b.bytes.start)
            .then_with(|| b.bytes.len().cmp(&a.bytes.len()))
    });
    let mut deduplicated: Vec<DetectedLink> = Vec::new();
    for link in links {
        if !deduplicated
            .iter()
            .any(|existing| ranges_overlap(&existing.bytes, &link.bytes))
        {
            deduplicated.push(link);
        }
    }
    deduplicated
}

fn is_url_terminator(ch: char) -> bool {
    ch.is_whitespace()
        || matches!(
            ch,
            '<' | '>' | '"' | '\'' | '`' | '，' | '。' | '；' | '：' | '！' | '？' | '、'
        )
}

fn trim_url_end(text: &str, start: usize, mut end: usize) -> usize {
    loop {
        let Some((idx, ch)) = text[start..end].char_indices().next_back() else {
            return end;
        };
        let absolute = start + idx;
        let always_trim = matches!(ch, '.' | ',' | ';' | ':' | '!' | '?');
        let unbalanced_closer = match ch {
            ')' => text[start..end].matches(')').count() > text[start..end].matches('(').count(),
            ']' => text[start..end].matches(']').count() > text[start..end].matches('[').count(),
            '}' => text[start..end].matches('}').count() > text[start..end].matches('{').count(),
            _ => false,
        };
        if always_trim || unbalanced_closer {
            end = absolute;
        } else {
            return end;
        }
    }
}

fn path_token_ranges(text: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut start = None;
    for (idx, ch) in text.char_indices() {
        if is_path_separator(ch) {
            if let Some(token_start) = start.take() {
                ranges.push(token_start..idx);
            }
        } else if start.is_none() {
            start = Some(idx);
        }
    }
    if let Some(token_start) = start {
        ranges.push(token_start..text.len());
    }
    ranges
}

fn is_path_separator(ch: char) -> bool {
    ch.is_whitespace()
        || matches!(
            ch,
            '"' | '\''
                | '`'
                | '<'
                | '>'
                | '|'
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '，'
                | '。'
                | '；'
                | '：'
                | '！'
                | '？'
                | '、'
                | '│'
        )
}

/// 返回：(去除包裹标点后的 token, 左侧去除字节数, 右侧去除字节数)。
fn trim_path_token(raw: &str) -> Option<(&str, usize, usize)> {
    let mut start = 0usize;
    let mut end = raw.len();
    while let Some((idx, ch)) = raw[start..end].char_indices().next() {
        if matches!(ch, '-' | '•' | '❯') {
            start += idx + ch.len_utf8();
        } else {
            break;
        }
    }
    while let Some((idx, ch)) = raw[start..end].char_indices().next_back() {
        if matches!(ch, '.' | ',' | ';' | '!' | '?') {
            end = start + idx;
        } else {
            break;
        }
    }
    (start < end).then(|| (&raw[start..end], start, raw.len() - end))
}

fn local_file_target(displayed: &str, cwd: &Path) -> Option<String> {
    if displayed.contains("://") || displayed.len() > 4096 {
        return None;
    }
    let (path_text, line, column) = split_path_position(displayed);
    if path_text.is_empty() || !looks_path_like(path_text) {
        return None;
    }
    let path = resolve_local_path(path_text, cwd)?;
    if !path.exists() {
        return None;
    }
    Some(file_uri(&path, line, column))
}

fn looks_path_like(path: &str) -> bool {
    let explicit_path = path.starts_with('/')
        || path.starts_with("~/")
        || path.starts_with("./")
        || path.starts_with("../")
        || path.contains('/')
        || path.contains('\\');
    if explicit_path {
        return true;
    }
    if matches!(
        path,
        "Cargo.toml" | "Cargo.lock" | "README" | "README.md" | "Makefile" | "Dockerfile"
    ) {
        return true;
    }
    Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(is_common_file_extension)
}

fn is_common_file_extension(extension: &str) -> bool {
    matches!(
        extension.to_ascii_lowercase().as_str(),
        "c" | "cc"
            | "conf"
            | "config"
            | "cpp"
            | "cs"
            | "css"
            | "csv"
            | "docx"
            | "fish"
            | "go"
            | "h"
            | "hpp"
            | "html"
            | "ini"
            | "java"
            | "js"
            | "json"
            | "jsx"
            | "lock"
            | "md"
            | "pdf"
            | "py"
            | "rb"
            | "rs"
            | "scss"
            | "sh"
            | "sql"
            | "toml"
            | "ts"
            | "tsx"
            | "txt"
            | "xml"
            | "yaml"
            | "yml"
            | "zsh"
    )
}

fn split_path_position(path: &str) -> (&str, Option<u32>, Option<u32>) {
    if let Some(hash) = path.rfind("#L") {
        let suffix = &path[hash + 2..];
        let (line_text, column) = suffix
            .split_once('C')
            .map_or((suffix, None), |(line, col)| (line, col.parse().ok()));
        if let Ok(line) = line_text.parse::<u32>() {
            return (&path[..hash], Some(line), column);
        }
    }

    if let Some((before_last, last)) = split_numeric_suffix(path) {
        if let Some((file, line)) = split_numeric_suffix(before_last) {
            return (file, Some(line), Some(last));
        }
        return (before_last, Some(last), None);
    }
    (path, None, None)
}

fn split_numeric_suffix(path: &str) -> Option<(&str, u32)> {
    let colon = path.rfind(':')?;
    let suffix = &path[colon + 1..];
    if suffix.is_empty() || !suffix.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let value = suffix.parse().ok()?;
    Some((&path[..colon], value))
}

fn resolve_local_path(displayed: &str, cwd: &Path) -> Option<PathBuf> {
    let expanded = if displayed == "~" {
        home_dir()?
    } else if let Some(rest) = displayed.strip_prefix("~/") {
        home_dir()?.join(rest)
    } else {
        PathBuf::from(displayed)
    };
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    };
    Some(normalize_path(&absolute))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() && !normalized.has_root() {
                    normalized.push("..");
                }
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

fn file_uri(path: &Path, line: Option<u32>, column: Option<u32>) -> String {
    let mut display = path.to_string_lossy().replace('\\', "/");
    if !display.starts_with('/') {
        display.insert(0, '/');
    }
    let mut uri = format!("file://{}", percent_encode_path(&display));
    if let Some(line) = line {
        uri.push_str(&format!("#L{line}"));
        if let Some(column) = column {
            uri.push_str(&format!("C{column}"));
        }
    }
    uri
}

fn percent_encode_path(path: &str) -> String {
    let mut encoded = String::with_capacity(path.len());
    for byte in path.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(*byte, b'-' | b'_' | b'.' | b'~' | b'/' | b':')
        {
            encoded.push(*byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn ranges_overlap(a: &Range<usize>, b: &Range<usize>) -> bool {
    a.start < b.end && b.start < a.end
}

fn is_safe_target(target: &str) -> bool {
    target.len() <= 8192
        && matches!(
            target.strip_prefix("https://")
                .or_else(|| target.strip_prefix("http://"))
                .or_else(|| target.strip_prefix("file://")),
            Some(rest) if !rest.is_empty()
        )
        && !target.chars().any(char::is_control)
}

/// 终端未自行消费修饰键点击时的应用侧兜底打开。仅接受本模块生成/认可的
/// `http(s)://` 与 `file://` 目标，不把模型文本当作 shell 命令解释。
pub(crate) fn open_target(target: &str) -> io::Result<()> {
    if !is_safe_target(target) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unsupported hyperlink target",
        ));
    }

    #[cfg(target_os = "macos")]
    let mut command = {
        let mut cmd = Command::new("open");
        cmd.arg(target);
        cmd
    };
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut cmd = Command::new("rundll32.exe");
        cmd.args(["url.dll,FileProtocolHandler", target]);
        cmd
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = {
        let mut cmd = Command::new("xdg-open");
        cmd.arg(target);
        cmd
    };
    #[cfg(not(any(unix, target_os = "windows")))]
    return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "opening hyperlinks is unsupported on this platform",
    ));

    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_web_links_and_existing_files_with_line_positions() {
        let dir = tempfile::tempdir().unwrap();
        let source_dir = dir.path().join("src");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(source_dir.join("main.rs"), "fn main() {}\n").unwrap();

        let text = "See https://example.com/docs, then src/main.rs:12:3.";
        let links = detect_links(text, dir.path());

        assert_eq!(links.len(), 2);
        assert_eq!(links[0].target, "https://example.com/docs");
        assert!(links[1].target.starts_with("file://"));
        assert!(links[1].target.ends_with("/src/main.rs#L12C3"));
    }

    #[test]
    fn ignores_slash_terms_and_missing_paths() {
        let dir = tempfile::tempdir().unwrap();
        let links = detect_links("Todo/SubAgent and missing/file.rs", dir.path());
        assert!(links.is_empty());
    }

    #[test]
    fn detects_backtick_path_with_spaces() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("docs/My Report.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "report\n").unwrap();

        let links = detect_links("open `docs/My Report.md:9`", dir.path());
        assert_eq!(links.len(), 1);
        assert!(links[0].target.ends_with("/docs/My%20Report.md#L9"));
    }

    #[test]
    fn http_url_with_www_is_not_overridden_by_nested_detection() {
        let links = detect_links("http://www.example.com/docs", Path::new("/tmp"));
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target, "http://www.example.com/docs");
    }

    #[test]
    fn wrapped_web_link_keeps_one_complete_target() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 2));
        buffer.set_string(0, 0, "  https://ex", Style::default());
        buffer.set_string(0, 1, "ample.com", Style::default());
        let registry = HyperlinkRegistry::default();

        linkify_buffer(
            &mut buffer,
            Rect::new(0, 0, 12, 2),
            Path::new("/tmp"),
            &registry,
        );

        assert_eq!(
            registry.target_at(2, 0).as_deref(),
            Some("https://example.com")
        );
        assert_eq!(
            registry.target_at(0, 1).as_deref(),
            Some("https://example.com")
        );
    }

    #[test]
    fn linkify_marks_file_cells_without_changing_symbols() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("README.md"), "hello\n").unwrap();
        let mut buffer = Buffer::empty(Rect::new(0, 0, 30, 1));
        buffer.set_string(0, 0, "open README.md", Style::default());
        let registry = HyperlinkRegistry::default();

        linkify_buffer(&mut buffer, Rect::new(0, 0, 30, 1), dir.path(), &registry);

        assert_eq!(buffer[(5, 0)].symbol(), "R");
        assert!(buffer[(5, 0)].modifier.contains(Modifier::UNDERLINED));
        assert!(registry
            .target_at(5, 0)
            .is_some_and(|target| target.ends_with("/README.md")));
    }

    #[test]
    fn file_uri_percent_encodes_spaces() {
        let uri = file_uri(Path::new("/tmp/a folder/main.rs"), Some(7), None);
        assert_eq!(uri, "file:///tmp/a%20folder/main.rs#L7");
    }

    #[test]
    fn backend_injects_osc8_after_buffer_diff() {
        #[derive(Clone, Default)]
        struct SharedWriter(Arc<std::sync::Mutex<Vec<u8>>>);

        impl Write for SharedWriter {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let registry = HyperlinkRegistry::default();
        registry.replace(HashMap::from([((0, 0), "https://example.com".to_string())]));
        let writer = SharedWriter::default();
        let output = writer.0.clone();
        let mut backend = HyperlinkBackend::new(writer, registry);
        let cell = Cell::new("x");

        backend.draw(std::iter::once((0, 0, &cell))).unwrap();
        let output = output.lock().unwrap();
        let output = String::from_utf8_lossy(&output);
        assert!(output.contains("\x1b]8;;https://example.com\x1b\\x\x1b]8;;\x1b\\"));
    }
}
