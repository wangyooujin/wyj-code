# crates/tui

TUI 聊天区渲染（对齐 Claude Code 鼠标体验）：主循环用 `ratatui::Viewport::Inline` 而非 alternate screen——已定型的消息一旦满足冻结条件就通过 `Terminal::insert_before` 一次性写入终端真实 scrollback（此后不再参与每帧重绘），鼠标滚轮翻页与原生点击拖拽选中复制因此天然可用，无需 `EnableMouseCapture`（那会让终端把鼠标事件整个交给应用，原生选中就没法用了，历史上这个取舍反复过好几次）。冻结判定 `app::compute_freezable_up_to` 三条阻塞规则：① 该位置是 `ToolCall` 但对应 `ToolResult` 未出现（`parallel_safe()` 工具并发乱序完成，结果可能 `insert` 在更早位置之后，未落定前不能冻结）；② 最后一条可折叠 `ToolResult` 及其后仍留在活跃尾部（用户仍需要方向键选中后展开/收起）；③ 关联子 Agent 仍 `Running`（对应位置还在画动态状态行）。冻结前缀之外的"待定尾部"（`AppState.frozen_up_to..`）+ 流式输出/任务面板/子 Agent 面板，每帧仍照常重绘，渲染逻辑通过 `render::render_message_range` 与冻结路径共用，避免两边 drift。

**交互尾部不变式**：`compute_freezable_up_to` 不自行扫描最后可折叠项，调用方传入 `render::last_collapsible_tool_result_idx` 的结果；冻结边界停在该下标之前，保证用户当前仍能通过方向键选中最后一条可交互 ToolResult。已经进入终端 scrollback 的旧消息是静态历史，不再支持应用内展开/收起。

**同帧内 insert_before 与 viewport resize 互斥**：主循环记录本轮是否发生了 `insert_before`（`froze_this_frame`），若是则本轮跳过 `fixed_footer_height`/`pending_chat_visual_height` 触发的 resize，推迟到下一轮——`insert_before` 刚写完紧接着 `clear()`+重建 `Terminal` 这个序列背靠背执行，叠加 Ctrl+O 展开/收起或多 Agent 面板导致的高度剧烈跳变，是终端绘制层面撕裂（字符错位、新旧内容重叠）的直接成因。

`/mcp` `/model` `/plugins` `/skills` `/config` `/memory` `/resume` 这 7 个重量级管理对话框（`AppState::wants_fullscreen`）打开期间临时整个重建 `Terminal` 切到 `Viewport::Fullscreen` + `EnterAlternateScreen`（复用原有整屏渲染代码不变），关闭后重建回 Inline；权限确认框（`PermissionDialog`）因为几乎每次 Edit/Write/Bash 调用都可能弹出，改归类为底部常驻面板（`BottomPanel::Permission`）而非全屏浮层，避免高频闪烁。Inline viewport 高度由 `render::fixed_footer_height` + `render::pending_chat_visual_height` 每帧计算，变化时重建 Terminal（重建前必须先 `terminal.clear()`，否则新 Terminal 不知道旧视口在屏幕上的位置会导致画面重叠）。接受的取舍：已冻结内容 resize 不会重新换行、历史回看长度由终端自身 scrollback 缓冲区大小决定、不再有应用内 PageUp/PageDown/Ctrl+Home/End 翻页快捷键（交还给终端原生处理）。
