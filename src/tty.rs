//! TTY 检测。

use std::io::IsTerminal;

pub fn is_tty() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}
