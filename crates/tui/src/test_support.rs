//! 测试脚手架：造状态 / 造消息 / 把一帧渲染成纯文本。
//!
//! 单独成文件是因为渲染测试散落在 `render.rs` 内部断言与 `golden_tests.rs`
//! 整帧快照两处，两边各自复刻一份迟早漂移——而漂移之后的快照比对等于
//! 在跟一个不存在的东西对比。

use std::path::PathBuf;
use std::sync::Arc;

use ratatui::backend::TestBackend;
use ratatui::text::Line;
use ratatui::{Frame, Terminal};

use crate::app::{AppState, ChatMessage, MessageRole};
use wyj_config::AgentMode;

/// 一个最小可渲染的 `AppState`（无 session、无后台任务）。
pub(crate) fn make_state() -> AppState {
    AppState::new(
        PathBuf::from("/tmp"),
        "test-model".to_string(),
        200_000,
        AgentMode::Auto,
        wyj_config::Config::default(),
        Arc::new(wyj_tools::SubAgentHub::new()),
    )
}

pub(crate) fn message(role: MessageRole, content: &str) -> ChatMessage {
    ChatMessage {
        id: 1,
        role,
        content: content.to_string(),
        is_error: false,
        elapsed_secs: None,
        thinking_elapsed_secs: None,
        sequence_no: None,
        tool_name: None,
        display_summary: String::new(),
        sub_agent_id: None,
        md_cache: std::cell::RefCell::new(None),
    }
}

/// `Vec<Line>` 拼成每行一条的纯文本（丢弃样式）。
pub(crate) fn rendered_text(lines: Vec<Line<'static>>) -> Vec<String> {
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

/// 渲染一屏到 TestBackend 并按 cell 拼成纯文本（**含尾随空格**，适合光标/边框断言）。
pub(crate) fn render_to_text(f: impl FnOnce(&mut Frame), width: u16, height: u16) -> String {
    frame_cells(f, width, height).join("\n")
}

fn frame_cells(f: impl FnOnce(&mut Frame), width: u16, height: u16) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("TestBackend init");
    terminal.draw(f).expect("draw should not panic");
    let buf = terminal.backend().buffer();
    (0..buf.area.height)
        .map(|y| {
            (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
}
