//! macOS 后台优先 computer-use 工具。
//!
//! 与旧版全局 `computer` 不同，本工具的每次动作都绑定稳定窗口目标；点击和
//! 文本走 Accessibility，键盘/滚动最多向目标 PID 定向投递，不移动系统光标、
//! 不主动切前台，也不会在失败时悄悄回退到全局输入。

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;
use wyj_api::types::{ToolDefinition, ToolResultPart};
use wyj_computer::element::{ElementAction, ElementRef, TraversalBudget};
use wyj_computer::target::WindowTarget;
use wyj_core::tool::{Tool, ToolContext, ToolResult};

pub struct AppComputerTool {
    max_dim: u32,
    focused_quiet_period: Duration,
    observations: Mutex<HashMap<u32, WindowTarget>>,
    /// 每个窗口最近一次 `list_elements` 的 `tree_id`，供动作校验 ref 是否过期。
    ///
    /// **只存 `tree_id`，不存 AX 句柄**：`AXUIElement` 不是 `Send`/`Sync`
    /// （底层 `UnsafeCell`），存进本工具的 `Mutex` 会让 `Arc<dyn Tool>` 失去
    /// 跨线程可用性。句柄的定位与使用因此完全下沉到 `wyj_computer::element`
    /// 内部完成（见 [`element_mutation`]）。
    element_tree_ids: Mutex<HashMap<u32, u64>>,
    /// 每个窗口最近一次 `list_menus` 的 `tree_id`，同理用于菜单引用的 stale 校验。
    menu_tree_ids: Mutex<HashMap<u32, u64>>,
    /// 某 App/动作若曾在没有人类输入的情况下抢走前台，本会话立即熔断该
    /// 组合，避免同一种不兼容后台动作反复打扰用户。
    incompatible_actions: Mutex<HashSet<(String, String)>>,
}

impl AppComputerTool {
    pub fn new(max_dim: u32, focused_quiet_period: Duration) -> Self {
        Self {
            max_dim,
            focused_quiet_period,
            observations: Mutex::new(HashMap::new()),
            element_tree_ids: Mutex::new(HashMap::new()),
            menu_tree_ids: Mutex::new(HashMap::new()),
            incompatible_actions: Mutex::new(HashSet::new()),
        }
    }

    fn screenshot(&self, input: &Value) -> ToolResult {
        let window_id = match required_u32(input, "window_id") {
            Ok(value) => value,
            Err(error) => return error_result(error),
        };
        if let Some(generation) = input.get("generation").and_then(Value::as_u64) {
            if let Err(error) = wyj_computer::target::validate_window_target(window_id, generation)
            {
                return error_result(error);
            }
        }
        let (target, capture) =
            match wyj_computer::target::capture_window_by_id(window_id, self.max_dim) {
                Ok(result) => result,
                Err(error) => return error_result(error),
            };
        self.observations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(window_id, target.clone());
        let display = serde_json::to_string_pretty(&serde_json::json!({
            "window": target,
            "capture": {
                "pixel_width": capture.physical_width,
                "pixel_height": capture.physical_height,
                "target_width": capture.target_width,
                "target_height": capture.target_height,
                "png_kb": capture.png.len() / 1024,
            }
        }))
        .unwrap_or_else(|_| format!("[app_computer screenshot window_id={window_id}]"));
        ToolResult::with_parts(
            display,
            vec![ToolResultPart::Image {
                media_type: "image/png".to_string(),
                data: capture.png_base64(),
            }],
        )
    }

    fn resolve_observation(&self, input: &Value) -> Result<WindowTarget> {
        let window_id = required_u32(input, "window_id")?;
        let generation = required_u64(input, "generation")?;
        let mut current = wyj_computer::target::validate_window_target(window_id, generation)?;

        if let Some(observed) = self
            .observations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&window_id)
            .filter(|observed| observed.generation == generation)
            .cloned()
        {
            current.target_width = observed.target_width;
            current.target_height = observed.target_height;
            return Ok(current);
        }

