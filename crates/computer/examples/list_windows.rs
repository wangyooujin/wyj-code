//! 临时真机验证脚本（非交付物）：只读列出当前所有可见窗口的 app_name/title，
//! 不截图、不改变任何东西，用于挑一个已经打开、截图它不会有任何副作用的
//! 窗口，供 window_capture_probe.rs 使用。
//! 用法：cargo run -p wyj-computer --example list_windows

fn main() -> anyhow::Result<()> {
    let windows = xcap::Window::all()?;
    for w in windows {
        let title = w.title().unwrap_or_default();
        let app = w.app_name().unwrap_or_default();
        let minimized = w.is_minimized().unwrap_or(true);
        if minimized || (title.is_empty() && app.is_empty()) {
            continue;
        }
        println!("app={app:?} title={title:?} minimized={minimized}");
    }
    Ok(())
}
