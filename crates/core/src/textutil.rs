//! UTF-8 char boundary 安全的字符串工具，供 crates/core 内复用。
//!
//! `String::truncate(new_len)` 在 `new_len` 不是 char boundary 时会
//! `assertion failed: self.is_char_boundary(new_len)` 触发进程 panic；
//! 这里提供"先把上限回退到最近的 char boundary、再 truncate"的标准做法，
//! 避免按字节硬切字符串时撞上 CJK / emoji 多字节字符。
//!
//! （`wyj-tools::textutil::truncate_str` 走的是返回 `&str` 的路径，适合"读
//! 取 + 展示"场景；这里提供的是 `usize` 索引版本，配合 `String::truncate`
//! 做原地截断，避免重复分配。）

/// 把 `idx` 回退到 `s` 中最近的 char boundary（`<= idx`）。保证返回值满足
/// `s.is_char_boundary(idx) == true`，可直接传给 `String::truncate`。
///
/// - `idx >= s.len()`：原样返回 `s.len()`（截到末尾也是合法 boundary）。
/// - `idx == 0`：原样返回 `0`（`0` 永远是 boundary）。
/// - 否则：若 `idx` 已落在 boundary 上，原样返回；否则线性向前回退到最近的
///   boundary。最坏情况回退 3 字节（UTF-8 单字符最长 4 字节）。
pub fn floor_char_boundary(s: &str, idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    if s.is_char_boundary(idx) {
        return idx;
    }
    let mut end = idx;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floor_char_boundary_ascii_passthrough() {
        let s = "hello";
        assert_eq!(floor_char_boundary(s, 3), 3);
        assert_eq!(floor_char_boundary(s, 5), 5);
        assert_eq!(floor_char_boundary(s, 0), 0);
    }

    #[test]
    fn floor_char_boundary_cjk_reverts() {
        // "中文" 中=3 字节,文=3 字节,共 6 字节
        let s = "中文";
        // 切到 4 字节 = "中" 末尾 + "文" 第一字节之间,应回退到 3
        assert_eq!(floor_char_boundary(s, 4), 3);
        // 切到 5 = "文" 第二字节处,应回退到 3
        assert_eq!(floor_char_boundary(s, 5), 3);
        // 切到 6 = 完整字符串
        assert_eq!(floor_char_boundary(s, 6), 6);
        // 切到 1 或 2 = "中" 第一字节内,回退到 0
        assert_eq!(floor_char_boundary(s, 1), 0);
        assert_eq!(floor_char_boundary(s, 2), 0);
    }

    #[test]
    fn floor_char_boundary_emoji_reverts() {
        // "🦀" 是 4 字节
        let s = "🦀";
        // 切到 3 = 第 3 字节,回退到 0
        assert_eq!(floor_char_boundary(s, 3), 0);
        // 切到 4 = 完整字符串
        assert_eq!(floor_char_boundary(s, 4), 4);
        // 切到 5 > len(),返回 4
        assert_eq!(floor_char_boundary(s, 5), 4);
    }

    #[test]
    fn floor_char_boundary_mixed() {
        // "a中文b" 字节布局(共 1+3+3+1=8 字节):
        //   idx 0='a'B0       (boundary)
        //   idx 1='中'B0      (boundary)
        //   idx 2='中'B1      (非 boundary)
        //   idx 3='中'B2      (非 boundary)
        //   idx 4='文'B0      (boundary)
        //   idx 5='文'B1      (非 boundary)
        //   idx 6='文'B2      (非 boundary)
        //   idx 7='b'B0       (boundary)
        //   idx 8=末尾         (boundary, == len)
        let s = "a中文b";
        assert_eq!(s.len(), 8);

        // 已在 boundary:原样返回
        assert_eq!(floor_char_boundary(s, 0), 0);
        assert_eq!(floor_char_boundary(s, 1), 1);
        assert_eq!(floor_char_boundary(s, 4), 4);
        assert_eq!(floor_char_boundary(s, 7), 7);
        assert_eq!(floor_char_boundary(s, 8), 8);

        // 落在 '中' 第二/第三字节:回退到 1('中' 起点)
        assert_eq!(floor_char_boundary(s, 2), 1);
        assert_eq!(floor_char_boundary(s, 3), 1);

        // 落在 '文' 第二/第三字节:回退到 4('文' 起点)
        assert_eq!(floor_char_boundary(s, 5), 4);
        assert_eq!(floor_char_boundary(s, 6), 4);
    }

    #[test]
    fn floor_char_boundary_out_of_range_clamps() {
        let s = "abc";
        assert_eq!(floor_char_boundary(s, 100), 3);
    }

    #[test]
    fn floor_char_boundary_empty_string() {
        let s = "";
        assert_eq!(floor_char_boundary(s, 0), 0);
        assert_eq!(floor_char_boundary(s, 100), 0);
    }

    #[test]
    fn truncate_after_floor_does_not_panic() {
        // 端到端：模拟 memory_v3.rs:977 真实 use case 的 String truncate 流程
        let mut s = String::from("a中文b中文c");
        // 长度 1+3+1+3+1 = 9,切到 4 = "a中" 末尾 + "文" 第一字节之间
        let keep = floor_char_boundary(&s, 4);
        s.truncate(keep);
        assert_eq!(s, "a中");
    }

    #[test]
    fn truncate_at_exact_boundary_does_not_panic() {
        let mut s = String::from("中文abc");
        // 6 字节正好落在 "文" 末尾
        let keep = floor_char_boundary(&s, 6);
        s.truncate(keep);
        assert_eq!(s, "中文");
    }
}
