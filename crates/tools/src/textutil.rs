//! 文本截断工具 — UTF-8 安全，供各工具共享。
//!
//! 旧实现 `&s[..max]` 按字节切片，`max` 落在多字节字符（中文/emoji）中间
//! 会直接 panic；这里统一回退到最近的 char boundary。
//!
//! **实现已上收到 `wyj_core::textutil`**：本模块此前带一份手写副本，而
//! `core/serialize.rs` 还各有一份逐行相同的实现（三处重复）。且副本的
//! `truncate_head_tail` 尾部起点**向上**走（`start += 1`），会超出
//! `tail_bytes` 预算最多 3 字节——与 `core::verify::clip_output` 不一致。
//! 现在全部走 core 的实现，尾部**向下**回退，保证不超预算。
//!
//! 本模块保留是为了不改动 `crate::textutil::truncate_str(...)` 的调用写法。

pub use wyj_core::textutil::{truncate_head_tail, truncate_str};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_str_ascii() {
        assert_eq!(truncate_str("hello", 3), "hel");
        assert_eq!(truncate_str("hello", 10), "hello");
    }

    #[test]
    fn truncate_str_multibyte_no_panic() {
        // "中" 是 3 字节，切在中间必须回退
        assert_eq!(truncate_str("中文", 4), "中");
        assert_eq!(truncate_str("中文", 2), "");
        assert_eq!(truncate_str("a中文", 3), "a");
        // emoji（4 字节）
        assert_eq!(truncate_str("🦀🦀", 5), "🦀");
    }

    #[test]
    fn head_tail_keeps_both_ends() {
        let s = "AAAA".repeat(100) + &"Z".repeat(100);
        let out = truncate_head_tail(&s, 40, 40);
        assert!(out.starts_with("AAAA"));
        assert!(out.ends_with(&"Z".repeat(40)));
        assert!(out.contains("[truncated"));
    }

    #[test]
    fn head_tail_multibyte_no_panic() {
        let s = "中".repeat(1000);
        let out = truncate_head_tail(&s, 100, 100);
        assert!(out.contains("[truncated"));
        assert!(out.starts_with('中'));
        assert!(out.ends_with('中'));
    }

    #[test]
    fn head_tail_short_passthrough() {
        assert_eq!(truncate_head_tail("short", 100, 100), "short");
    }
}
