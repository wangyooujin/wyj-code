//! 色彩主题（暗色系）

use ratatui::style::{Color, Modifier, Style};

pub struct Theme;

impl Theme {
    pub fn user_prefix() -> Style {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    }
    pub fn assistant_prefix() -> Style {
        Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
    }
    pub fn tool_call() -> Style {
        Style::default().fg(Color::Yellow)
    }
    pub fn tool_result() -> Style {
        Style::default().fg(Color::Blue)
    }
    pub fn error() -> Style {
        Style::default().fg(Color::Red)
    }
    pub fn dim() -> Style {
        Style::default().fg(Color::DarkGray)
    }
    pub fn border() -> Style {
        Style::default().fg(Color::DarkGray)
    }
    pub fn status_bar() -> Style {
        Style::default().bg(Color::DarkGray).fg(Color::White)
    }
    pub fn input_box() -> Style {
        Style::default().fg(Color::White)
    }
    pub fn permission_dialog() -> Style {
        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
    }
    pub fn streaming() -> Style {
        Style::default().fg(Color::Green)
    }
    pub fn highlight() -> Style {
        Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
    }
}
