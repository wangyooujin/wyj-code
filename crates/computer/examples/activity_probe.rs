//! 临时真机验证脚本（非交付物）：验证 activity::snapshot() 的两个核心假设——
//! ①enigo 合成的鼠标事件是否也会重置 HID idle 计时器（决定 B 节自归因设计
//! 是否必要）；②CGSessionCopyCurrentDictionary 的锁屏键读取是否按预期工作
//! （此脚本不主动锁屏，只读当前状态，避免打断使用者）。
//! 用法：cargo run -p wyj-computer --example activity_probe

fn main() -> anyhow::Result<()> {
    let snap = wyj_computer::activity::snapshot()?;
    println!(
        "[1] initial snapshot: idle_secs={:.3} screen_locked={}",
        snap.idle_secs, snap.screen_locked
    );

    // 非侵入式验证：把鼠标"移动"到它当前所在的同一坐标（零位移，物理上
    // 不会产生可见移动/不会点击任何东西），只用来验证 enigo 走的
    // CGEventPost 合成事件是否也会被计入 HID idle 计时器。
    let (x, y) = wyj_computer::cursor_location()?;
    println!("[2] current cursor at ({x}, {y}), issuing a same-position synthetic move_mouse...");
    wyj_computer::move_mouse(x, y)?;
    std::thread::sleep(std::time::Duration::from_millis(80));

    let snap2 = wyj_computer::activity::snapshot()?;
    println!(
        "[3] snapshot right after self move_mouse: idle_secs={:.3} screen_locked={}",
        snap2.idle_secs, snap2.screen_locked
    );

    if snap2.idle_secs < 0.5 {
        println!("=> CONFIRMED: enigo 合成输入确实会重置 idle 计时器，B 节的自归因设计是必要的。");
    } else {
        println!(
            "=> 出乎意料：合成输入似乎没有重置 idle 计时器（idle_secs 未归零）。这与最初的\n   假设不符，需要重新评估 B 节的自归因设计是否还有必要，或问题出在别处\n   （例如 move_mouse 到同一坐标可能被系统去重、未真正触发 HID 事件）。"
        );
    }

    // 静置几秒后再采一次样，确认 idle_secs 会持续增长（探测本身没有把
    // 自己的读取误当成一次"输入"，即 CGEventSourceSecondsSinceLastEventType
    // 是纯读操作，不产生副作用）。
    std::thread::sleep(std::time::Duration::from_secs(2));
    let snap3 = wyj_computer::activity::snapshot()?;
    println!(
        "[4] snapshot after 2s idle: idle_secs={:.3} (应比 [3] 大约多 2.0s) screen_locked={}",
        snap3.idle_secs, snap3.screen_locked
    );

    println!("\n注：本脚本不会尝试锁屏（锁屏检测的键值判断需要真实锁屏后人工复核，属于\n后续手动验收步骤，不在此自动化探测范围内，避免打断当前使用）。");

    Ok(())
}