        current.target_width = required_u32(input, "target_width")?;
        current.target_height = required_u32(input, "target_height")?;
        anyhow::ensure!(
            current.target_width > 0 && current.target_height > 0,
            "target_changed: invalid captured coordinate space; take a new window screenshot"
        );
        self.observations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(window_id, current.clone());
        Ok(current)
    }

    fn target_region(target: &WindowTarget) -> wyj_computer::activity::InputRegion {
        wyj_computer::activity::InputRegion {
            x: f64::from(target.x),
            y: f64::from(target.y),
            width: f64::from(target.width),
            height: f64::from(target.height),
        }
    }

    fn begin_mutation(
        &self,
        target: &WindowTarget,
        action: &str,
    ) -> Result<(wyj_computer::activity::InputLease, u32)> {
        if self
            .incompatible_actions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(&(target.app_name.clone(), action.to_string()))
        {
            return Err(anyhow!(
                "requires_foreground_takeover: background {action} was disabled for `{}` after it changed the user's foreground application",
                target.app_name
            ));
        }

        let frontmost_pid = wyj_computer::accessibility::frontmost_pid()?;
        let quiet = if frontmost_pid == target.pid {
            self.focused_quiet_period
        } else {
            Duration::ZERO
        };
        let lease = wyj_computer::activity::acquire_lease(quiet)?;
        self.ensure_mutation_safe(target, &lease, frontmost_pid == target.pid)?;
        Ok((lease, frontmost_pid))
    }

    fn ensure_mutation_safe(
        &self,
        target: &WindowTarget,
        lease: &wyj_computer::activity::InputLease,
        target_frontmost: bool,
    ) -> Result<()> {
        let conflict = if target_frontmost {
            !wyj_computer::activity::lease_is_valid(lease)
        } else {
            wyj_computer::activity::conflicts_with_background_target(
                lease,
                Self::target_region(target),
                false,
            )
        };
        if conflict {
            return Err(anyhow!(
                "preempted_by_user: external input may affect the target window; do not retry automatically"
            ));
        }
        Ok(())
    }

    fn finish_mutation(
        &self,
        target: &WindowTarget,
        action: &str,
        lease: &wyj_computer::activity::InputLease,
        frontmost_before: u32,
    ) -> Result<()> {
        let frontmost_after = wyj_computer::accessibility::frontmost_pid()?;
        if frontmost_after != frontmost_before {
            if !wyj_computer::activity::lease_is_valid(lease) {
                return Err(anyhow!(
                    "preempted_by_user: the user changed the foreground application during the background action"
                ));
            }
            self.incompatible_actions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert((target.app_name.clone(), action.to_string()));
            wyj_computer::telemetry::record_background_focus_fuse();
            return Err(anyhow!(
                "target_changed: background {action} on `{}` changed the user's foreground application ({frontmost_before} -> {frontmost_after}); this App/action is disabled for the rest of the session",
                target.app_name
            ));
        }

        self.ensure_mutation_safe(target, lease, frontmost_after == target.pid)
    }

    fn inspect(&self, input: &Value) -> ToolResult {
        let target = match self.resolve_observation(input) {
            Ok(target) => target,
            Err(error) => return error_result(error),
        };
        let (x, y) = match required_coordinate(input) {
            Ok(coordinate) => coordinate,
            Err(error) => return error_result(error),
        };
        let (global_x, global_y) = match target.screenshot_to_global(x, y) {
            Ok(coordinate) => coordinate,
            Err(error) => return error_result(error),
        };
        match wyj_computer::accessibility::inspect(target.pid, global_x, global_y) {
            Ok(info) => ToolResult::ok(
                serde_json::to_string_pretty(&info).unwrap_or_else(|_| format!("{info:?}")),
            ),
            Err(error) => error_result(error),
        }
    }

    /// 列出目标窗口 AX 树里的语义元素。
    ///
    /// 这是 grounding 的主力入口：模型据此用 `element_ref` 指元素，而不是在
    /// 下采样截图上猜像素。**始终附截图**——AX 树缺失时（`root_available:false`）
    /// 截图就是模型唯一的退路，不该让它再花一次调用才拿到。
    ///
    /// 截图只做一次：元素几何要映射到「模型看到的截图空间」必须有捕获尺寸，
    /// 而返回给模型的截图必须是**同一次观察**，否则两套坐标会错位。
    fn list_elements(&self, input: &Value) -> ToolResult {
        let window_id = match required_u32(input, "window_id") {
            Ok(value) => value,
            Err(error) => return error_result(error),
        };
        if let Some(generation) = input.get("generation").and_then(Value::as_u64) {
            if let Err(error) = wyj_computer::target::validate_window_target(window_id, generation)
            {
                return error_result(error);
            }
        }

        let (target, capture) =
            match wyj_computer::target::capture_window_by_id(window_id, self.max_dim) {
                Ok(result) => result,
                Err(error) => return error_result(error),
            };
        self.observations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(window_id, target.clone());

        let snapshot =
            match wyj_computer::element::snapshot_elements(&target, &TraversalBudget::default()) {
                Ok(snapshot) => snapshot,
                Err(error) => return error_result(error),
            };

        let mut display = serde_json::to_string_pretty(&serde_json::json!({
            "tree_id": snapshot.tree_id,
            "root_available": snapshot.root_available,
            "count": snapshot.elements.len(),
            "truncated": snapshot.truncated,
            "filtered_out": snapshot.filtered_out,
            "visited": snapshot.visited,
            "elapsed_ms": snapshot.elapsed_ms,
            "capture": {
                "target_width": capture.target_width,
                "target_height": capture.target_height,
            },
            "elements": snapshot.elements,
        }))
        .unwrap_or_else(|_| format!("[list_elements window_id={window_id}]"));

        if !snapshot.root_available {
            display.push_str(
                "\n\nThis app does not expose an accessibility tree. Use `coordinate` plus `zoom` instead; do not retry `list_elements`.",
            );
        } else if snapshot.truncated {
            display.push_str(
                "\n\nPartial result: the element tree exceeded the traversal budget, so some elements are missing. Use the attached screenshot with `coordinate`, or narrow the target window, to reach the rest.",
            );
        }

        ToolResult::with_parts(
            display,
            vec![ToolResultPart::Image {
                media_type: "image/png".to_string(),
                data: capture.png_base64(),
            }],
        )
    }

    /// 列举目标 App 的菜单栏。
    ///
    /// 菜单是「无副作用地驱动 App」的最优通道：「文件 → 新建标签页」这类命令
    /// 不需要移动光标、不需要点中任何控件，纯 AXPress 即可，而对应的右键菜单
    /// （上下文菜单）根本没法凭空创建。
    fn list_menus(&self, input: &Value) -> ToolResult {
        let window_id = match required_u32(input, "window_id") {
            Ok(value) => value,
            Err(error) => return error_result(error),
        };
        if let Some(generation) = input.get("generation").and_then(Value::as_u64) {
            if let Err(error) = wyj_computer::target::validate_window_target(window_id, generation)
            {
                return error_result(error);
            }
        }
        let target = match wyj_computer::target::find_window_by_id(window_id) {
            Ok(target) => target,
            Err(error) => return error_result(error),
        };
        // 带 `menu_ref` 时展开该菜单项的子菜单，否则返回菜单栏顶层。
        let parent = match input.get("expand").and_then(Value::as_str) {
            Some(raw) => match parse_menu_ref_str(raw) {
                Ok(reference) => Some(reference),
                Err(error) => return error_result(error),
            },
            None => None,
        };
        let snapshot = match wyj_computer::menu::list_level(
            &target,
            parent,
            &wyj_computer::menu::MenuBudget::default(),
        ) {
            Ok(snapshot) => snapshot,
            Err(error) => return error_result(error),
        };
        self.menu_tree_ids
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(window_id, snapshot.tree_id);

        let mut display = serde_json::to_string_pretty(&serde_json::json!({
            "tree_id": snapshot.tree_id,
            "menu_available": snapshot.menu_available,
            "count": snapshot.items.len(),
            "truncated": snapshot.truncated,
            "visited": snapshot.visited,
            "elapsed_ms": snapshot.elapsed_ms,
            "items": snapshot.items,
        }))
        .unwrap_or_else(|_| format!("[list_menus window_id={window_id}]"));
        if !snapshot.menu_available {
            display.push_str(
                "\n\nThis app has no menu bar. Use `list_elements` with `element_ref` instead; do not retry.",
            );
        }
        ToolResult::ok(display)
    }

    /// 执行一个菜单项。
    fn press_menu(&self, input: &Value) -> ToolResult {
        let window_id = match required_u32(input, "window_id") {
            Ok(value) => value,
            Err(error) => return error_result(error),
        };
        let generation = match required_u64(input, "generation") {
            Ok(value) => value,
            Err(error) => return error_result(error),
        };
        let current = match wyj_computer::target::validate_window_target(window_id, generation) {
            Ok(target) => target,
            Err(error) => return error_result(error),
        };
        // 与 element_ref 同样的 stale 契约。
        let tree_id = match required_u64(input, "tree_id") {
            Ok(value) => value,
            Err(error) => return error_result(error),
        };
        let known = self
            .menu_tree_ids
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&window_id)
            .copied();
        let known = match known {
            Some(value) => value,
            None => {
                return error_result(anyhow!(
                    "target_changed: no menu snapshot for window {window_id}; call list_menus again"
                ))
            }
        };
        if known != tree_id {
            return error_result(anyhow!(
                "target_changed: menu tree_id {tree_id} is stale (the last list_menus returned {known}); call list_menus again"
            ));
        }

        let (lease, frontmost_before) = match self.begin_mutation(&current, "press_menu") {
            Ok(guard) => guard,
            Err(error) => return error_result(error),
        };
        // `menu_path` 给出从顶层到目标项的层级引用（如 [2, 4] 表示「文件 → 打开」），
        // 因为分层的编号只在**同级**内有效，跨层引用必须带完整路径。
        let path = match parse_menu_path(input) {
            Ok(path) => path,
            Err(error) => return error_result(error),
        };
        let action_result = wyj_computer::menu::press_menu_path(&current, &path);
        wyj_computer::telemetry::record_background_action();
        if let Err(error) = self.finish_mutation(&current, "press_menu", &lease, frontmost_before) {
            return error_result(error);
        }
        match action_result {
            Ok(content) => ToolResult::ok(content),
            Err(error) => error_result(error),
        }
    }

    fn click(&self, input: &Value) -> ToolResult {
        if has_element_ref(input) {
            return self.element_mutation(input, "click", ElementAction::Press);
        }
        self.coordinate_mutation(input, "click", |target, x, y| {
            wyj_computer::accessibility::press(target.pid, x, y)?;
            Ok("clicked with AXPress".to_string())
        })
    }

    fn set_text(&self, input: &Value) -> ToolResult {
        let text = match input.get("text").and_then(Value::as_str) {
            Some(text) => text,
            None => return ToolResult::err("missing required field `text`"),
        };
        if has_element_ref(input) {
            return self.element_mutation(input, "set_text", ElementAction::SetText(text));
        }
        self.coordinate_mutation(input, "set_text", |target, x, y| {
            let attribute = wyj_computer::accessibility::set_text(target.pid, x, y, text)?;
            Ok(format!("set text using {attribute}"))
        })
    }

    fn scroll(&self, input: &Value) -> ToolResult {
        let direction = match input.get("scroll_direction").and_then(Value::as_str) {
            Some(direction) => direction,
            None => return ToolResult::err("missing required field `scroll_direction`"),
        };
        let amount = input
            .get("scroll_amount")
            .and_then(Value::as_u64)
            .and_then(|amount| u32::try_from(amount).ok())
            .unwrap_or(1)
            .clamp(1, 20);
        if has_element_ref(input) {
            return self.element_mutation(
                input,
                "scroll",
                ElementAction::Scroll { direction, amount },
            );
        }
        self.coordinate_mutation(input, "scroll", |target, x, y| {
            if wyj_computer::accessibility::scroll(target.pid, x, y, direction, amount)? {
                Ok(format!(
                    "scrolled {direction} x{amount} using Accessibility"
                ))
            } else {
                wyj_computer::targeted_event::scroll(target.pid, direction, amount)?;
                Ok(format!(
                    "scrolled {direction} x{amount} using target PID event"
                ))
            }
        })
    }

    fn key(&self, input: &Value) -> ToolResult {
        let target = match self.resolve_observation(input) {
            Ok(target) => target,
            Err(error) => return error_result(error),
        };
        let combo = match input.get("key").and_then(Value::as_str) {
            Some(combo) if !combo.trim().is_empty() => combo,
            _ => return ToolResult::err("missing required field `key`"),
        };
        let (lease, frontmost_before) = match self.begin_mutation(&target, "key") {
            Ok(guard) => guard,
            Err(error) => return error_result(error),
        };
        let action_result = wyj_computer::targeted_event::key(target.pid, combo);
        wyj_computer::telemetry::record_background_action();
        if let Err(error) = self.finish_mutation(&target, "key", &lease, frontmost_before) {
            return error_result(error);
        }
        if let Err(error) = action_result {
            return error_result(error);
        }
        ToolResult::ok(format!("sent key `{combo}` to pid {}", target.pid))
    }

    /// 把 `element_ref` 解析成活的 AX 元素句柄，并强制校验 `tree_id`。
    ///
    /// stale 契约（对齐 Anthropic `ref`）：`element_ref` 只在产生它的那一次
    /// `list_elements` 里有效。窗口变了（generation 不符）、模型传了旧的 /
    /// 凭空编的 `tree_id`、或该 ref 超出当时的元素数，一律报
    /// `target_changed` 并要求重新列举——**绝不按序号回到当前树里盲找**，
    /// 那会把动作打到模型从未见过的控件上。
    fn resolve_element(&self, target: &WindowTarget, input: &Value) -> Result<ElementRef> {
        let reference = parse_element_ref(input)?;
        let tree_id = required_u64(input, "tree_id")?;
        let current = self
            .element_tree_ids
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&target.window_id)
            .copied();
        let current = current.ok_or_else(|| {
            anyhow!(
                "target_changed: no element snapshot for window {}; call list_elements again",
                target.window_id
            )
        })?;
        // stale 契约的核心：ref 只能配合产生它的那次 tree_id 使用。
        // 对不上就要求重新列举，绝不按序号回到当前树里盲找元素。
        anyhow::ensure!(
            current == tree_id,
            "target_changed: tree_id {tree_id} is stale (the last list_elements returned {current}); call list_elements again"
        );
        Ok(reference)
    }

    /// 句柄寻址的变更动作统一入口。
    ///
    /// 与坐标版本共用同一套安全闸门（`begin_mutation` 拿输入租约 +
    /// `finish_mutation` 校验前台未被抢走），这是本仓库最不该被绕过的一条
    /// 不变式：换一种寻址方式不等于换一种权限语义。
    fn element_mutation(
        &self,
        input: &Value,
        action_name: &str,
        action: ElementAction<'_>,
    ) -> ToolResult {
        let target = match self.resolve_observation(input) {
            Ok(target) => target,
            Err(error) => return error_result(error),
        };
        let reference = match self.resolve_element(&target, input) {
            Ok(reference) => reference,
            Err(error) => return error_result(error),
        };

        let (lease, frontmost_before) = match self.begin_mutation(&target, action_name) {
            Ok(guard) => guard,
            Err(error) => return error_result(error),
        };
        // 重新遍历并按序号定位：句柄绝不跨 crate 边界（AXUIElement 非 Send/Sync），
        // 定位与使用都在 `wyj_computer` 内部一次完成。
        let action_result = wyj_computer::element::execute_with_element(
            &target,
            &TraversalBudget::default(),
            reference,
            action,
        );
        wyj_computer::telemetry::record_background_action();
        if let Err(error) = self.finish_mutation(&target, action_name, &lease, frontmost_before) {
            return error_result(error);
        }
        match action_result {
            Ok(content) => ToolResult::ok(content),
            Err(error) => error_result(error),
        }
    }

    fn coordinate_mutation<F>(&self, input: &Value, action_name: &str, action: F) -> ToolResult
    where
        F: FnOnce(&WindowTarget, f64, f64) -> Result<String>,
    {
        let target = match self.resolve_observation(input) {
            Ok(target) => target,
            Err(error) => return error_result(error),
        };
        let (x, y) = match required_coordinate(input) {
            Ok(coordinate) => coordinate,
            Err(error) => return error_result(error),
        };
        let (global_x, global_y) = match target.screenshot_to_global(x, y) {
            Ok(coordinate) => coordinate,
            Err(error) => return error_result(error),
        };
        let (lease, frontmost_before) = match self.begin_mutation(&target, action_name) {
            Ok(guard) => guard,
            Err(error) => return error_result(error),
        };
        let action_result = action(&target, global_x, global_y);
        wyj_computer::telemetry::record_background_action();
        if let Err(error) = self.finish_mutation(&target, action_name, &lease, frontmost_before) {
            return error_result(error);
        }
        match action_result {
            Ok(content) => ToolResult::ok(content),
            Err(error) => error_result(error),
        }
    }
}

