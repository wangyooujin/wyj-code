# crates/cli (wyj-code 二进制入口)

## 首次启动缺 API Key 的处理(2026-09 起)

`wyj-code` 启动时若 `Config::api_key()` 返回 Err,**TUI 默认入口**会自动打开
`ProfileDialog`(`AppState::profile_dialog`),焦点预置到 active profile 的
`api_key` 字段(`PROFILE_API_KEY_FIELD_IDX = 5`)。用户填写后 Ctrl+S,
`profile_try_save` 会写盘 + chmod 0600 + 通过 `rebuild_fn` 重建 `shared_agent`,
无需重启。

其余 4 个入口(headless REPL / `-p` 单轮 prompt / ACP stdio / daemon TCP)
没有 UI 通道,继续以非零状态退出;`require_provider` 闭包统一给错误文案
追加"运行 `wyj-code` 打开交互界面完成首次配置,或设置 `WYJ_CODE_API_KEY`"
的 hint。

### 哨兵类型与占位 Provider

`crates/cli/src/main.rs::InitialProvider`:

- `Ready(Arc<dyn Provider>)` —— 正常路径
- `MissingApiKey` —— **仅 TUI 默认入口** + `cfg.api_key()` 失败时进入

```rust
let initial_provider = match wyj_api::build_provider_with_model(&cfg, &model_name) {
    Ok(p) => InitialProvider::Ready(p),
    Err(_e) if is_tui_mode && cfg.api_key().is_err() => InitialProvider::MissingApiKey,
    Err(e) => return Err(e),
};
let provider_arc: Arc<dyn Provider> = match &initial_provider {
    InitialProvider::Ready(p) => p.clone(),
    InitialProvider::MissingApiKey => Arc::new(MissingKeyProvider),
};
```

`MissingApiKeyProvider`(`crates/api/src/lib.rs`)实现 `Provider` trait,
所有方法返回 `ProviderErrorKind::MissingApiKey`。它**永不真正被触发**——
ProfileDialog 浮层在事件循环里拦截所有用户输入,`agent.run_turn` 在浮层
打开期间不会被调用;`profile_try_save` 成功后 `rebuild_fn` 替换掉 `shared_agent`,
占位 Provider 被丢弃。

### 4 个非 TUI 入口的 guard

`crates/cli/src/main.rs::require_provider(initial, entry_label)`,在以下 4 处调用:

- `RuntimeCommand::Acp` → `"acp"`
- `RuntimeCommand::Daemon { .. }` → `"daemon"`
- `RuntimeCommand::WorkflowRun { .. }` → `"workflow run"`
- `cli.prompt.is_some()` → `"single prompt (-p)"`
- `cli.headless` → `"headless REPL"`

`TUI` 分支(`wyj_tui::run_tui`)末尾传 `needs_api_key_onboarding = matches!(initial_provider, InitialProvider::MissingApiKey)`,由 `tui_main` 内部 `AppState::new` 之后立即 `state.profile_dialog = Some(ProfileDialog::new_for_onboarding(&state.config))`。

### 文件权限

`Config::save_to` 调 `write_atomic`(临时文件 + rename)后,在 Unix 上调用
`std::fs::set_permissions(path, Permissions::from_mode(0o600))`,收紧到仅当前
用户可读写;Windows 上无等价模式,跳过。

### i18n 新增 6 个 key(中英同步)

- `main.api_key_missing_for` —— 非 TUI 入口错误前缀(含 `{entry}` 参数)
- `status.api_key_missing_onboarding_headless` —— 双行 hint
- `profile.onboarding.title` / `profile.onboarding.hint` / `profile.onboarding.success`
- `profile.error.api_key_required` —— 引导提交校验