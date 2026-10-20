//! `app` 子模块共用的无状态小工具。
//!
//! 从 `app.rs` 拆出。这里的函数不碰 `AppState`、不做 IO，纯粹是下标循环、
//! 均分、比较这类纯函数——被多个面板/按键处理调用，放哪个业务文件都不合适。

use super::*;

pub(crate) fn cycle_index(current: usize, len: usize, forward: bool) -> usize {
    if len == 0 {
        return 0;
    }
    if forward {
        (current + 1) % len
    } else {
        (current + len - 1) % len
    }
}

/// 判断新到达的任务列表是否属于"新一轮"（相对旧列表没有任何 (id, content) 都匹配的延续任务）。
pub(crate) fn is_new_todo_round(old: Option<&[TodoItem]>, new: &[TodoItem]) -> bool {
    let Some(old) = old else {
        return true;
    };
    !new.iter()
        .any(|n| old.iter().any(|o| o.id == n.id && o.content == n.content))
}

/// 将 total 均分给 n 份，各份之和严格等于 total（余数分给前几份）。
pub(crate) fn split_evenly(total: u32, n: usize) -> Vec<u32> {
    let n32 = n as u32;
    let base = total / n32;
    let rem = total % n32;
    (0..n32)
        .map(|i| if i < rem { base + 1 } else { base })
        .collect()
}
