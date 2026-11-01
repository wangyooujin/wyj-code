# vendor/xcap — 本地 patch 说明

## 为什么要 vendor 这份 xcap

xcap 0.9.8 在 macOS 平台的依赖声明里**没有写 `default-features = false`**：

```toml
[target.'cfg(target_os = "macos")'.dependencies.objc2-app-kit]
version = "0.3.2"          # ← 默认 feature 全开
```

`objc2-app-kit` 的 `default` feature 会启用**全部 298 个 AppKit 绑定 feature**，
对应 `src/generated/` 下 294 个生成文件（约 4.9 MB 代码）。

而 xcap 的 macOS 实现（`src/macos/`）实际只用到极少数几个类：

```
NSApplicationProcessIdentifier  NSDictionary  NSNumber  NSObject
NSObjectProtocol                NSScreen      NSScreenNumber
NSString                       NSWorkspace
```

代价是实打实的（2026-11-01 实测，`cargo build --timings`）：

| | app-kit feature 数 | objc2-app-kit 编译耗时 | objc2-foundation 编译耗时 |
|---|---|---|---|
| 未 patch | 298 | 10.95s | 10.44s |
| 本 patch | 13 | （不再出现在榜首） | 2.94s |

冷构建总耗时 31.3s → 22.5s。

**上游未修复**：0.9.8 的声明与 0.9.6 完全一致，仍未加 `default-features = false`。

## 本 patch 做了两件事

### 1. 裁剪 objc2 feature（性能收益）

给 `objc2-app-kit` / `objc2-foundation` / `objc2-core-foundation` 加上
`default-features = false`，并显式列出 xcap 真正用到的 feature：

- `objc2-app-kit`: `std`, `NSScreen`, `NSWorkspace`, `NSGraphics`
- `objc2-foundation`: `std`, `NSString`, `NSArray`, `NSDictionary`, `NSValue`,
  `NSObject`, `NSData`, `NSDate`, `NSError`, `NSEnumerator`, `NSKeyValueCoding`
- `objc2-core-foundation`: 无额外 feature

其中：
- `NSGraphics` —— `NSScreen.deviceDescription()` 需要它
- `NSKeyValueCoding` —— `NSDictionary.valueForKey()` 需要它
- `libc` —— `NSRunningApplication.processIdentifier()` 需要它
  （注意：这条是 **`crates/computer` 自己**的需求，不是 xcap 的，见下）

> 项目自己的 `crates/computer/Cargo.toml` 里对 `objc2-app-kit` /
> `objc2-foundation` 也补了 `NSGraphics` / `libc` / `NSKeyValueCoding` 等 feature。
> 这些以前是从 xcap 拉进来的默认 feature 里「白拿」的，裁剪后必须自己声明。

### 2. 压制 deprecated 告警（`[lints]` 段）

裁剪后构建会出现 3 条告警：

```
warning: use of deprecated function `objc2_core_graphics::CGWindowListCreateImage`:
    Please use ScreenCaptureKit instead.
warning: use of deprecated method `objc2_app_kit::NSWorkspace::activeApplication`:
    Use -[NSWorkspace frontmostApplication] instead.
```

**这不是裁剪引起的**，而是 path 依赖与 registry 依赖的固有差异：

- xcap 从 crates.io 装时，cargo 会给它加 `--cap-lints allow`，把 lint 告警封顶，
  所以**上游用户根本看不到这些告警**。
- 作为 path 依赖（本地代码）时没有 `--cap-lints allow`，告警原样暴露。

实测证据（同一份源码与 manifest，只改依赖形式）：

| 依赖形式 | deprecated 告警数 |
|---|---|
| registry (`xcap = "0.9.8"`) | 0 |
| path (`vendor/xcap`) | 3 |

因此本 patch 在 `vendor/xcap/Cargo.toml` 末尾加：

```toml
[lints.rust]
deprecated = "allow"
```

效果与上游发布时的实际可见行为一致，**且不修改任何源码**。

> 注：xcap 源码里对 `activeApplication` 有注释说明为何仍用 deprecated API——
> `frontmostApplication` 返回的是「进程启动时在前台的应用」，与 `activeApplication`
> 语义不同，替换会改变行为。这是上游的已知取舍，不是 bug。

## 生效方式

项目根 `Cargo.toml` 末尾：

```toml
[patch.crates-io]
xcap = { path = "vendor/xcap" }
```

## 维护说明

- **源码零改动**：只有 `Cargo.toml` 被修改（feature 裁剪 + `[lints]` 段）。
- 上游 xcap 若哪天修好了 feature 问题（加上 `default-features = false`），
  可以直接删掉 `vendor/` 目录和 `[patch.crates-io]` 段。检查命令：

  ```bash
  grep -A3 'dependencies.objc2-app-kit' \
    ~/.cargo/registry/src/*/xcap-0.9.*/Cargo.toml
  ```

- 升级 xcap 版本时，需要同步更新 `vendor/xcap/` 的源码，并重新核对 feature 列表
  （新版本可能用到新的 AppKit 类）。
- 若编译报 `no method named ... found`，说明 xcap 新增了对某个类的依赖，
  在对应 crate 的 features 里补上即可（feature 名见各 crate 的 `Cargo.toml`）。
