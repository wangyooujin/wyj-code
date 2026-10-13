//! 退出前排空 tty 输入队列。
//!
//! Ghostty 路径下 `app::enter_terminal_screen` 压入了 Kitty 键盘协议的
//! `DISAMBIGUATE_ESCAPE_CODES | REPORT_EVENT_TYPES`。按协议规范，disambiguate 一开，
//! `ctrl+c` 就不再以 `0x03` 送达，而是 `CSI 99;5;1u`（press）+ `CSI 99;5;3u`
//! （release，由 report_event_types 追加事件类型字段）。
//!
//! TUI 收到 press 就立刻退出（二次 Ctrl+C → `should_quit` → 事件循环 break），而松键
//! 才生成的 release 事件此刻还躺在内核 tty 队列里没人读。`PopKeyboardEnhancementFlags`
//! 只影响**之后**的按键编码方式，已经写进 pty 的字节撤不回来，于是它们被下一个进程
//! 读到：zsh 的 ZLE 不认识带参数的 `CSI u`，吃掉 `ESC [` 后把参数当普通字符插进命令行，
//! 表现为提示符后凭空多出 `9;5;3u`（Ctrl+D 退出则是 `100;5;3u`）。
//!
//! 修法：pop 之后、关闭 raw mode 之前，非阻塞读 `/dev/tty` 到 `EAGAIN`，把这批在途
//! 事件丢掉。必须夹在两步之间：
//! * pop 之前排空没用——终端还在增强模式，随时可能再写新的 release 事件进来；
//! * 关 raw mode 之后排空有害——canonical 模式的 `ISIG` 会让队列里残留的 `0x03`
//!   给本进程发 SIGINT，`ECHO` 也可能把字节回显到屏幕上。
//!
//! 注意 pop 只能改变**之后**的编码方式，杀不掉已经发出的字节，而 release 事件的
//! 产生时刻 = 用户松手时刻，与终端何时收到 pop 无关：所以不能只排一次空就收工，
//! 必须在一个有界窗口里反复排空，直到连续一段时间没有新字节到达。

/// 两轮排空之间的间隔。
const QUIET_SLICE: std::time::Duration = std::time::Duration::from_millis(40);
/// 最多排空多久。人类按键的 hold 时长中位数约 100ms，200ms 的窗口足够覆盖
/// “退出流程跑得比松手还快”这种竞态；再长只会白白拖慢退出。
const MAX_WINDOW: std::time::Duration = std::time::Duration::from_millis(200);

/// 丢掉已经到达、但退出后没人会处理的按键事件（Kitty release/repeat、粘贴尾巴等）。
///
/// 必须在 **raw mode 仍开启** 且 **键盘增强标志已 pop** 的窗口内调用，且只应由持有
/// TUI 控制终端的那个线程调用：后台 tokio worker 的 panic 也会触发 panic hook，
/// 那种情况下从 worker 排空 `/dev/tty` 会吞掉用户在 TUI 里刚敲的键。
pub fn drain_pending_input() {
    #[cfg(unix)]
    {
        use std::fs::OpenOptions;
        use std::io::Read;
        use std::os::unix::fs::OpenOptionsExt;
        use std::time::Instant;

        let mut tty = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open("/dev/tty")
        {
            Ok(tty) => tty,
            // 没有控制终端（daemon / 计划任务唤起 / 单元测试）：没有队列可排。
            Err(_) => return,
        };

        let deadline = Instant::now() + MAX_WINDOW;
        let mut buf = [0u8; 256];
        loop {
            // 队列里有多少吃多少，直到 EAGAIN。
            loop {
                match tty.read(&mut buf) {
                    Ok(0) => break,
                    Ok(_) => continue,
                    // EAGAIN / EWOULDBLOCK：队列已空
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => return,
                }
            }
            let now = Instant::now();
            if now >= deadline || deadline.duration_since(now) < QUIET_SLICE {
                break;
            }
            std::thread::sleep(QUIET_SLICE);
        }
    }
    #[cfg(not(unix))]
    {
        // 非 Unix 平台没有这里的 `/dev/tty` 等价物需要处理，保持 no-op。
    }
}
