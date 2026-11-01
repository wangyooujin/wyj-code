# crates/cli

二进制入口。组装各 crate、解析参数、启动 TUI / REPL / 单次模式。

## 缺 API Key

只有 TUI 默认入口会打开 `ProfileDialog` 让用户填写（`InitialProvider::MissingApiKey` + `MissingKeyProvider`）。浮层拦截输入，占位 Provider 不会真被调用；保存后 `rebuild_fn` 换掉它。

headless、`-p`、ACP、daemon、`workflow run`、`run` 没有 UI，经 `require_provider` 非零退出，并提示打开 TUI 或设置 `WYJ_CODE_API_KEY`。不要在这些入口里阻塞读 stdin。

`Config::save_to` 在 Unix 上把文件收成 `0600`。

## Review

`cli::review_cmd` 对 diff 出可审计 JSON（rename、带空格路径、binary numstat，secret 脱敏），**不调用 LLM**。Release workflow 同时跑 workspace tests 和 `clippy --all-targets -D warnings`。
