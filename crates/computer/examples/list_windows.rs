//! 临时真机验证脚本（非交付物）：只读列出当前所有可见窗口的 app_name/title，
//! 不截图、不改变任何东西，用于挑一个已经打开、截图它不会有任何副作用的
//! 窗口，供 window_capture_probe.rs 使用。
//! 用法：cargo run -p wyj-computer --example list_windows
//!
//! 走 `wyj_computer::target::list_windows()` 而非直接 `xcap::Window::all()`：
//! `xcap` 只在 macOS/Windows 是本 crate 的依赖，直接引用会导致 Linux 上
//! `cargo test`（默认会编译 examples）因未声明的 crate 报错，间接调用则天然
//! 复用跨平台的桩实现（非 macOS/Windows 上运行期返回错误而非编译期失败）。

fn main() -> anyhow::Result<()> {
    let windows = wyj_computer::target::list_windows()?;
    for w in windows {
        println!(
            "app={:?} title={:?} focused={}",
            w.app_name, w.title, w.focused
        );
    }
    Ok(())
}