const DESCRIPTION: &str = "Controls a specific macOS application window in the background without moving the user's pointer or activating the app. Prefer this over the legacy `computer` tool. Workflow: `list_windows` or `screenshot` to capture a window, then `list_elements` to read its accessibility tree and get stable `element_ref` ids (always paired with a screenshot, and much more reliable than guessing coordinates). To run an application command (File then New Tab, View then Show Path Bar, Window then Minimize), prefer the menu bar: `list_menus` returns every menu item with a `menu_ref` (e.g. `m12`) and a readable `path`, and `press_menu` activates it in the background without moving the pointer, clicking a control, or activating the app. Note that `list_menus` covers the app's menu bar only, not right-click context menus. For actions on window controls pass `window_id`, `generation`, the `tree_id` from your `list_elements` call, and `element_ref` (e.g. `e12`); pass `coordinate` only when `list_elements` reports `root_available:false`, the element you need is missing, or `truncated:true` hid it. A stale `tree_id` or an out-of-range `element_ref` returns `target_changed` — call `list_elements` again rather than retrying the old reference. `click` uses AXPress; `set_text` writes AXValue/AXSelectedText; `key` and unsupported semantic scrolling are posted only to the target PID. This tool NEVER silently falls back to global mouse/keyboard input. On `target_changed`, capture again. On `requires_foreground_takeover`, `preempted_by_user`, `user_active`, or permission errors, stop and report instead of retrying.";

