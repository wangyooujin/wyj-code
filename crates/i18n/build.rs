fn main() {
    // rust-i18n 的 `i18n!` 宏在部分环境下无法可靠地把 locales/*.yml 注册为
    // 增量编译依赖，导致只改 yml 不改 .rs 时 cargo 不会重新编译本 crate，
    // 运行时看到的还是旧翻译（甚至显示字面量 key）。显式声明该目录触发重编译。
    println!("cargo:rerun-if-changed=locales");
}