#[async_trait]
impl Tool for AppComputerTool {
    fn name(&self) -> &str {
        "app_computer"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: DESCRIPTION.to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["list_windows", "screenshot", "list_elements", "list_menus", "inspect_element", "click", "set_text", "press_menu", "key", "scroll"]
                    },
                    "window_id": {"type": "integer", "minimum": 0},
                    "generation": {"type": "integer", "minimum": 0},
                    "target_width": {"type": "integer", "minimum": 1},
                    "target_height": {"type": "integer", "minimum": 1},
                    "tree_id": {"type": "integer", "minimum": 0},
                    "element_ref": {"type": "string", "pattern": "^e[0-9]+$"},
                    "menu_ref": {"type": "string", "pattern": "^m[0-9]+$"},
                    "expand": {"type": "string", "pattern": "^m[0-9]+$"},
                    "menu_path": {
                        "type": "array",
                        "items": {"type": "string", "pattern": "^m[0-9]+$"},
                        "minItems": 1,
                        "description": "Full menu hierarchy from the top level, e.g. [\"m2\", \"m4\"] for File then Open."
                    },
                    "coordinate": {
                        "type": "array",
                        "items": {"type": "integer"},
                        "minItems": 2,
                        "maxItems": 2
                    },
                    "text": {"type": "string"},
                    "key": {"type": "string"},
                    "scroll_direction": {
                        "type": "string",
                        "enum": ["up", "down", "left", "right"]
                    },
                    "scroll_amount": {"type": "integer", "minimum": 1, "maximum": 20}
                },
                "required": ["action"]
            }),
            native: None,
        }
    }

    fn needs_permission(&self, input: &Value) -> bool {
        !matches!(
            input.get("action").and_then(Value::as_str),
            Some(
                "list_windows" | "screenshot" | "list_elements" | "list_menus" | "inspect_element"
            )
        )
    }

    fn action_summary(&self, input: &Value) -> String {
        let action = input
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let window = input
            .get("window_id")
            .and_then(Value::as_u64)
            .map(|id| format!("window {id}"))
            .unwrap_or_else(|| "unknown window".to_string());
        format!("{action} in background on {window}")
    }

    async fn run(&self, input: Value, _ctx: &dyn ToolContext) -> Result<ToolResult> {
        let action = input
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("missing `action` field"))?;
        Ok(match action {
            "list_windows" => match wyj_computer::target::list_windows() {
                Ok(windows) => ToolResult::ok(
                    serde_json::to_string_pretty(&serde_json::json!({
                        "count": windows.len(),
                        "windows": windows,
                    }))
                    .unwrap_or_else(|_| "{\"windows\":[]}".to_string()),
                ),
                Err(error) => error_result(error),
            },
            "screenshot" => self.screenshot(&input),
            "inspect_element" => self.inspect(&input),
            "list_elements" => self.list_elements(&input),
            "list_menus" => self.list_menus(&input),
            "press_menu" => self.press_menu(&input),
            "click" => self.click(&input),
            "set_text" => self.set_text(&input),
            "key" => self.key(&input),
            "scroll" => self.scroll(&input),
            other => ToolResult::err(format!("unsupported app_computer action `{other}`")),
        })
    }
}

fn required_u32(input: &Value, field: &str) -> Result<u32> {
    input
        .get(field)
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| anyhow!("missing or invalid required field `{field}`"))
}

fn required_u64(input: &Value, field: &str) -> Result<u64> {
    input
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("missing or invalid required field `{field}`"))
}

/// 输入是否带了元素引用（而非坐标）。
pub(crate) fn has_element_ref(input: &Value) -> bool {
    input
        .get("element_ref")
        .and_then(Value::as_str)
        .is_some_and(|r| !r.trim().is_empty())
}

/// 解析 `"e12"` 形式的元素引用。
fn parse_element_ref(input: &Value) -> Result<ElementRef> {
    let raw = input
        .get("element_ref")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing required field `element_ref`"))?;
    let digits = raw
        .strip_prefix('e')
        .or_else(|| raw.strip_prefix('E'))
        .ok_or_else(|| anyhow!("invalid `element_ref` `{raw}`: expected the form `e12`"))?;
    let index: u32 = digits
        .parse()
        .map_err(|_| anyhow!("invalid `element_ref` `{raw}`: expected the form `e12`"))?;
    Ok(ElementRef(index))
}

/// 解析 `"m12"` 形式的菜单引用。
fn parse_menu_ref_str(raw: &str) -> Result<wyj_computer::menu::MenuRef> {
    let digits = raw
        .strip_prefix('m')
        .or_else(|| raw.strip_prefix('M'))
        .ok_or_else(|| anyhow!("invalid menu reference `{raw}`: expected the form `m12`"))?;
    let index: u32 = digits
        .parse()
        .map_err(|_| anyhow!("invalid menu reference `{raw}`: expected the form `m12`"))?;
    Ok(wyj_computer::menu::MenuRef(index))
}

/// 解析 `menu_path`（如 `[2, 4]` = 顶层第 3 项下的第 5 项）。
///
/// 分层菜单的编号只在同级内有效，所以动作必须带**完整层级路径**而不是单个
/// 引用——这与 `element_ref` 的单层语义不同，也是菜单比控件更严格的地方。
fn parse_menu_path(input: &Value) -> Result<Vec<wyj_computer::menu::MenuRef>> {
    let raw = input
        .get("menu_path")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing required field `menu_path` (array of menu references)"))?;
    // 空路径必须在参数校验阶段就拒掉——否则会一路走到 `press_menu_path` 的
    // `split_first()` 才失败，白白跑完一次租约与前台校验。
    anyhow::ensure!(!raw.is_empty(), "`menu_path` must contain at least one reference");
    raw.iter()
        .map(|item| {
            item.as_str()
                .ok_or_else(|| anyhow!("invalid `menu_path` entry: expected a string like `m2`"))
                .and_then(parse_menu_ref_str)
        })
        .collect()
}

fn required_coordinate(input: &Value) -> Result<(i32, i32)> {
    let coordinate = input
        .get("coordinate")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing required field `coordinate`"))?;
    anyhow::ensure!(
        coordinate.len() == 2,
        "`coordinate` must contain exactly 2 integers"
    );
    let x = coordinate[0]
        .as_i64()
        .and_then(|value| i32::try_from(value).ok())
        .ok_or_else(|| anyhow!("invalid coordinate x"))?;
    let y = coordinate[1]
        .as_i64()
        .and_then(|value| i32::try_from(value).ok())
        .ok_or_else(|| anyhow!("invalid coordinate y"))?;
    Ok((x, y))
}

fn error_result(error: impl std::fmt::Display) -> ToolResult {
    let message = error.to_string();
    wyj_computer::telemetry::record_error_message(&message);
    let code = [
        "target_changed",
        "requires_foreground_takeover",
        "preempted_by_user",
        "user_active",
        "screen_locked",
        "input_monitor_unavailable",
        "accessibility_permission_required",
    ]
    .into_iter()
    .find(|code| message.contains(code))
    .unwrap_or("app_computer_failed");
    ToolResult::err(
        serde_json::to_string_pretty(&serde_json::json!({
            "error": {
                "code": code,
                "message": message,
                "automatic_retry": false,
                "foreground_fallback_used": false,
            }
        }))
        .unwrap_or(message),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definition_is_custom_and_background_explicit() {
        let definition = AppComputerTool::new(1280, Duration::from_secs(2)).definition();
        assert!(definition.native.is_none());
        assert!(definition.description.contains("NEVER silently falls back"));
        assert_eq!(
            definition.input_schema["properties"]["action"]["enum"][0],
            "list_windows"
        );
    }

    #[test]
    fn read_only_actions_do_not_request_permission() {
        let tool = AppComputerTool::new(1280, Duration::from_secs(2));
        assert!(!tool.needs_permission(&serde_json::json!({"action": "screenshot"})));
        // 列举元素只读 AX 树，不动输入，必须与截图同属免权限档。
        assert!(!tool.needs_permission(&serde_json::json!({"action": "list_elements"})));
        assert!(tool.needs_permission(&serde_json::json!({"action": "click"})));
    }

    #[test]
    fn definition_exposes_list_elements_and_element_addressing() {
        let definition = AppComputerTool::new(1280, Duration::from_secs(2)).definition();
        let actions = definition.input_schema["properties"]["action"]["enum"]
            .as_array()
            .expect("action enum");
        assert!(actions.iter().any(|a| a == "list_elements"));
        // 元素寻址与树标识必须出现在 schema 里，模型才知道能传什么。
        let properties = &definition.input_schema["properties"];
        assert!(properties.get("tree_id").is_some());
        assert!(properties.get("element_ref").is_some());
        // 描述必须把 ref 路线讲成首选，否则模型仍会去猜像素。
        assert!(definition.description.contains("element_ref"));
    }

    #[test]
    fn element_ref_parsing_accepts_only_the_documented_form() {
        assert_eq!(
            parse_element_ref(&serde_json::json!({"element_ref": "e12"})).unwrap(),
            ElementRef(12)
        );
        assert_eq!(
            parse_element_ref(&serde_json::json!({"element_ref": "E0"})).unwrap(),
            ElementRef(0)
        );
        // 形状不对必须拒绝，而不是默默退回坐标路线。
        for bad in ["12", "e", "e-1", "e12x", "", "ref_2"] {
            assert!(
                parse_element_ref(&serde_json::json!({ "element_ref": bad })).is_err(),
                "{bad} should be rejected"
            );
        }
        assert!(parse_element_ref(&serde_json::json!({})).is_err());
    }

    #[test]
    fn element_ref_takes_precedence_over_coordinate() {
        // 两者都给了时按语义寻址走：坐标是旧的猜测，ref 是模型确认过的目标。
        assert!(has_element_ref(&serde_json::json!({
            "element_ref": "e3",
            "coordinate": [10, 20],
        })));
        assert!(has_element_ref(&serde_json::json!({"element_ref": "e3"})));
        assert!(!has_element_ref(
            &serde_json::json!({"coordinate": [10, 20]})
        ));
        // 空白不算提供了 ref。
        assert!(!has_element_ref(&serde_json::json!({"element_ref": "  "})));
    }

    #[test]
    fn stale_tree_id_is_rejected_before_any_mutation() {
        let tool = AppComputerTool::new(1280, Duration::from_secs(2));
        let target = WindowTarget {
            window_id: 7,
            pid: 42,
            app_name: "App".into(),
            title: "T".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
            target_width: 400,
            target_height: 300,
            generation: 3,
            focused: false,
        };
        // 从未 list_elements 过：任何 element_ref 都必须被拒。
        let error = tool
            .resolve_element(
                &target,
                &serde_json::json!({"element_ref": "e0", "tree_id": 1}),
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("no element snapshot"), "{error}");
        assert!(error.contains("list_elements"), "{error}");
    }

    #[test]
    fn element_actions_still_require_permission() {
        // 换一种寻址方式不等于换一种权限语义：ref 路线必须与坐标路线同档。
        let tool = AppComputerTool::new(1280, Duration::from_secs(2));
        for action in ["click", "set_text", "scroll"] {
            let mut input =
                serde_json::json!({"action": action, "element_ref": "e1", "tree_id": 9});
            if action == "set_text" {
                input["text"] = serde_json::json!("hello");
            }
            assert!(
                tool.needs_permission(&input),
                "{action} via element_ref must still need permission"
            );
        }
    }

    #[test]
    fn action_summary_surfaces_element_addressing() {
        // 审批弹窗要让人看出这是「点了哪个元素」，而不是一团坐标。
        let tool = AppComputerTool::new(1280, Duration::from_secs(2));
        let summary = tool.action_summary(
            &serde_json::json!({"action": "click", "window_id": 5, "element_ref": "e12"}),
        );
        assert!(summary.contains("click"), "{summary}");
        assert!(summary.contains("window 5"), "{summary}");
    }

    #[test]
    fn menu_reference_parsing_rejects_wrong_forms() {
        let parse = |raw: &str| parse_menu_ref_str(raw);
        assert_eq!(parse("m3").unwrap(), wyj_computer::menu::MenuRef(3));
        assert_eq!(parse("M0").unwrap(), wyj_computer::menu::MenuRef(0));
        for bad in ["3", "m", "e3", "mx", ""] {
            assert!(parse(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn menu_path_requires_the_full_hierarchy() {
        // 分层菜单的编号只在同级内有效，所以动作必须带完整路径。
        let path = parse_menu_path(&serde_json::json!({"menu_path": ["m2", "m4"]})).unwrap();
        assert_eq!(path.len(), 2);
        assert_eq!(path[0], wyj_computer::menu::MenuRef(2));
        assert_eq!(path[1], wyj_computer::menu::MenuRef(4));
        // 缺字段、空数组、非字符串都要拒绝。
        assert!(parse_menu_path(&serde_json::json!({})).is_err());
        assert!(parse_menu_path(&serde_json::json!({"menu_path": []})).is_err());
        assert!(parse_menu_path(&serde_json::json!({"menu_path": [3]})).is_err());
    }

    #[test]
    fn menu_actions_follow_the_same_permission_rules() {
        let tool = AppComputerTool::new(1280, Duration::from_secs(2));
        // 列举菜单只读，免权限。
        assert!(!tool.needs_permission(&serde_json::json!({"action": "list_menus"})));
        // 执行菜单项是副作用，必须与 click 同档。
        assert!(tool.needs_permission(
            &serde_json::json!({"action": "press_menu", "menu_ref": "m1", "tree_id": 5})
        ));
    }

    #[test]
    fn stale_menu_tree_id_is_rejected_before_any_mutation() {
        let tool = AppComputerTool::new(1280, Duration::from_secs(2));
        let result = tool.press_menu(&serde_json::json!({
            "action": "press_menu",
            "window_id": 999_999,
            "generation": 1,
            "menu_ref": "m0",
            "tree_id": 1,
        }));
        assert!(result.is_error);
        // 窗口都不存在，绝不能走到「按序号盲找菜单项」。
        assert!(
            result.content.contains("target_changed"),
            "{}",
            result.content
        );
    }

    #[test]
    fn errors_are_structured_and_never_claim_foreground_fallback() {
        let result = error_result(anyhow!("target_changed: moved"));
        assert!(result.is_error);
        assert!(result.content.contains("target_changed"));
        assert!(result
            .content
            .contains("\"foreground_fallback_used\": false"));
    }
}
