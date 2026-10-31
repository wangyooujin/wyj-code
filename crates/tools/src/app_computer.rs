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

/// 一次 `list_elements` 留下的、动作侧需要的全部可 `Send` 观察信息。
#[derive(Debug, Clone, Default)]
struct ElementObservation {
    /// 产生这批 ref 的那次遍历的 id。
    tree_id: u64,
    /// 那一次遍历的树内容指纹，动作前用来检测界面是否已经漂移。
    content_fingerprint: u64,
    /// 每个 ref 在截图坐标空间里的中心点，下标即 `ElementRef` 的序号。
    ///
    /// 存它是为了让「AX 动作在这个元素上必然失败」时能降级到坐标点击：
    /// 元素明明在树里、`frame` 也有，只是不支持 `AXPress`（自绘控件常见），
    /// 这时让模型自己去查坐标再发一次 `click` 是纯粹多出来的往返。
    ///
    /// 只存两个 `i32`，不存 AX 句柄——`AXUIElement` 非 `Send`/`Sync`，一旦
    /// 进这张表整个工具就无法跨线程共享。
    centers: Vec<Option<(i32, i32)>>,
    /// 产生这批 ref 时用的过滤条件。
    ///
    /// **必须原样传给动作路径**：`element_ref` 的序号是「过滤后列表里的第
    /// N 个」，动作时若改用无过滤遍历，同一个 `e3` 会指向完整树里的另一个
    /// 元素——ref 契约禁止的按序号盲点。
    query: ElementQuerySnapshot,
}

/// [`ElementQuery`] 的可克隆快照（原类型含 `Vec<String>`，本身可克隆，
/// 这里单独命名是为了让「这是观察记录的一部分」在类型上可见）。
type ElementQuerySnapshot = wyj_computer::element::ElementQuery;

impl ElementObservation {
    fn center_of(&self, reference: ElementRef) -> Option<(i32, i32)> {
        self.centers.get(reference.0 as usize).copied().flatten()
    }
}

pub struct AppComputerTool {
    max_dim: u32,
    focused_quiet_period: Duration,
    observations: Mutex<HashMap<u32, WindowTarget>>,
    /// 每个窗口最近一次 `list_elements` 的观察记录，供动作校验 ref 是否过期。
    ///
    /// **只存可 `Send` 的纯数据，不存 AX 句柄**：`AXUIElement` 不是 `Send`/`Sync`
    /// （底层 `UnsafeCell`），存进本工具的 `Mutex` 会让 `Arc<dyn Tool>` 失去
    /// 跨线程可用性。句柄的定位与使用因此完全下沉到 `wyj_computer::element`
    /// 内部完成（见 [`element_mutation`]）。
    ///
    /// 存的是**两项**而不是只有 `tree_id`：`tree_id` 含遍历时刻，只能证明
    /// 「这个 ref 来自哪一次观察」；真正能检测「观察之后界面变了」的是
    /// `content_fingerprint`。只校验前者等于放行按序号盲点。
    element_observations: Mutex<HashMap<u32, ElementObservation>>,
    /// 每个窗口最近一次 `list_menus` 的 `tree_id`，同理用于菜单引用的 stale 校验。
    menu_tree_ids: Mutex<HashMap<u32, u64>>,
    /// 某 App/动作若曾在没有人类输入的情况下抢走前台，本会话立即熔断该
    /// 组合，避免同一种不兼容后台动作反复打扰用户。
    incompatible_actions: Mutex<HashSet<(String, String)>>,
    /// 变更动作成功后是否自动附带一张窗口截图，见 [`Self::with_auto_screenshot`]。
    auto_screenshot: Option<u32>,
    /// 自上次截图以来的连续变更动作数，用于 `MAX_ACTIONS_WITHOUT_SCREENSHOT`。
    mutations_since_screenshot: Mutex<HashMap<u32, u32>>,
    /// 本次会话能否把截图真正送到模型眼前。
    ///
    /// 只由 `profile.vision` 决定：两条线现在都能送达（Anthropic 内嵌
    /// `tool_result`；OpenAI 由 `openai.rs::to_api_messages` 摘出来后置成 user
    /// 消息）。`vision=false` 时附图不是「模型看到了画面」，而是「模型以为自己
    /// 看到了画面」，比不附更糟。
    image_channel: bool,
}

/// 连续变更动作的上限。超过之后即使 `auto_screenshot` 关闭也强制附图一次。
///
/// 旧 `computer` 工具早就有这条保护（`computer.rs` 的
/// `MAX_ACTIONS_WITHOUT_SCREENSHOT`），`app_computer` 从上线起一直缺。它防的
/// 是同一个失效模式：模型连续盲点十几下、每步都「成功」，直到某一步点到
/// 完全不相干的地方才停下来问「为什么没反应」——那时已经无从回溯哪一步开始
/// 偏的。
const MAX_ACTIONS_WITHOUT_SCREENSHOT: u32 = 20;

/// `wait` 动作的上限。
///
/// 设上限是因为这个动作在回合里是**同步阻塞**的：模型误判一次「等 30 秒」，
/// 用户就真的盯着一个不动的界面 30 秒，而且这期间 ESC 只能取消整回合。3000ms
/// 覆盖界面过渡动画与本地加载，更长的情况该用 `wait` 多调几次、每次之间截图
/// 确认——那才是模型真正需要的行为。
const MAX_WAIT_MS: u64 = 3000;

impl AppComputerTool {
    pub fn new(max_dim: u32, focused_quiet_period: Duration) -> Self {
        Self {
            max_dim,
            focused_quiet_period,
            observations: Mutex::new(HashMap::new()),
            element_observations: Mutex::new(HashMap::new()),
            menu_tree_ids: Mutex::new(HashMap::new()),
            incompatible_actions: Mutex::new(HashSet::new()),
            auto_screenshot: None,
            mutations_since_screenshot: Mutex::new(HashMap::new()),
            image_channel: true,
        }
    }

    /// 声明本 profile **没有**图片回传通道，工具只剩文本路径。
    ///
    /// 用于 `profile.vision = false`：图片发不出去（Anthropic 线会降级成
    /// `[image omitted: ...]` 文本块，OpenAI 线同样只留占位符），于是「附图」
    /// 在模型眼里等于「这里本该有张图，但你看到的是占位符」。与其让模型对着
    /// 一张它看不见的图推理，不如把图片整个撤掉、只给 Accessibility 文本与
    /// OCR 文本，并在回执里说清楚这是能力边界。
    pub fn without_image_channel(mut self) -> Self {
        self.image_channel = false;
        self.auto_screenshot = None;
        self
    }

    /// 把截图附进回执。没有图片通道时退化成「文本 + 一句能力说明」。
    fn attach_capture(&self, display: String, png_base64: String) -> ToolResult {
        if !self.image_channel {
            return ToolResult::ok(format!(
                "{display}\n\n[no image attached: this profile cannot receive images, so only \
                 the text above is available. Do not retry expecting a picture — read the \
                 accessibility text (`list_elements` with `include_text:true`) or OCR text \
                 (`read_text`) instead.]\n\
                 本 profile 无法接收图片，以上文本即全部信息，不要重试等图片。"
            ));
        }
        ToolResult::with_parts(
            display,
            vec![ToolResultPart::Image {
                media_type: "image/png".to_string(),
                data: png_base64,
            }],
        )
    }

    /// 开启「变更动作后自动附带截图」，`max_dim` 为附图的长边像素上限。
    ///
    /// 为什么这件事必须由工具自己做、而不是靠提示词要求模型「记得截图」：
    /// 模型对「我刚刚做了什么」有强烈的一致性倾向，它更愿意相信自己的动作
    /// 成功了，而不是主动去验证。让工具在回执里直接给出**动作之后的画面**，
    /// 是把验证从「模型的自觉」变成「协议的默认」。
    pub fn with_auto_screenshot(mut self, max_dim: u32) -> Self {
        self.auto_screenshot = Some(max_dim);
        self
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
        // 显式截图本身就是一次观察，连续动作计数归零。
        self.mutations_since_screenshot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(window_id, 0);
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
        self.attach_capture(display, capture.png_base64())
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
        let (x, y) = match required_coordinate(input, "coordinate") {
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

    /// 用系统 OCR 读出窗口里的文字（带像素坐标）。
    ///
    /// # 这是**兜底**通道，不是首选
    ///
    /// 有 AX 文本时用 `list_elements` + `include_text`：那条路 0.2~0.9 秒、
    /// 文本直接来自系统语义树、无识别误差。`read_text` 只在 AX 树空洞时才有
    /// 价值——原生自绘应用（微信）、canvas 图表、视频字幕、图片里的文字。
    ///
    /// # 为什么必须传原始像素图
    ///
    /// 实测同一张 4K 截屏：原始像素下识别 114 行、中文基本完整；缩到 1280
    /// 宽后错字明显增加（`Fille` / `seripts` / `pubLish`），而**耗时不变**
    /// （0.91s 是固定底价）。因此这里刻意用 `u32::MAX` 关闭下采样，代价是
    /// 多传几十毫秒的截图，换来可用得多的识别结果。
    ///
    /// # 结果必须标注是机器识别
    ///
    /// 置信度实测常落在 0.3~0.5，中文尤其容易串行（「标普500ETF」会读成
    /// 「さ标普500ETF南方」）。因此回执里逐行给置信度，并在末尾明确告诉
    /// 模型「这是 OCR，不是原文，数值要二次核对」——否则模型会把它当成
    /// 读到的确定事实。
    fn read_text(&self, input: &Value) -> ToolResult {
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
        // 阈值只接受 [0,1]；传 `"0.4"` 这类字符串说明模型理解偏了，显式报错
        // 而不是静默当成 0（那会让它以为过滤掉了噪音、实际没有）。
        let min_confidence = match input.get("min_confidence") {
            Some(value) => {
                let raw = match value.as_f64() {
                    Some(raw) => raw,
                    None => return error_result("`min_confidence` must be a number"),
                };
                if !(0.0..=1.0).contains(&raw) {
                    return error_result("`min_confidence` must be between 0 and 1");
                }
                raw as f32
            }
            None => 0.0,
        };

        // 不缩放：OCR 的准确率依赖原始像素，而耗时是固定底价。
        let (target, capture) =
            match wyj_computer::target::capture_window_by_id(window_id, u32::MAX) {
                Ok(result) => result,
                Err(error) => return error_result(error),
            };
        self.observations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(window_id, target.clone());
        // `read_text` 同样附截图，也算一次观察——连续变更计数归零，
        // 否则模型连做 20 次只读观察后会被迫多付一张强制截图。
        self.mutations_since_screenshot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(window_id, 0);

        let result = match wyj_computer::ocr::recognize_png(&capture.png) {
            Ok(result) => result,
            Err(error) => return error_result(error),
        };
        let kept = result
            .lines
            .iter()
            .filter(|line| line.confidence >= min_confidence)
            .count();

        let mut display = format!(
            "tree_id=none (OCR reads pixels, not the accessibility tree — use `list_elements` \
             with `include_text:true` when the app exposes text) window={} image={}x{} \
             lines={} kept={} elapsed={}ms\n",
            window_id,
            result.image_width,
            result.image_height,
            result.lines.len(),
            kept,
            result.elapsed_ms
        );
        display.push_str("conf  @centre        text\n");
        // 按阅读顺序排序：先纵后横。Vision 返回的顺序大致按置信度，直接给
        // 模型会得到一堆跳跃的行——而模型读表格/列表时依赖上下文的先后。
        let mut lines: Vec<_> = result
            .lines
            .iter()
            .filter(|line| line.confidence >= min_confidence)
            .collect();
        lines.sort_by_key(|line| (line.center.1 / 8, line.center.0));
        for line in &lines {
            display.push_str(&format!(
                "{:.2}  ({:>5},{:>5})  {}\n",
                line.confidence, line.center.0, line.center.1, line.text
            ));
        }
        if result.truncated {
            display.push_str(&format!(
                "\nTruncated at {} lines. Narrow the window or read a smaller region.\n",
                wyj_computer::ocr::MAX_LINES
            ));
        }
        display.push_str(
            "\nThese lines were recognized from pixels, not read from the UI model. \
             Confidence is per-line; Chinese and dense digits are the least reliable \
             (a number that looks right can still be a misread). Verify before acting \
             on a value. Coordinates are pixel positions in this capture — the same \
             space `coordinate` actions accept for this window.\n\
             以下文本由 OCR 从像素识别而来，不是从界面模型读到的。数值与中文尤其容易误识，\
             据此操作前请二次核对。坐标是本截图内的像素坐标，可直接用于 `coordinate` 动作。",
        );

        self.attach_capture(display, capture.png_base64())
    }

    /// 执行一条编译期固定的 AppleScript recipe。
    ///
    /// # 为什么这是 action 而不是独立工具
    ///
    /// 它做的是「控制某个应用」，与 `app_computer` 同一职责面，因此复用同一套
    /// 权限确认（`needs_permission` 里刻意**不**放行）：每次调用都要用户点头。
    /// 独立工具会得到另一份 `allowed_tools.json` 条目，用户可以整体放行它，
    /// 那正是这条路径最不该拥有的自由度。
    ///
    /// # 交互面是硬前置
    ///
    /// 首次调用某个目标 App 时 macOS 会弹一次 TCC 模态框。headless / 子 Agent /
    /// 定时任务里没人能点它，调用会挂到超时——因此这里显式要求交互面，与
    /// `computer` 的 `requires_foreground_takeover` 判据同源。
    fn run_recipe(&self, input: &Value, ctx: &dyn ToolContext) -> ToolResult {
        if !ctx.supports_interactive_confirmation() {
            return error_result(
                "requires_foreground_takeover: recipes may prompt for macOS Automation \
                 permission, which needs a person to accept the system dialog. This \
                 execution surface has no interactive confirmation channel.",
            );
        }
        let name = match input.get("recipe").and_then(Value::as_str) {
            Some(name) => name,
            None => return error_result("`run_recipe` requires a `recipe` name"),
        };
        let recipe = match wyj_computer::applescript::require(name) {
            Ok(recipe) => recipe,
            Err(error) => return error_result(error),
        };
        let args: Vec<String> = match input.get("args") {
            Some(value) => match value.as_array() {
                Some(items) => {
                    let mut out = Vec::with_capacity(items.len());
                    for item in items {
                        match item.as_str() {
                            Some(text) => out.push(text.to_string()),
                            None => return error_result("`args` entries must be strings"),
                        }
                    }
                    out
                }
                None => return error_result("`args` must be an array of strings"),
            },
            // 少数 recipe 可能零参数；目前表里没有，但不要在这里写死前提。
            None => Vec::new(),
        };
        if let Err(error) = wyj_computer::applescript::validate_args(recipe, &args) {
            return error_result(error);
        }

        match wyj_computer::applescript::run(recipe, &args) {
            Ok(output) => {
                let mut display = format!("recipe `{name}` completed: {}", output.stdout);
                if !output.stderr.is_empty() {
                    // 脚本成功但有 stderr（比如 Finder 的警告）时一并给出：
                    // 模型需要知道「做成了，但有异常」。
                    display.push_str(&format!("\nstderr: {}", output.stderr));
                }
                ToolResult::ok(display)
            }
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
        // `list_elements` 也附截图，同样是一次观察。
        self.mutations_since_screenshot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(window_id, 0);

        let query = match parse_element_query(input) {
            Ok(query) => query,
            Err(error) => return error_result(error),
        };
        let snapshot = match wyj_computer::element::snapshot_elements_with(
            &target,
            &TraversalBudget::default(),
            &query,
        ) {
            Ok(snapshot) => snapshot,
            Err(error) => return error_result(error),
        };
        // 必须记录观察结果，否则下面所有 `element_ref` 动作都会因
        // `no element snapshot` 而恒定失败——`resolve_element` 拿这个值校验
        // 引用的新鲜度，它只可能由 `list_elements` 写入（`list_menus` 写的是
        // 另一张 `menu_tree_ids` 表，两者互不通用）。
        //
        // 这是个静默 bug：写在这里之前，`element_ref` 路线从上线起一次都没能
        // 执行成功，而工具描述与 system prompt 都把它称作「首选」。现有单测
        // 只覆盖「从未 list 过」的 None 分支，正好测不到「list 过却仍然为空」。
        self.element_observations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                window_id,
                ElementObservation {
                    tree_id: snapshot.tree_id,
                    content_fingerprint: snapshot.content_fingerprint,
                    centers: snapshot
                        .elements
                        .iter()
                        .map(|element| {
                            element.frame_in_screenshot.map(|frame| {
                                let (x, y) = frame.center();
                                (x.round() as i32, y.round() as i32)
                            })
                        })
                        .collect(),
                    query: query.clone(),
                },
            );

        let mut display = render_element_list(&snapshot, &capture, wants_json(input));

        if !snapshot.root_available {
            display.push_str(
                "\n\nThis app exposes no accessibility elements in this window. Coordinate \
                 actions still work and fall back automatically: `click` warps the cursor to \
                 the point, clicks, and returns the cursor; `set_text` and `scroll` degrade \
                 similarly. Do not retry `list_elements`.\n\n\
                 该应用在此窗口内不暴露辅助功能元素。坐标操作仍可用且会自动降级：\
                 `click` 会把光标瞬移到目标点、点击、再把光标移回原处；\
                 `set_text` 与 `scroll` 同理降级。不要重试 `list_elements`。",
            );
        } else if snapshot.truncated {
            // 没有图片通道时不能说「用附带的截图」——那张截图模型看不到。
            let hint = if self.image_channel {
                "Use the attached screenshot with `coordinate`, or narrow the target window, \
                 to reach the rest."
            } else {
                "There is no attached screenshot on this profile — narrow the target window, \
                 raise the element quota, or retry with a `role_filter` to reach the rest."
            };
            display.push_str(&format!(
                "\n\nPartial result: the element tree exceeded the traversal budget, so some \
                 elements are missing. {hint}"
            ));
        }

        self.attach_capture(display, capture.png_base64())
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
            Ok(content) => self.with_after_screenshot(&current, content),
            Err(error) => error_result(error),
        }
    }

    fn click(&self, input: &Value) -> ToolResult {
        let kind = click_kind_from(input);
        if has_element_ref(input) {
            return self.element_mutation(
                input,
                "click",
                ElementAction::Press,
                Some(|target: &WindowTarget, x: f64, y: f64| {
                    click_with_fallback(target, x, y, kind)
                }),
            );
        }
        self.coordinate_mutation(input, "click", |target, x, y| {
            click_with_fallback(target, x, y, kind)
        })
    }

    fn set_text(&self, input: &Value) -> ToolResult {
        let text = match input.get("text").and_then(Value::as_str) {
            Some(text) => text,
            None => return ToolResult::err("missing required field `text`"),
        };
        if has_element_ref(input) {
            return self.element_mutation(
                input,
                "set_text",
                ElementAction::SetText(text),
                Some(|target: &WindowTarget, x: f64, y: f64| {
                    type_text_with_fallback(target, x, y, text)
                }),
            );
        }
        self.coordinate_mutation(input, "set_text", |target, x, y| {
            type_text_with_fallback(target, x, y, text)
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
                Some(|target: &WindowTarget, _x: f64, _y: f64| {
                    scroll_with_fallback(target, direction, amount)
                }),
            );
        }
        self.coordinate_mutation(input, "scroll", |target, x, y| {
            match wyj_computer::accessibility::scroll(target.pid, x, y, direction, amount) {
                Ok(true) => Ok(format!(
                    "scrolled {direction} x{amount} using Accessibility"
                )),
                Ok(false) | Err(_) => {
                    wyj_computer::targeted_event::scroll(target.pid, direction, amount)?;
                    Ok(format!(
                        "scrolled {direction} x{amount} using target PID event \
                         (this app exposes no AX scroll action at these coordinates)"
                    ))
                }
            }
        })
    }

    /// 从 `from` 拖到 `to`（截图坐标空间）。
    ///
    /// # 为什么不走元素寻址
    ///
    /// 拖拽天然是**两个点**之间的关系，而 `element_ref` 只指向一个元素。
    /// 即使两端都用 ref 表达，AX 也没有跨元素的拖拽动作（`AXPress` 按下之后
    /// 无法把「按住状态」转移到另一个元素上），所以最终还是落到坐标事件。
    /// 因此这里只接受坐标，但**接受两端都带 ref 的可选参数**用于校验目标
    /// 是否还在原位——那个校验由调用方在 `list_elements` 之后自行完成。
    ///
    /// # 安全边界比点击更严
    ///
    /// 拖拽能移动文件、改滑块、重排列表，破坏性远大于点错一次。因此：
    /// 起点必须先过遮挡检测（起点落在别的窗口上时拖走的是别人的东西）、
    /// 全程持有 `InputLease`、每一步校验前台 PID、鼠标键在**任何**退出路径
    /// 上释放（`cursor_click::ButtonRelease`）。
    fn drag(&self, input: &Value) -> ToolResult {
        let target = match self.resolve_observation(input) {
            Ok(target) => target,
            Err(error) => return error_result(error),
        };
        let from = match required_coordinate(input, "from") {
            Ok(coordinate) => coordinate,
            Err(error) => return error_result(error),
        };
        let to = match required_coordinate(input, "to") {
            Ok(coordinate) => coordinate,
            Err(error) => return error_result(error),
        };
        let (from_x, from_y) = match target.screenshot_to_global(from.0, from.1) {
            Ok(coordinate) => coordinate,
            Err(error) => return error_result(error),
        };
        let (to_x, to_y) = match target.screenshot_to_global(to.0, to.1) {
            Ok(coordinate) => coordinate,
            Err(error) => return error_result(error),
        };
        let kind = click_kind_from(input);

        let (lease, frontmost_before) = match self.begin_mutation(&target, "drag") {
            Ok(guard) => guard,
            Err(error) => return error_result(error),
        };
        // 起点遮挡检测与点击同源：HID 事件按 z-order 命中，窗口被盖住时
        // 拖走的是盖住它的那个窗口里的东西。
        if let Some(occluder) = wyj_computer::occlusion::occlusion_of(target.window_id)
            .ok()
            .flatten()
        {
            return error_result(anyhow!(
                "requires_foreground_takeover: the target window is covered by another window \
                 (id {}, pid {}) so a drag would grab content from that window instead. \
                 Bring the target window to the front first",
                occluder.window_id,
                occluder.pid
            ));
        }
        let action_result =
            wyj_computer::cursor_click::execute_drag((from_x, from_y), (to_x, to_y), kind);
        wyj_computer::telemetry::record_background_action();
        if let Err(error) = self.finish_mutation(&target, "drag", &lease, frontmost_before) {
            return error_result(error);
        }
        match action_result {
            Ok(content) => self.with_after_screenshot(&target, content),
            Err(error) => error_result(error),
        }
    }

    /// 弹出元素或坐标处的上下文菜单（右键菜单）。
    ///
    /// # 为什么单独做一个 action
    ///
    /// `click` 带 `click_kind: right` 也能右键，但它走的是**坐标**路径
    /// （光标瞬移 + HID 事件）。而 `AXShowMenu` 是语义动作：不移动光标、
    /// 不依赖命中测试、被遮挡也照样生效。两条路各有适用面，不能互相替代
    /// ——自绘应用（微信）没有 AX 元素，只能用坐标右键；而原生列表行用
    /// `AXShowMenu` 更准（坐标右键可能因为行内嵌套控件而弹错层级的菜单）。
    ///
    /// # 菜单弹出后是独立窗口
    ///
    /// 上下文菜单不由 `app_computer` 的 `window_id` 覆盖。模型必须重新
    /// `window_capture list` 找到那个菜单窗口，再用新 id 操作。这一点写在
    /// 工具描述里，否则模型会以为「右键了但什么都没发生」。
    fn show_menu(&self, input: &Value) -> ToolResult {
        if has_element_ref(input) {
            return self.element_mutation(
                input,
                "show_menu",
                ElementAction::ShowMenu,
                Some(|target: &WindowTarget, x: f64, y: f64| {
                    click_with_fallback(target, x, y, wyj_computer::cursor_click::ClickKind::Right)
                }),
            );
        }
        let kind = wyj_computer::cursor_click::ClickKind::Right;
        self.coordinate_mutation(input, "show_menu", |target, x, y| {
            click_with_fallback(target, x, y, kind)
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
        self.with_after_screenshot(&target, format!("sent key `{combo}` to pid {}", target.pid))
    }

    /// 校验 `element_ref` 属于最近一次 `list_elements`，并取出那次的观察记录。
    ///
    /// stale 契约（对齐 Anthropic `ref`）：`element_ref` 只在产生它的那一次
    /// `list_elements` 里有效。窗口变了（generation 不符）、模型传了旧的 /
    /// 凭空编的 `tree_id`、或该 ref 超出当时的元素数，一律报
    /// `target_changed` 并要求重新列举——**绝不按序号回到当前树里盲找**，
    /// 那会把动作打到模型从未见过的控件上。
    ///
    /// 返回的 [`ElementObservation`] 还要传给 `execute_with_element`：那里的
    /// 指纹比对才真正挡住「树变了但序号还在范围内」这一档。
    fn resolve_element(
        &self,
        target: &WindowTarget,
        input: &Value,
    ) -> Result<(ElementRef, ElementObservation)> {
        let reference = parse_element_ref(input)?;
        let tree_id = required_u64(input, "tree_id")?;
        let current = self
            .element_observations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&target.window_id)
            .cloned();
        let current = current.ok_or_else(|| {
            anyhow!(
                "target_changed: no element snapshot for window {}; call list_elements again",
                target.window_id
            )
        })?;
        // stale 契约的核心：ref 只能配合产生它的那次 tree_id 使用。
        // 对不上就要求重新列举，绝不按序号回到当前树里盲找元素。
        anyhow::ensure!(
            current.tree_id == tree_id,
            "target_changed: tree_id {tree_id} is stale (the last list_elements returned {}); call list_elements again",
            current.tree_id
        );
        Ok((reference, current))
    }

    /// 句柄寻址的变更动作统一入口。
    ///
    /// 与坐标版本共用同一套安全闸门（`begin_mutation` 拿输入租约 +
    /// `finish_mutation` 校验前台未被抢走），这是本仓库最不该被绕过的一条
    /// 不变式：换一种寻址方式不等于换一种权限语义。
    ///
    /// # 为什么带 `degrade`
    ///
    /// 元素在树里、几何也有，只是不支持对应的 AX 动作——自绘控件（微信）
    /// 和只读文本框都会这样。坐标路径对同一情形早已自动降级（AXPress 失败
    /// 就瞬移点击），元素路径却直接 `bail`，于是**换一种更精确的寻址方式
    /// 反而变成更弱的能力**。模型被迫自己把 ref 换算回坐标再发一次，等于
    /// 把一次必然成功的操作拆成两趟、还多给它一次算错的机会。
    ///
    /// 降级只在错误**确实是结构性的**时候发生（`requires_foreground_takeover`）。
    /// `target_changed` / `preempted_by_user` / 权限缺失都**不降级**：那些是
    /// 「你现在不该动手」，换个寻址方式并不能让它变安全。
    fn element_mutation<F>(
        &self,
        input: &Value,
        action_name: &str,
        action: ElementAction<'_>,
        degrade: Option<F>,
    ) -> ToolResult
    where
        F: FnOnce(&WindowTarget, f64, f64) -> Result<String>,
    {
        let target = match self.resolve_observation(input) {
            Ok(target) => target,
            Err(error) => return error_result(error),
        };
        let (reference, observation) = match self.resolve_element(&target, input) {
            Ok(resolved) => resolved,
            Err(error) => return error_result(error),
        };

        let (lease, frontmost_before) = match self.begin_mutation(&target, action_name) {
            Ok(guard) => guard,
            Err(error) => return error_result(error),
        };
        // 重新遍历并按序号定位：句柄绝不跨 crate 边界（AXUIElement 非 Send/Sync），
        // 定位与使用都在 `wyj_computer` 内部一次完成。指纹一并传进去，让
        // 「树变了但序号还在范围内」在真正执行动作之前被拒掉。
        let action_result = wyj_computer::element::execute_with_element(
            &target,
            &TraversalBudget::default(),
            reference,
            action,
            Some(observation.content_fingerprint),
            // 必须复用当次 `list_elements` 的过滤条件：ref 序号是「过滤后
            // 列表里的第 N 个」，用无过滤遍历会点到另一个元素。
            Some(&observation.query),
        );
        wyj_computer::telemetry::record_background_action();
        if let Err(error) = self.finish_mutation(&target, action_name, &lease, frontmost_before) {
            return error_result(error);
        }
        match action_result {
            Ok(content) => self.with_after_screenshot(&target, content),
            Err(error) => {
                let message = error.to_string();
                let structural = message.contains("requires_foreground_takeover");
                let (Some(degrade), true) = (degrade, structural) else {
                    return error_result(error);
                };
                let Some((x, y)) = observation.center_of(reference) else {
                    // 元素没有截图坐标（AX 不给几何）。降级无从谈起，把原始
                    // 错误交回模型——它至少知道这个控件结构上点不动。
                    return error_result(error);
                };
                let (global_x, global_y) = match target.screenshot_to_global(x, y) {
                    Ok(coordinate) => coordinate,
                    Err(error) => return error_result(error),
                };
                match degrade(&target, global_x, global_y) {
                    Ok(content) => self.with_after_screenshot(
                        &target,
                        format!(
                            "{content}\n\nThe element itself has no usable Accessibility action \
                             ({message}); the action was performed at its centre ({x}, {y}) in the \
                             screenshot coordinate space instead."
                        ),
                    ),
                    Err(fallback_error) => error_result(anyhow!(
                        "{fallback_error}\n\nThe element-level action also failed: {message}"
                    )),
                }
            }
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
        let (x, y) = match required_coordinate(input, "coordinate") {
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
            Ok(content) => self.with_after_screenshot(&target, content),
            Err(error) => error_result(error),
        }
    }

    /// 变更动作成功后的统一收尾：按需附一张「动作之后」的窗口截图。
    ///
    /// 三条不变式：
    ///
    /// 1. **只在成功路径调用**。失败回执里塞一张截图会把错误信息挤到次要
    ///    位置，而错误恰恰是模型最需要读的东西。
    /// 2. **附图失败绝不改变工具结果**。截图可能因为窗口关闭、屏幕锁定、
    ///    权限被撤而失败——那时回执仍然必须是「动作成功」这个事实，只是
    ///    没有附图（与 `tools::spill` 落盘失败的处理同一条原则）。
    /// 3. **连续动作上限是兜底而非补充**。`auto_screenshot` 关掉时也要在
    ///    `MAX_ACTIONS_WITHOUT_SCREENSHOT` 处强制附一张，否则关掉开关就等于
    ///    关掉了整条验证链。
    fn with_after_screenshot(&self, target: &WindowTarget, content: String) -> ToolResult {
        let count = {
            let mut counts = self
                .mutations_since_screenshot
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let slot = counts.entry(target.window_id).or_insert(0);
            *slot += 1;
            *slot
        };
        let forced = count >= MAX_ACTIONS_WITHOUT_SCREENSHOT;
        // 没有图片通道时，附图只是一段模型看不见的 base64。但「连续盲点」这条
        // 保护必须保留语义：到阈值就让模型**主动去读一次界面状态**（AX 文本
        // 或 OCR），而不是继续凭动作回执往下点。
        if !self.image_channel {
            if !forced {
                return ToolResult::ok(content);
            }
            self.mutations_since_screenshot
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(target.window_id, 0);
            return ToolResult::ok(format!(
                "{content}\n\nThis is mutation #{MAX_ACTIONS_WITHOUT_SCREENSHOT} since the last \
                 observation. This profile cannot receive screenshots, so verify the window \
                 state with `list_elements` (with `include_text:true`) or `read_text` before \
                 continuing."
            ));
        }
        // 强制档用观察用的 `max_dim`：既然是被迫补看一次，就给足分辨率。
        let max_dim = match (self.auto_screenshot, forced) {
            (_, true) => Some(self.max_dim),
            (Some(configured), false) => Some(configured),
            (None, false) => None,
        };
        let Some(max_dim) = max_dim else {
            return ToolResult::ok(content);
        };

        let mut display = content;
        match wyj_computer::target::capture_window_by_id(target.window_id, max_dim) {
            Ok((_, capture)) => {
                self.mutations_since_screenshot
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(target.window_id, 0);
                if forced {
                    display.push_str(&format!(
                        "\n\nThis is mutation #{MAX_ACTIONS_WITHOUT_SCREENSHOT} since the last \
                         screenshot; the attached capture is what the window looks like now. \
                         Verify the task state before continuing."
                    ));
                }
                ToolResult::with_parts(
                    display,
                    vec![ToolResultPart::Image {
                        media_type: "image/png".to_string(),
                        data: capture.png_base64(),
                    }],
                )
            }
            Err(error) => {
                // 截图失败不改变结果语义，只在文本里说明一句。
                display.push_str(&format!(
                    "\n\n[the action succeeded but the follow-up screenshot failed: {error}]"
                ));
                ToolResult::ok(display)
            }
        }
    }
}

/// 在全局坐标处点击：先试 AXPress，失败才降级到光标瞬移。
///
/// # 为什么是自动降级而不是让模型二选一
///
/// 判断依据是「这个 App 的 AX 树在这一点上有没有元素」，那是**运行时事实**，
/// 模型看不见也猜不准。让模型先试 AXPress、失败了再自己改用另一种 action，
/// 等于把一次必然失败的往返写进每个 GUI 任务的必经之路。
///
/// 实测（微信 v4.1.15，原生 AppKit 自绘）：窗口内 5×5 网格 hit-test 25/25
/// 全部返回 `kAXErrorNotImplemented`，AX 路线对该应用**必然**失败；而它恰恰
/// 是最常见的中文桌面应用之一。
///
/// 坐标点击与元素寻址共用本函数，这样「元素路径降级」与「坐标路径降级」
/// 走的是同一段逻辑，不会各自演化出不同的安全边界。
fn click_with_fallback(
    target: &WindowTarget,
    x: f64,
    y: f64,
    kind: wyj_computer::cursor_click::ClickKind,
) -> Result<String> {
    match wyj_computer::accessibility::press(target.pid, x, y) {
        Ok(()) => Ok("clicked with AXPress".to_string()),
        Err(ax_error) => {
            // 降级到光标瞬移前必须确认目标窗口没被遮挡。
            //
            // 两条路径的「命中」机制完全不同：截图是窗口级快照（被盖住也照样
            // 能截到自己的内容），而光标瞬移发出的是 HID 事件，由窗口服务器
            // 按 z-order 分发。窗口被盖住时，模型看到的是目标窗口、点中的是
            // 上面那个窗口——最典型的触发是「另存为」sheet。这种错配比直接
            // 报错糟糕得多：模型会以为点了没反应，然后反复重试同一个坐标。
            //
            // 注意只在**降级点**检查，不能提到 `begin_mutation` 里当通用前置：
            // AXPress 按元素寻址、不经窗口服务器命中测试，被遮挡也照样生效。
            if let Some(occluder) = wyj_computer::occlusion::occlusion_of(target.window_id)? {
                return Err(anyhow!(
                    "requires_foreground_takeover: the target window is covered by another \
                     window (id {}, pid {}) so a coordinate click would hit that window instead; \
                     AXPress failed ({ax_error}). Bring the target window to the front or act on \
                     the covering window instead",
                    occluder.window_id,
                    occluder.pid
                ));
            }
            wyj_computer::cursor_click::execute(x, y, kind).map(|detail| {
                format!(
                    "clicked via cursor warp: {detail}. \
                     AXPress was tried first and failed ({ax_error}) because this app \
                     exposes no accessibility element at these coordinates."
                )
            })
        }
    }
}

/// 在全局坐标处写入文本：AXValue → 剪贴板粘贴 → 定向 Unicode → 逐字符按键。
///
/// # 四级降级的顺序是有理由的
///
/// 1. **AXValue / AXSelectedText**：最干净——不碰剪贴板、不碰键盘、不依赖
///    焦点。可写的原生控件（`AXTextField`、`AXTextArea`）走这条。
/// 2. **剪贴板 + `cmd+V`**：覆盖**所有**文字（中文、日文、emoji），代价是
///    短暂借用用户的剪贴板。自绘应用（微信）和任何不暴露可写 AX 属性的
///    控件都靠这条。
/// 3. **定向 PID Unicode**（[`wyj_computer::targeted_event::type_text`]）：
///    事件里携带文本本身，非 ASCII 也能走，**不动用户剪贴板**。排在剪贴板
///    之后是因为它无法自证（`CGEventPostToPid` 返回 void，App 静默丢弃时
///    调用方仍拿到成功），而剪贴板路径的覆盖面已经被实测验证过。
/// 4. **逐字符定向按键**：只在**全 ASCII** 时可用，作为最后兜底。
///
/// 顺序不能反：把逐字符放前面会让每个中文输入都先失败一次再降级，白白多付
/// 一趟；把剪贴板放第一则会为本来能 AX 写入的普通控件无谓地动用户的剪贴板。
///
/// # 为什么第 3 档不是「非 ASCII 专属」
///
/// 理论上它值得排到第 2 档（不动剪贴板是实打实的好处），但那会把**每次**
/// 文本写入都推进一条无法自证的路径：第 2 档的剪贴板粘贴至少留下「应用主动
/// 读了剪贴板」这条语义证据，第 3 档什么都没有。取舍是「少动一次剪贴板」
/// ↔ 「多一次静默失败的可能」，在这里后者更贵。
fn type_text_with_fallback(target: &WindowTarget, x: f64, y: f64, text: &str) -> Result<String> {
    // 第 1 档：AX 写入。
    match wyj_computer::accessibility::set_text(target.pid, x, y, text) {
        Ok(attribute) => Ok(format!("set text using {attribute}")),
        // 第 2 档：剪贴板粘贴。AX 失败是常态（自绘应用在目标点不做
        // hit-test），所以这一档才是主路径，不是兜底。
        Err(ax_error) => match paste_via_clipboard(target.pid, text) {
            Ok(detail) => Ok(format!(
                "{detail} The accessibility write failed ({ax_error}) because this app \
                 exposes no settable accessibility element there; the click that focused \
                 the field must have succeeded first."
            )),
            // 第 3 档：定向 Unicode。不动剪贴板，非 ASCII 也能走。
            Err(clipboard_error) => {
                match wyj_computer::targeted_event::type_text(target.pid, text) {
                    Ok(()) => Ok(format!(
                        "typed {} characters via target PID unicode events at ({x}, {y}). \
                     The accessibility write failed ({ax_error}) and clipboard paste failed \
                     ({clipboard_error}); the click that focused the field must have \
                     succeeded first. Verify with a screenshot — this channel cannot \
                     confirm that the target app consumed the events.",
                        text.chars().count()
                    )),
                    // 第 4 档：逐字符。只在全 ASCII 时可能成功；非 ASCII 会在这里
                    // 拿到一个明确的错误，而不是静默丢字。
                    Err(unicode_error) => {
                        for character in text.chars() {
                            wyj_computer::targeted_event::type_char(target.pid, character)
                                .map_err(|typing_error| {
                                    anyhow!(
                                        "{typing_error}\n\nAll earlier attempts failed: \
                                     accessibility write ({ax_error}), clipboard paste \
                                     ({clipboard_error}), target PID unicode \
                                     ({unicode_error})."
                                    )
                                })?;
                        }
                        Ok(format!(
                            "typed {} characters via target PID keystrokes at ({x}, {y}). \
                         The accessibility write failed ({ax_error}) and clipboard paste failed \
                         ({clipboard_error}); the click that focused the field must have \
                         succeeded first.",
                            text.chars().count()
                        ))
                    }
                }
            }
        },
    }
}

/// 把 `text` 写进剪贴板并向目标 PID 发 `cmd+V`，随后无条件恢复原剪贴板。
///
/// 恢复失败**不影响返回值**：粘贴已经发生，回执必须是「成功」；剪贴板没还原
/// 是另一个问题，只在文本里说明一句（与 `tools::spill` 落盘失败同一条原则）。
fn paste_via_clipboard(pid: u32, text: &str) -> Result<String> {
    let guard = wyj_computer::clipboard::ClipboardGuard::write(text)
        .map_err(|error| anyhow!("could not prepare the system clipboard: {error}"))?;
    let restorable = guard.is_restorable();
    wyj_computer::targeted_event::paste(pid)
        .map_err(|error| anyhow!("could not send cmd+V to the target process: {error}"))?;
    let restore_error = guard.restore_now().err();
    Ok(match (restorable, restore_error) {
        (true, None) => format!(
            "pasted {} characters from the clipboard",
            text.chars().count()
        ),
        (true, Some(error)) => format!(
            "pasted {} characters from the clipboard (the previous clipboard content could not \
             be restored: {error})",
            text.chars().count()
        ),
        // 原本剪贴板里没有文本（图片/文件）：我们无从还原它的原始类型，
        // 必须让模型知道用户丢了什么东西。
        (false, _) => format!(
            "pasted {} characters from the clipboard. The clipboard previously held no text \
             (it may have contained an image or files, which could not be restored)",
            text.chars().count()
        ),
    })
}

/// 语义滚动：先试 AX increment/decrement，失败才降级到定向 PID 滚轮。
///
/// 之前写成 `accessibility::scroll(...)?`：它在内部先做 `element_at(pid,x,y)`
/// 命中测试，微信这类自绘 AppKit 应用返回 `kAXErrorNotImplemented(-25208)`，
/// `?` 直接把整个调用短路掉，后面的定向 PID 滚轮永远到不了——于是「不依赖
/// AX 的滚动能力其实存在但不可达」。
///
/// 降级必须覆盖「hit-test 本身就失败」这一档：它不是「元素不支持该 AX
/// action」（那是 `Ok(false)`），而是压根没找到元素。
fn scroll_with_fallback(target: &WindowTarget, direction: &str, amount: u32) -> Result<String> {
    wyj_computer::targeted_event::scroll(target.pid, direction, amount)?;
    Ok(format!(
        "scrolled {direction} x{amount} using target PID event \
         (the element itself exposes no AX scroll action)"
    ))
}

const DESCRIPTION: &str = "Controls a specific macOS application window without moving the user's pointer or taking away their keyboard focus. Most apps expose an accessibility tree, and there `element_ref` is the most reliable way to act: `list_windows` or `screenshot` to capture a window, `list_elements` to read its tree and get stable `element_ref` ids (one compact aligned line per element by default; pass `format: json` only when you need every field verbatim), then pass `window_id`, `generation`, the `tree_id` from that `list_elements` call, and `element_ref` (e.g. `e12`). Use a `coordinate` from the attached screenshot instead when `list_elements` reports `root_available:false` (common for self-drawn apps such as WeChat), the element you need is missing, or `truncated:true` hid it. When you need to *read* text rather than click it, prefer `list_elements` with `include_text:true` — the text comes from the accessibility tree, so it is exact. Only when the app exposes no text at all (self-drawn canvases, images, video captions) use `read_text`, which runs on-device OCR over the window pixels and returns every line with a per-line confidence and pixel coordinates; treat those results as a machine reading, not as ground truth, and re-check numbers before acting on them. Coordinate actions fall back automatically when accessibility is unavailable: `click` uses AXPress first and switches to a cursor warp (jumps to the point, clicks, returns the cursor to where it was) if the app exposes no accessibility element there; `set_text` writes through Accessibility when the field is settable and otherwise falls back to a clipboard paste (which handles any script, including Chinese and emoji, at the cost of briefly borrowing the user's clipboard — the previous content is restored afterwards); ASCII-only text may also fall back to per-character target-PID keystrokes. `scroll` degrades the same way. `drag` moves from one screenshot point to another in interpolated steps (needed because apps detect a drag from the intermediate movement, not from press-and-release alone) and is the only way to move files, drag sliders, or reorder lists; like `click`, it refuses to run when the target window is covered. A stale `tree_id` or an out-of-range `element_ref` returns `target_changed` — call `list_elements` again rather than retrying the old reference. To run an application command (File then New Tab, View then Show Path Bar, Window then Minimize), prefer the menu bar: `list_menus` returns every menu item with a `menu_ref` (e.g. `m12`) and a readable `path`, and `press_menu` activates it in the background without touching the pointer; it covers the app's menu bar only, not right-click context menus — for those use `show_menu`, which opens the context menu of an element (via AXShowMenu, no pointer movement) or of a coordinate (as a right click); that menu is a separate window, so list windows again to find it. `key` and unsupported semantic scrolling are posted only to the target PID. Every mutation that succeeds returns a fresh screenshot of the window as it looks afterwards, so read it instead of assuming the action had the effect you intended; when a mutation starts an animation or a load, call `wait` (up to 3000ms) first and then `screenshot`. If the action was supposed to open a dialog or a new window, run `window_capture` action `list` afterwards: a sheet or dialog is usually its own window and will not appear in the previous window's capture. On `target_changed`, capture again — if the window no longer exists, list windows again instead of retrying the old id. On `requires_foreground_takeover`, `preempted_by_user`, `user_active`, or permission errors, stop and report instead of retrying. A few operations cannot be expressed through the accessibility tree at all — naming a brand-new Finder folder, creating a note whose body is rich text, opening a URL without driving the address bar. For those, `run_recipe` runs one of a small fixed set of automation scripts shipped with the binary: you pick the recipe name and pass its positional `args`, and the arguments travel as data rather than being spliced into the script, so quoting in them is harmless. The first time a recipe drives a given app macOS shows an Automation permission dialog that a person must accept, so recipes only run on interactive surfaces; if that permission is refused you get `appleevents_permission_required` and must not retry.";

/// 附在没有图片回传通道的 profile 上的补充说明。
///
/// 这段**不能省**：工具描述里大量承诺「附带的截图」「用 coordinate 猜坐标」，
/// 而 `vision=false` 的端点上图片根本发不出去。不声明这一点，模型会先按描述里
/// 的截图工作流规划、再拿到一张永远看不见的图，然后开始编造画面内容。
///
/// 注意这里说的是**这个 profile 收不到图**，而不是「这条线协议不行」——OpenAI
/// 线的 tool_result 图片现在由 `openai.rs::to_api_messages` 后置成 user 消息送达，
/// 只有在 `profile.vision = false` 时才会走到本段。
const NO_IMAGE_CHANNEL_NOTE: &str = "IMPORTANT for this profile: images cannot be delivered to you on this profile, so the `screenshot` action is not available and `read_text` returns text only — no picture ever reaches you. Do NOT plan around seeing a screenshot, and never describe or guess what the window looks like. Read state exclusively through `list_elements` (pass `include_text:true` for on-screen text) and `read_text` (OCR text). Prefer `element_ref` over `coordinate`: with no image to measure against, a guessed coordinate is just a guess. After each mutation, re-run `list_elements` to confirm the change took effect.";

#[async_trait]
impl Tool for AppComputerTool {
    fn name(&self) -> &str {
        "app_computer"
    }

    fn definition(&self) -> ToolDefinition {
        // 没有图片通道时，描述里所有「读附带的截图」「用 coordinate」都成了
        // 模型做不到的事。与其让它反复尝试再困惑，不如把这些能力从描述里
        // 撤掉，只剩 Accessibility 与 OCR 两条文本路径。
        let description = if self.image_channel {
            DESCRIPTION.to_string()
        } else {
            format!("{DESCRIPTION} {NO_IMAGE_CHANNEL_NOTE}")
        };
        // `screenshot` 在无图片通道时**从枚举里摘掉**，而不是留着返回一段文本。
        // 留着的代价是双向的：模型看到一个叫 screenshot 的动作就会去调（它的
        // 训练先验认为这动作能拿到画面），而每次调用都要过一遍权限确认、
        // 真截一张图、编码 base64 再丢掉。摘掉之后模型从动作面上就知道这条路
        // 不存在，只会走 `list_elements` / `read_text`。
        //
        // 元信息没有损失：窗口尺寸由 `list_windows` 直接给出，`list_elements`
        // 回执里也带 `capture` 块。
        let actions: Vec<&str> = if self.image_channel {
            vec![
                "list_windows",
                "screenshot",
                "list_elements",
                "list_menus",
                "inspect_element",
                "read_text",
                "wait",
                "click",
                "drag",
                "show_menu",
                "set_text",
                "press_menu",
                "key",
                "scroll",
                "run_recipe",
            ]
        } else {
            vec![
                "list_windows",
                "list_elements",
                "list_menus",
                "inspect_element",
                "read_text",
                "wait",
                "click",
                "drag",
                "show_menu",
                "set_text",
                "press_menu",
                "key",
                "scroll",
                "run_recipe",
            ]
        };
        let wait_hint = if self.image_channel {
            "Use it after an action that starts an animation or a load, then screenshot to see the settled state."
        } else {
            "Use it after an action that starts an animation or a load, then re-run `list_elements` to see the settled state."
        };
        ToolDefinition {
            name: self.name().to_string(),
            description,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": actions
                    },
                    "window_id": {"type": "integer", "minimum": 0},
                    "generation": {"type": "integer", "minimum": 0},
                    "target_width": {"type": "integer", "minimum": 1},
                    "target_height": {"type": "integer", "minimum": 1},
                    "tree_id": {"type": "integer", "minimum": 0},
                    "element_ref": {"type": "string", "pattern": "^e[0-9]+$"},
                    "click_kind": {
                        "type": "string",
                        "enum": ["left", "right", "middle", "double_left"],
                        "description": "Mouse button for a coordinate click. Defaults to left."
                    },
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
                    "from": {
                        "type": "array",
                        "items": {"type": "integer"},
                        "minItems": 2,
                        "maxItems": 2,
                        "description": "Drag start point in screenshot coordinates. Requires `to`."
                    },
                    "to": {
                        "type": "array",
                        "items": {"type": "integer"},
                        "minItems": 2,
                        "maxItems": 2,
                        "description": "Drag end point in screenshot coordinates. Requires `from`."
                    },
                    "text": {"type": "string"},
                    "key": {"type": "string"},
                    "scroll_direction": {
                        "type": "string",
                        "enum": ["up", "down", "left", "right"]
                    },
                    "scroll_amount": {"type": "integer", "minimum": 1, "maximum": 20},
                    "format": {
                        "type": "string",
                        "enum": ["compact", "json"],
                        "description": "Output format for `list_elements`. Defaults to `compact` (one aligned line per element, ~10x fewer tokens). Use `json` only when you need every field verbatim for debugging."
                    },
                    "region": {
                        "type": "array",
                        "items": {"type": "number"},
                        "minItems": 4,
                        "maxItems": 4,
                        "description": "For `list_elements`: keep only elements intersecting this rectangle [x0, y0, x1, y1] in screenshot coordinates. Use it when the full list is truncated or too long."
                    },
                    "role_filter": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "For `list_elements`: keep only these roles (case-insensitive, `AX` prefix optional, substring match — e.g. \"button\" matches AXButton and AXToolbarButton)."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "For `list_elements`: return at most this many elements. When the list is cut, the result reports has_more=true."
                    },
                    "include_text": {
                        "type": "boolean",
                        "description": "For `list_elements`: also list static text nodes (AXStaticText, AXHeading, AXListMarker) with their exact positions, up to 40 per window. This is the reliable way to read on-screen text — far more accurate than reading it off a downsampled screenshot. Text nodes have their own quota, so they never crowd out interactable controls. Defaults to false."
                    },
                    "min_confidence": {
                        "type": "number",
                        "minimum": 0,
                        "maximum": 1,
                        "description": "For `read_text`: drop recognized lines below this confidence (default 0). Recognition is statistical — raise it (e.g. 0.4) to suppress misread noise when you need reliable values, lower it to keep faint or small text."
                    },
                    "recipe": {
                        "type": "string",
                        "enum": ["finder_new_folder", "notes_new_note", "chrome_open_url"],
                        "description": "For `run_recipe`: which fixed automation to run. These are compile-time scripts shipped with the binary — you choose the recipe and its arguments, never the script text. Prefer normal `app_computer` actions for anything they can already do; use a recipe only for the specific cases where the accessibility tree cannot express the operation."
                    },
                    "args": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "For `run_recipe`: positional arguments. finder_new_folder takes [parent_posix_path, folder_name]; notes_new_note takes [title, body]; chrome_open_url takes [url]. Arguments are passed as data, not spliced into the script, so quotes and semicolons in them are harmless."
                    },
                    "ms": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 3000,
                        "description": wait_hint
                    }
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
                "list_windows"
                    | "screenshot"
                    | "list_elements"
                    | "list_menus"
                    | "inspect_element"
                    | "read_text"
                    | "wait"
            )
        )
    }

    fn action_summary(&self, input: &Value) -> String {
        let action = input
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let mut parts = vec![action.to_string()];
        // 窗口标题与坐标必须带上：auto 分类器只能看到 `action_summary`，
        // 「在编号 76 的窗口上后台点一下」这种摘要既没有目标 App 也没有落点，
        // 模型只能按「不确定就 deny」处理（`prompts::AUTO_CLASSIFIER`）。
        // 让它看见「微信 / click at (640, 320)」才判得准。
        if let Some(id) = input.get("window_id").and_then(Value::as_u64) {
            let title = self
                .observations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(&u32::try_from(id).unwrap_or(u32::MAX))
                .map(|target| {
                    format!(
                        "{}/{} \"{}\"",
                        target.app_name, target.window_id, target.title
                    )
                });
            parts.push(title.unwrap_or_else(|| format!("window {id}")));
        }
        if let Some(kind) = input.get("click_kind").and_then(Value::as_str) {
            parts.push(format!("{kind} button"));
        }
        if let Some(key) = input.get("key").and_then(Value::as_str) {
            parts.push(format!("key {key}"));
        }
        if let Some(direction) = input.get("scroll_direction").and_then(Value::as_str) {
            parts.push(format!("scroll {direction}"));
        }
        if let Some(text) = input.get("text").and_then(Value::as_str) {
            parts.push(format!(
                "type {:?}",
                crate::textutil::truncate_str(text, 120)
            ));
        }
        if let Some(pair) = input.get("coordinate").and_then(Value::as_array) {
            if let [Some(x), Some(y)] = [
                pair.first().and_then(Value::as_i64),
                pair.get(1).and_then(Value::as_i64),
            ] {
                parts.push(format!("at ({x}, {y})"));
            }
        }
        // 拖拽的两端都要进摘要：auto 分类器只看这段文本，「drag」一个词
        // 完全看不出是把什么东西从哪儿拖到哪儿。
        if let Some(path) = describe_drag_path(input) {
            parts.push(path);
        }
        // recipe 也一样：`run_recipe` 单独一个词完全看不出要跑什么、带什么
        // 参数，而这段文本是 auto 分类器唯一的输入。
        if let Some(recipe) = input.get("recipe").and_then(Value::as_str) {
            parts.push(format!("recipe={recipe}"));
        }
        if let Some(args) = input.get("args").and_then(Value::as_array) {
            let rendered: Vec<String> = args
                .iter()
                .filter_map(Value::as_str)
                .map(|arg| format!("{arg:?}"))
                .collect();
            if !rendered.is_empty() {
                parts.push(format!("args=[{}]", rendered.join(", ")));
            }
        }
        format!(
            "{} (background, does not move the pointer permanently)",
            parts.join(" ")
        )
    }

    async fn run(&self, input: Value, ctx: &dyn ToolContext) -> Result<ToolResult> {
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
            // schema 里已摘掉这个动作，但模型可能凭旧上下文重放或手工拼一个
            // 调用。这里是最后一道：直接拒绝并说清替代路径，而不是悄悄截一张
            // 图、编码成模型看不见的 base64 再返回。
            "screenshot" if !self.image_channel => ToolResult::err(
                "unsupported app_computer action `screenshot`: this profile has no image \
                 channel inside tool results, so a capture would never reach you. Use \
                 `list_elements` (with `include_text:true`) or `read_text` to read the \
                 window instead.\n\
                 本端点无法回传图片，请改用 `list_elements` 或 `read_text` 读取窗口。",
            ),
            "screenshot" => self.screenshot(&input),
            "inspect_element" => self.inspect(&input),
            "list_elements" => self.list_elements(&input),
            "list_menus" => self.list_menus(&input),
            "read_text" => self.read_text(&input),
            "run_recipe" => self.run_recipe(&input, ctx),
            "wait" => {
                let ms = input
                    .get("ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(500)
                    .clamp(1, MAX_WAIT_MS);
                tokio::time::sleep(Duration::from_millis(ms)).await;
                let next = if self.image_channel {
                    "Take a screenshot now to see the settled state; \
                     do not assume the previous screenshot is still current."
                } else {
                    "Re-run `list_elements` now to see the settled state; \
                     do not assume the previous reading is still current."
                };
                ToolResult::ok(format!("waited {ms}ms. {next}"))
            }
            "press_menu" => self.press_menu(&input),
            "click" => self.click(&input),
            "drag" => self.drag(&input),
            "show_menu" => self.show_menu(&input),
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

/// 坐标点击的按键类型。模型通过 `click_kind` 指定，缺省左键。
///
/// 为什么让它走 schema 而不是复用 `action`：坐标点击与 `element_ref` 点击
/// 共用同一个 `click` action，而前者需要额外的「点哪个键」信息。
fn click_kind_from(input: &Value) -> wyj_computer::cursor_click::ClickKind {
    input
        .get("click_kind")
        .and_then(Value::as_str)
        .and_then(|raw| wyj_computer::cursor_click::parse_click_kind(raw).ok())
        .unwrap_or(wyj_computer::cursor_click::ClickKind::Left)
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
    anyhow::ensure!(
        !raw.is_empty(),
        "`menu_path` must contain at least one reference"
    );
    raw.iter()
        .map(|item| {
            item.as_str()
                .ok_or_else(|| anyhow!("invalid `menu_path` entry: expected a string like `m2`"))
                .and_then(parse_menu_ref_str)
        })
        .collect()
}

/// 解析一个 `[x, y]` 坐标字段。
///
/// `field` 是字段名：`click`/`scroll` 用 `coordinate`，`drag` 用 `from`/`to`。
/// 错误消息必须带上真实字段名——模型收到「missing `coordinate`」而它明明
/// 传了 `from`，只会让它重新猜一遍参数而不是改正。
fn required_coordinate(input: &Value, field: &str) -> Result<(i32, i32)> {
    let coordinate = input
        .get(field)
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing required field `{field}`"))?;
    anyhow::ensure!(
        coordinate.len() == 2,
        "`{field}` must contain exactly 2 integers"
    );
    let x = coordinate[0]
        .as_i64()
        .and_then(|value| i32::try_from(value).ok())
        .ok_or_else(|| anyhow!("invalid `{field}` x"))?;
    let y = coordinate[1]
        .as_i64()
        .and_then(|value| i32::try_from(value).ok())
        .ok_or_else(|| anyhow!("invalid `{field}` y"))?;
    Ok((x, y))
}

/// `list_elements` 是否要求原始 JSON（调试用）。
///
/// 默认走紧凑行文本：pretty JSON 对 100 个元素是 ~80KB，而同样的信息用
/// 对齐行只要 ~6KB。模型读两种格式的准确率没有差别，但 token 差 10 倍，
/// 而 `list_elements` 是 GUI 任务的**每一步**都会调的工具。
fn wants_json(input: &Value) -> bool {
    input
        .get("format")
        .and_then(Value::as_str)
        .is_some_and(|format| format.eq_ignore_ascii_case("json"))
}

/// 把一次元素快照渲染成给模型看的文本。
///
/// 两种格式：
///
/// - **紧凑行**（默认）：表头一行给出 `tree_id` / 尺寸 / 统计，随后每个元素
///   一行，字段定宽对齐，空字段直接省略。模型要的是「哪个 ref 是什么控件、
///   在哪儿、能不能点」，多余的 `"value": null` 只是在烧 token。
/// - **pretty JSON**（`"format":"json"`）：完整字段，供排障时肉眼比对。
///
/// 提取成自由函数是为了能脱离真机单测——这个函数决定了模型**看得见什么**，
/// 漏掉 `tree_id` 会让所有后续动作失败，漏掉 `frame_in_screenshot` 会让
/// 坐标降级无从下手。
fn render_element_list(
    snapshot: &wyj_computer::element::ElementSnapshot,
    capture: &wyj_computer::Capture,
    as_json: bool,
) -> String {
    if as_json {
        return serde_json::to_string_pretty(&serde_json::json!({
            "tree_id": snapshot.tree_id,
            "content_fingerprint": snapshot.content_fingerprint,
            "root_available": snapshot.root_available,
            "count": snapshot.elements.len(),
            "truncated": snapshot.truncated,
            "has_more": snapshot.has_more,
            "filtered_out": snapshot.filtered_out,
            "visited": snapshot.visited,
            "elapsed_ms": snapshot.elapsed_ms,
            "capture": {
                "target_width": capture.target_width,
                "target_height": capture.target_height,
            },
            "elements": snapshot.elements,
        }))
        .unwrap_or_else(|_| format!("[list_elements window_id={}]", snapshot.window_id));
    }

    let mut out = format!(
        "tree_id={} window={} capture={}x{} elements={} visited={} filtered_out={} elapsed={}ms\n",
        snapshot.tree_id,
        snapshot.window_id,
        capture.target_width,
        capture.target_height,
        snapshot.elements.len(),
        snapshot.visited,
        snapshot.filtered_out,
        snapshot.elapsed_ms,
    );
    if snapshot.truncated {
        out.push_str("truncated=true (the traversal budget was hit; some elements are missing)\n");
    }
    if snapshot.has_more {
        out.push_str(
            "has_more=true (the `limit` cut the list short; raise it or narrow with `region`)\n",
        );
    }
    out.push_str("ref  role                @centre      size      text / state\n");
    for element in &snapshot.elements {
        out.push_str(&format_element_line(element));
        out.push('\n');
    }
    out
}

/// 单个元素的紧凑行。
///
/// 列：`ref` / `role`（去掉 `AX` 前缀）/ 截图坐标中心 / 尺寸 / 文本与状态。
///
/// 中心点而不是左上角：模型要的是「点哪儿」，不是「矩形从哪儿开始」。
/// 尺寸保留是因为「按钮只有 4x4」与「按钮 200x40」对模型是两种不同的东西。
fn format_element_line(element: &wyj_computer::element::ElementInfo) -> String {
    // 去掉 `AX` 前缀：`AXButton` → `Button`。前缀是 macOS 的实现细节，
    // 每个元素都重复它纯属浪费列宽。
    let raw_role = element.role.as_deref().unwrap_or("-");
    let role = raw_role.strip_prefix("AX").unwrap_or(raw_role);
    let (centre, size) = match element.frame_in_screenshot {
        Some(frame) => {
            let (x, y) = frame.center();
            (
                format!("({:.0},{:.0})", x, y),
                format!("{:.0}x{:.0}", frame.width, frame.height),
            )
        }
        // 没有截图坐标的元素仍然要列出来：它能被 element_ref 操作，只是
        // 不能降级到坐标。
        None => ("-".to_string(), "-".to_string()),
    };

    let mut text = String::new();
    // 优先 title，退回 description，再退回 value——同一个控件在不同 App 里
    // 只有一个字段有内容，三个都打会重复。
    let label = element
        .title
        .as_deref()
        .or(element.description.as_deref())
        .or(element.value.as_deref());
    if let Some(label) = label {
        // 纯文本节点放宽到 120（= `element::MAX_FIELD_CHARS`）：控件标签是
        // 几个词，而文本节点的价值就在内容本身——截到 60 会把一段话腰斩成
        // 「无法据此判断」的半句。120 仍是源码侧已经截断过的长度，不会失控。
        let limit = if wyj_computer::element::is_text_role(element.role.as_deref()) {
            wyj_computer::element::MAX_FIELD_CHARS
        } else {
            60
        };
        text.push_str(&format!(
            "{:?}",
            crate::textutil::truncate_str(label, limit)
        ));
    }
    let mut states: Vec<&str> = Vec::new();
    if element.focused == Some(true) {
        states.push("focused");
    }
    if element.selected == Some(true) {
        states.push("selected");
    }
    if element.enabled == Some(false) {
        states.push("disabled");
    }
    if element.value_settable || element.selected_text_settable {
        states.push("settable");
    }
    if !element.actions.iter().any(|action| action == "AXPress")
        && !wyj_computer::element::is_text_role(element.role.as_deref())
    {
        // 明确标出「不能按」：模型据此决定要不要退回坐标。
        //
        // 纯文本节点不标：它们**本来**就不能按，每一行都挂一个 `no-press`
        // 只是噪音。role 名（`StaticText` / `Heading` / `ListMarker`）已经
        // 足够说明它只能读。
        states.push("no-press");
    }
    if !states.is_empty() {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push('[');
        text.push_str(&states.join(","));
        text.push(']');
    }

    format!(
        "{:<4} {:<19} {:<12} {:<9} {}",
        element.reference.to_string(),
        crate::textutil::truncate_str(role, 19),
        centre,
        size,
        text
    )
    .trim_end()
    .to_string()
}

/// 解析 `list_elements` 的收窄参数。
///
/// 三个参数都是可选的；任何一个格式不对都**显式报错**而不是静默忽略——
/// 静默忽略会让模型以为「只看了这个区域」，实际拿到的是全窗口列表，
/// 它据此下的判断全部偏掉。
fn parse_element_query(input: &Value) -> Result<wyj_computer::element::ElementQuery> {
    let region = match input.get("region").and_then(Value::as_array) {
        Some(rect) => {
            anyhow::ensure!(
                rect.len() == 4,
                "`region` must be [x0, y0, x1, y1] in screenshot coordinates"
            );
            let number = |index: usize, name: &str| -> Result<f64> {
                rect[index]
                    .as_f64()
                    .ok_or_else(|| anyhow!("invalid `region` {name}"))
            };
            let (x0, y0, x1, y1) = (
                number(0, "x0")?,
                number(1, "y0")?,
                number(2, "x1")?,
                number(3, "y1")?,
            );
            anyhow::ensure!(x1 > x0 && y1 > y0, "`region` must have x1 > x0 and y1 > y0");
            Some(wyj_computer::element::Frame {
                x: x0,
                y: y0,
                width: x1 - x0,
                height: y1 - y0,
            })
        }
        None => None,
    };

    let role_filter = match input.get("role_filter") {
        Some(value) => {
            let list = value
                .as_array()
                .ok_or_else(|| anyhow!("`role_filter` must be an array of role names"))?;
            let mut roles = Vec::with_capacity(list.len());
            for item in list {
                let role = item
                    .as_str()
                    .ok_or_else(|| anyhow!("`role_filter` entries must be strings"))?;
                anyhow::ensure!(
                    !role.trim().is_empty(),
                    "`role_filter` entries must not be empty"
                );
                roles.push(role.to_string());
            }
            // 空数组等同于「不返回任何元素」，对模型永远是错误意图——
            // 当成没传更安全。
            if roles.is_empty() {
                None
            } else {
                Some(roles)
            }
        }
        None => None,
    };

    let limit = match input.get("limit").and_then(Value::as_u64) {
        Some(limit) => {
            anyhow::ensure!(limit > 0, "`limit` must be at least 1");
            Some(limit as usize)
        }
        None => None,
    };

    // `include_text` 只接受真布尔。用 `as_bool()` 而不是 `unwrap_or(false)`：
    // 传了 `"true"`（字符串）说明模型对 schema 的理解已经偏了，静默当 false
    // 会让它以为打开了文本通道、实际拿到一份纯控件列表。
    let include_text = match input.get("include_text") {
        Some(value) => value
            .as_bool()
            .ok_or_else(|| anyhow!("`include_text` must be a boolean"))?,
        None => false,
    };

    Ok(wyj_computer::element::ElementQuery {
        region,
        role_filter,
        limit,
        include_text,
    })
}

/// 把 `from`/`to` 渲染成权限摘要里的一段文字。两端缺一就返回 `None`。
fn describe_drag_path(input: &Value) -> Option<String> {
    let pair = |field: &str| {
        let array = input.get(field).and_then(Value::as_array)?;
        Some((
            array.first().and_then(Value::as_i64)?,
            array.get(1).and_then(Value::as_i64)?,
        ))
    };
    let (from, to) = (pair("from")?, pair("to")?);
    Some(format!(
        "dragging from ({}, {}) to ({}, {})",
        from.0, from.1, to.0, to.1
    ))
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
    use wyj_computer::element::{ElementInfo, ElementSnapshot, Frame};

    /// 构造一个带几何的按钮，供渲染测试使用。
    fn element(reference: u32, role: &str, title: Option<&str>, with_press: bool) -> ElementInfo {
        ElementInfo {
            reference: ElementRef(reference),
            depth: 1,
            role: Some(role.to_string()),
            title: title.map(str::to_string),
            description: None,
            value: None,
            enabled: Some(true),
            focused: None,
            selected: None,
            actions: if with_press {
                vec!["AXPress".to_string()]
            } else {
                Vec::new()
            },
            value_settable: false,
            selected_text_settable: false,
            frame: Some(Frame {
                x: 10.0,
                y: 20.0,
                width: 40.0,
                height: 40.0,
            }),
            frame_in_screenshot: Some(Frame {
                x: 10.0,
                y: 20.0,
                width: 40.0,
                height: 40.0,
            }),
        }
    }

    fn snapshot_with(elements: Vec<ElementInfo>) -> ElementSnapshot {
        ElementSnapshot {
            window_id: 7,
            generation: 3,
            tree_id: 42,
            content_fingerprint: 99,
            elements,
            truncated: false,
            filtered_out: 0,
            visited: 1,
            root_available: true,
            elapsed_ms: 1,
            has_more: false,
        }
    }

    fn capture() -> wyj_computer::Capture {
        wyj_computer::Capture {
            physical_width: 1600,
            physical_height: 1200,
            target_width: 800,
            target_height: 600,
            png: Vec::new(),
        }
    }

    #[test]
    fn definition_is_custom_and_explains_its_fallback() {
        let definition = AppComputerTool::new(1280, Duration::from_secs(2)).definition();
        assert!(definition.native.is_none());
        assert_eq!(
            definition.input_schema["properties"]["action"]["enum"][0],
            "list_windows"
        );
    }

    /// OpenAI 线拿不到截图：工具必须**主动声明**这一点，而不是继续按
    /// 「读附带截图」的描述工作。
    ///
    /// 没有这条声明，模型会照着 DESCRIPTION 里那套截图工作流规划，随后收到
    /// 一句 `[image omitted: ...]`——它看不到画面却以为自己看过，然后开始
    /// 描述界面。这类编造比「工具不可用」难查得多，因为结果里没有任何错误。
    #[test]
    fn without_an_image_channel_the_description_declares_it() {
        let with_images = AppComputerTool::new(1280, Duration::from_secs(2)).definition();
        let text_only = AppComputerTool::new(1280, Duration::from_secs(2))
            .without_image_channel()
            .definition();
        assert!(
            !with_images
                .description
                .contains("images cannot be delivered"),
            "有图片通道时不该出现这段声明"
        );
        assert!(
            text_only.description.contains("images cannot be delivered"),
            "没有图片通道时必须声明"
        );
        assert!(
            text_only.description.contains("include_text:true"),
            "必须把模型引向唯一可行的文本读取路径"
        );
        // 动作面与提示文案的差异**只有两处**：`action` 枚举少了 `screenshot`，
        // 以及 `ms` 的说明从「截图确认」改成「重读元素」。其余字段与约束逐字
        // 相同。收紧到这个粒度是为了两边都钉住：看不见画面的模型被授予
        // screenshot 等于给一个必然浪费往返的陷阱；而多撤任何一项（如
        // `list_elements`）就是把文本路径也断了。
        assert_ne!(with_images.input_schema, text_only.input_schema);
        let normalized = |d: &ToolDefinition| {
            let mut v = d.input_schema.clone();
            v["properties"]["action"]["enum"] = serde_json::json!([]);
            v["properties"]["ms"]["description"] = serde_json::json!("");
            v
        };
        assert_eq!(
            normalized(&with_images),
            normalized(&text_only),
            "除 action 枚举与 ms 说明外，schema 必须完全一致"
        );
    }

    /// 没有图片通道时，回执里绝不能出现 image part——那正是被
    /// `openai.rs` 降级成占位符、让模型以为自己看过画面的东西。
    #[test]
    fn attach_capture_degrades_to_text_without_an_image_channel() {
        let png = "iVBORw0KGgo=".to_string();
        let with_images = AppComputerTool::new(1280, Duration::from_secs(2))
            .attach_capture("body".to_string(), png.clone());
        assert!(with_images.parts.is_some(), "有图片通道时必须附 image part");

        let text_only = AppComputerTool::new(1280, Duration::from_secs(2))
            .without_image_channel()
            .attach_capture("body".to_string(), png);
        assert!(
            text_only.parts.is_none(),
            "没有图片通道时不得产出 image part"
        );
        assert!(text_only.content.starts_with("body"));
        assert!(text_only.content.contains("no image attached"));
    }

    /// `without_image_channel` 必须顺带关掉自动附图。否则每次变更动作都会
    /// 白白截一张图、编码成 base64、再被上游丢弃——纯浪费，而且描述里的
    /// 「动作会附截图」与实际回执不符。
    #[test]
    fn without_an_image_channel_disables_auto_screenshot() {
        let tool = AppComputerTool::new(1280, Duration::from_secs(2))
            .with_auto_screenshot(960)
            .without_image_channel();
        assert!(tool.auto_screenshot.is_none());
        assert!(!tool.image_channel);
    }

    /// `screenshot` 在无图片通道时**必须从 action 枚举里消失**。
    ///
    /// 只把返回值降级成文本不够：动作名还在，模型的训练先验就会驱使它去调
    /// （「要看清界面当然先截图」），每次调用还要过一遍权限确认、真截一张图、
    /// 编码成模型看不见的 base64。动作面本身才是模型能看到的唯一信号。
    #[test]
    fn screenshot_is_absent_from_the_action_enum_without_an_image_channel() {
        let actions = |tool: &AppComputerTool| -> Vec<String> {
            tool.definition().input_schema["properties"]["action"]["enum"]
                .as_array()
                .expect("action enum")
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        };
        let with_images = AppComputerTool::new(1280, Duration::from_secs(2));
        let text_only = AppComputerTool::new(1280, Duration::from_secs(2)).without_image_channel();

        assert!(actions(&with_images).iter().any(|a| a == "screenshot"));
        assert!(
            !actions(&text_only).iter().any(|a| a == "screenshot"),
            "无图片通道时不得暴露 screenshot"
        );
        // 其余动作面一个都不能少：降级只摘掉 screenshot。
        let mut expected = actions(&with_images);
        expected.retain(|a| a != "screenshot");
        assert_eq!(actions(&text_only), expected);

        // `wait` 的说明也不能再让模型「截图确认」。
        let ms_hint = text_only.definition().input_schema["properties"]["ms"]["description"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert!(!ms_hint.contains("screenshot"));
        assert!(ms_hint.contains("list_elements"));
    }

    /// 即便模型凭旧上下文硬发一个 `screenshot`，也必须被明确拒绝并指路，
    /// 而不是默默返回一段它读不懂的文本。
    #[tokio::test]
    async fn an_explicit_screenshot_call_is_rejected_without_an_image_channel() {
        struct Ctx;
        #[async_trait]
        impl ToolContext for Ctx {
            fn cwd(&self) -> &std::path::Path {
                std::path::Path::new(".")
            }
            fn is_allowed(&self, _name: &str, _input: &Value) -> bool {
                true
            }
        }

        let tool = AppComputerTool::new(1280, Duration::from_secs(2)).without_image_channel();
        let result = tool
            .run(
                serde_json::json!({"action": "screenshot", "window_id": 1}),
                &Ctx,
            )
            .await
            .expect("run returns a result");
        assert!(result.is_error, "无图片通道时的 screenshot 必须是错误");
        assert!(result.content.contains("list_elements"));
    }

    /// 坐标降级必须**在描述里写明**——模型要能预判「点了会发生什么」，也要知道
    /// 光标会被短暂借用。
    ///
    /// 这条替换掉了原先的 `NEVER silently falls back` 断言：那条守护的是
    /// 「绝不降级」，而「AX 树空洞的应用根本点不动」正是本次要解决的问题。
    /// 降级本身不放松任何安全边界——它仍走同一套租约与前台校验、事件带
    /// `INPUT_EVENT_MARKER`（不被自己的 Event Tap 误判成用户抢占）、光标
    /// 无条件复原。变的是「可不可以降级」，不是「降级要不要守规矩」。
    #[test]
    fn the_description_states_the_cursor_warp_fallback_and_its_limits() {
        let definition = AppComputerTool::new(1280, Duration::from_secs(2)).definition();
        assert!(
            definition.description.contains("cursor warp"),
            "描述必须说明存在光标瞬移降级"
        );
        assert!(
            definition.description.contains("returns the cursor"),
            "描述必须说明光标会被移回原处"
        );
        assert!(
            definition.description.contains("root_available:false"),
            "描述必须告诉模型什么情况下该用坐标而不是 element_ref"
        );
    }

    #[test]
    fn read_only_actions_do_not_request_permission() {
        let tool = AppComputerTool::new(1280, Duration::from_secs(2));
        assert!(!tool.needs_permission(&serde_json::json!({"action": "screenshot"})));
        // 列举元素只读 AX 树，不动输入，必须与截图同属免权限档。
        assert!(!tool.needs_permission(&serde_json::json!({"action": "list_elements"})));
        // `read_text` 只截图 + 跑 OCR，不碰输入，同样免权限。
        assert!(!tool.needs_permission(&serde_json::json!({"action": "read_text"})));
        // `wait` 只是睡眠，没有任何输入副作用。
        assert!(!tool.needs_permission(&serde_json::json!({"action": "wait", "ms": 500})));
        assert!(tool.needs_permission(&serde_json::json!({"action": "click"})));
        // recipe 会驱动别的应用、还可能弹 TCC 模态框，**必须**逐次确认。
        assert!(
            tool.needs_permission(
                &serde_json::json!({"action": "run_recipe", "recipe": "chrome_open_url"})
            ),
            "run_recipe 绝不能进入免确认白名单：它执行的是自己编译的脚本，\
             但仍会真实改动用户的其他应用"
        );
    }

    /// recipe 的 schema `enum` 必须与 `wyj_computer::applescript::RECIPES`
    /// **逐项一致**。
    ///
    /// 两处各写一份必然会漂移：加了 recipe 却忘了改 schema，模型永远看不到
    /// 那条；删了 recipe 却留着 enum，模型会调用一个必然报错的 op。schema 的
    /// `enum` 在部分客户端还会被忽略，所以描述文本里也必须有指引。
    #[test]
    fn recipe_enum_matches_the_shipped_table() {
        let definition = AppComputerTool::new(1280, Duration::from_secs(2)).definition();
        let declared: Vec<&str> = definition.input_schema["properties"]["recipe"]["enum"]
            .as_array()
            .expect("recipe enum")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        let shipped = wyj_computer::applescript::op_names();
        assert_eq!(
            declared, shipped,
            "schema 里的 recipe 清单必须与 applescript::RECIPES 完全一致"
        );
        // `run_recipe` 必须在 action enum 里，且 `args` 字段存在。
        let actions = definition.input_schema["properties"]["action"]["enum"]
            .as_array()
            .expect("action enum");
        assert!(actions.iter().any(|a| a == "run_recipe"));
        assert!(definition.input_schema["properties"].get("args").is_some());
        // 描述必须说明「参数是数据、不会被拼进脚本」与「首次会弹授权框」，
        // 前者决定模型敢不敢传带引号的值，后者决定它知不知道失败可能来自用户。
        assert!(definition.description.contains("run_recipe"));
        assert!(definition.description.contains("Automation permission"));
    }

    /// `wait` 必须出现在 schema 里并带 `ms` 字段——模型看不见的 action 等于
    /// 不存在，它会继续在动画中途截图然后误判界面状态。
    #[test]
    fn definition_exposes_the_wait_action() {
        let definition = AppComputerTool::new(1280, Duration::from_secs(2)).definition();
        let actions = definition.input_schema["properties"]["action"]["enum"]
            .as_array()
            .expect("action enum");
        assert!(actions.iter().any(|a| a == "wait"));
        assert!(definition.input_schema["properties"].get("ms").is_some());
        // 描述必须告诉模型「动作成功会附截图」与「先 wait 再截图」，
        // 否则它仍然会凭旧截图推断结果。
        assert!(definition.description.contains("fresh screenshot"));
        assert!(definition.description.contains("`wait`"));
        // 跨窗口重发现：对话框是独立窗口，不在旧窗口的截图里。
        assert!(definition.description.contains("window_capture"));
    }

    /// 描述必须写明剪贴板通道存在且会借用剪贴板——模型要能预判「打了这句话
    /// 会发生什么」，用户也要能理解为什么剪贴板会短暂变化。
    #[test]
    fn the_description_states_the_clipboard_channel_and_its_cost() {
        let definition = AppComputerTool::new(1280, Duration::from_secs(2)).definition();
        assert!(
            definition.description.contains("clipboard paste"),
            "描述必须说明存在剪贴板粘贴通道"
        );
        assert!(
            definition.description.contains("Chinese"),
            "必须说明这条通道覆盖非拉丁文字——这正是它存在的理由"
        );
        assert!(
            definition.description.contains("restored afterwards"),
            "必须说明剪贴板会被恢复，否则用户无法判断代价"
        );
    }

    /// 拖拽是**副作用最强的动作**（能移动文件、改滑块），必须与 click 同权限档，
    /// 且 schema 里要暴露 `from`/`to`——模型看不见的参数等于不存在。
    #[test]
    fn drag_is_exposed_and_requires_permission() {
        let tool = AppComputerTool::new(1280, Duration::from_secs(2));
        let definition = tool.definition();
        let actions = definition.input_schema["properties"]["action"]["enum"]
            .as_array()
            .expect("action enum");
        assert!(actions.iter().any(|a| a == "drag"));
        assert!(definition.input_schema["properties"].get("from").is_some());
        assert!(definition.input_schema["properties"].get("to").is_some());
        assert!(tool.needs_permission(&serde_json::json!({
            "action": "drag",
            "from": [10, 20],
            "to": [30, 40],
        })));
        // 描述必须说明插值存在——模型要知道这不是「按下+抬起」两下。
        assert!(definition.description.contains("interpolated steps"));
    }

    /// 权限摘要必须带上拖拽两端：auto 分类器只看这段文字，一个「drag」
    /// 完全看不出是把什么东西从哪儿拖到哪儿。
    #[test]
    fn action_summary_shows_both_drag_endpoints() {
        let tool = AppComputerTool::new(1280, Duration::from_secs(2));
        let summary = tool.action_summary(&serde_json::json!({
            "action": "drag",
            "window_id": 5,
            "from": [10, 20],
            "to": [300, 400],
        }));
        assert!(summary.contains("dragging from (10, 20)"), "{summary}");
        assert!(summary.contains("to (300, 400)"), "{summary}");
        // 只给一端时不该渲染出半句。
        let partial = tool.action_summary(&serde_json::json!({
            "action": "drag",
            "window_id": 5,
            "from": [10, 20],
        }));
        assert!(!partial.contains("dragging from"), "{partial}");
    }

    /// `required_coordinate` 的错误消息必须带真实字段名：模型收到
    /// 「missing `coordinate`」而它明明传了 `from`，只会重新猜参数。
    #[test]
    fn coordinate_errors_name_the_actual_field() {
        let error = required_coordinate(&serde_json::json!({}), "from")
            .unwrap_err()
            .to_string();
        assert!(error.contains("`from`"), "{error}");
        let error = required_coordinate(&serde_json::json!({"to": [1]}), "to")
            .unwrap_err()
            .to_string();
        assert!(error.contains("`to`"), "{error}");
        assert!(error.contains("exactly 2 integers"), "{error}");
    }

    /// 紧凑格式必须**保住 tree_id**：漏掉它，模型后续每一个动作都会
    /// `target_changed`，整条 GUI 任务直接卡死。
    #[test]
    fn compact_rendering_keeps_the_tree_id_and_geometry() {
        let snapshot = snapshot_with(vec![element(0, "AXButton", Some("Save"), true)]);
        let text = render_element_list(&snapshot, &capture(), false);
        assert!(text.contains("tree_id=42"), "{text}");
        assert!(text.contains("window=7"), "{text}");
        assert!(text.contains("capture=800x600"), "{text}");
        // 元素行：ref / role（去掉 AX 前缀）/ 中心点 / 尺寸 / 标题。
        assert!(text.contains("e0"), "{text}");
        assert!(text.contains("Button"), "{text}");
        assert!(!text.contains("AXButton"), "role 前缀应被去掉：{text}");
        assert!(text.contains("(30,40)"), "应给出中心点而不是左上角：{text}");
        assert!(text.contains("\"Save\""), "{text}");
    }

    /// 紧凑格式必须比 pretty JSON 小一个量级——这是这次改动的全部目的。
    #[test]
    fn compact_rendering_is_far_smaller_than_json() {
        let elements: Vec<_> = (0..40)
            .map(|index| element(index, "AXButton", Some("Some Button Label"), true))
            .collect();
        let snapshot = snapshot_with(elements);
        let compact = render_element_list(&snapshot, &capture(), false);
        let json = render_element_list(&snapshot, &capture(), true);
        assert!(
            compact.len() * 4 < json.len(),
            "紧凑格式应至少小 4 倍：compact={} json={}",
            compact.len(),
            json.len()
        );
    }

    /// `format: json` 仍然给出完整字段（排障用）。
    #[test]
    fn json_rendering_keeps_every_field() {
        let snapshot = snapshot_with(vec![element(0, "AXButton", Some("Save"), true)]);
        let text = render_element_list(&snapshot, &capture(), true);
        assert!(text.contains("\"tree_id\": 42"), "{text}");
        assert!(text.contains("\"content_fingerprint\""), "{text}");
        assert!(text.contains("\"frame_in_screenshot\""), "{text}");
        assert!(text.contains("\"AXButton\""), "{text}");
    }

    /// `no-press` 标记必须出现：它直接决定模型该不该退回坐标路线。
    ///
    /// **纯文本节点是刻意的例外**：`AXStaticText` / `AXHeading` /
    /// `AXListMarker` 本来就不能按，每行挂一个 `no-press` 只是噪音，role
    /// 名已经说明了它只能读。所以这里用一个「看起来能按、实际没有 AXPress」
    /// 的控件来验证这条标记，而不是拿文本节点当反例。
    #[test]
    fn an_element_without_axpress_is_flagged() {
        let mut with_press = element(0, "AXButton", Some("Save"), true);
        with_press.actions = vec!["AXPress".into()];
        // `AXToolbarButton` 属于控件白名单，但没有 AXPress → 必须标出来。
        let mut without_press = element(1, "AXToolbarButton", Some("Label"), true);
        without_press.actions = Vec::new();
        let snapshot = snapshot_with(vec![with_press, without_press]);
        let text = render_element_list(&snapshot, &capture(), false);
        let lines: Vec<&str> = text.lines().collect();
        let press_line = lines.iter().find(|line| line.starts_with("e0")).unwrap();
        let no_press_line = lines.iter().find(|line| line.starts_with("e1")).unwrap();
        assert!(!press_line.contains("no-press"), "{press_line}");
        assert!(no_press_line.contains("no-press"), "{no_press_line}");
    }

    /// 纯文本节点不标 `no-press`，且文本截断上限放宽到 120 字符。
    ///
    /// 两条都是 `include_text` 配套的展示规则：模型要能一眼区分「这是可读的
    /// 内容」和「这是不能按的控件」，而文本的价值在内容本身——截到 60 会把
    /// 一段话腰斩成无法据此判断的半句。
    ///
    /// 注意 `truncate_str` 的第二个参数是**字节**上限（见 `core::textutil`），
    /// 所以这里用 ASCII 验证长度语义，用中文验证「比控件宽松」这一定性结论。
    #[test]
    fn static_text_lines_skip_no_press_and_keep_more_characters() {
        // ASCII：100 字节 < 120，完整保留。
        let mut ascii = element(0, "AXStaticText", Some(&"x".repeat(100)), true);
        ascii.actions = Vec::new();
        let snapshot = snapshot_with(vec![ascii]);
        let text = render_element_list(&snapshot, &capture(), false);
        let line = text.lines().find(|line| line.starts_with("e0")).unwrap();
        assert!(!line.contains("no-press"), "{line}");
        // 100 个字符完整保留（控件的 60 字节上限会把它砍到 60）。
        assert!(line.contains(&"x".repeat(100)), "{line}");

        // 中文 100 个字符 = 300 字节，超过 120 必须被截断——但仍比控件的
        // 60 字节宽松得多（同样输入，控件只会留下 20 个汉字）。
        let mut cjk = element(0, "AXStaticText", Some(&"字".repeat(100)), true);
        cjk.actions = Vec::new();
        let snapshot = snapshot_with(vec![cjk]);
        let text = render_element_list(&snapshot, &capture(), false);
        let line = text.lines().find(|line| line.starts_with("e0")).unwrap();
        assert!(line.matches('字').count() > 20, "{line}");
    }

    /// 没有截图坐标的元素仍要列出（它能被 ref 操作），只是坐标列显示 `-`。
    #[test]
    fn an_element_without_geometry_is_still_listed() {
        let mut item = element(0, "AXButton", Some("Save"), true);
        item.frame_in_screenshot = None;
        let snapshot = snapshot_with(vec![item]);
        let text = render_element_list(&snapshot, &capture(), false);
        let line = text.lines().find(|line| line.starts_with("e0")).unwrap();
        assert!(line.contains("-"), "{line}");
        assert!(line.contains("\"Save\""), "{line}");
    }

    #[test]
    fn format_flag_only_accepts_json_explicitly() {
        assert!(!wants_json(&serde_json::json!({})));
        assert!(!wants_json(&serde_json::json!({"format": "compact"})));
        assert!(wants_json(&serde_json::json!({"format": "json"})));
        assert!(wants_json(&serde_json::json!({"format": "JSON"})));
    }

    /// 收窄参数必须是**全有或全无**：给了就按它过滤，格式不对就报错。
    /// 静默忽略最糟——模型以为只看了某区域，实际拿到全窗口列表。
    #[test]
    fn element_query_parsing_accepts_the_documented_shapes() {
        let query = parse_element_query(&serde_json::json!({
            "region": [10, 20, 300, 400],
            "role_filter": ["Button", "TextField"],
            "limit": 50,
        }))
        .unwrap();
        let region = query.region.expect("region");
        assert_eq!(
            (region.x, region.y, region.width, region.height),
            (10.0, 20.0, 290.0, 380.0)
        );
        assert_eq!(query.role_filter.as_ref().unwrap().len(), 2);
        assert_eq!(query.limit, Some(50));

        // 全都不给 = 不过滤。
        let query = parse_element_query(&serde_json::json!({})).unwrap();
        assert!(query.region.is_none());
        assert!(query.role_filter.is_none());
        assert!(query.limit.is_none());
    }

    #[test]
    fn element_query_parsing_rejects_malformed_input() {
        // 区域必须是 4 个数且 x1>x0 / y1>y0。
        for bad in [
            serde_json::json!({"region": [1, 2, 3]}),
            serde_json::json!({"region": [100, 20, 10, 400]}),
            serde_json::json!({"region": ["a", 2, 3, 4]}),
        ] {
            assert!(parse_element_query(&bad).is_err(), "{bad}");
        }
        // role_filter 必须是字符串数组。
        assert!(parse_element_query(&serde_json::json!({"role_filter": "Button"})).is_err());
        assert!(parse_element_query(&serde_json::json!({"role_filter": [1]})).is_err());
        // limit 至少 1。
        assert!(parse_element_query(&serde_json::json!({"limit": 0})).is_err());
    }

    /// 空 `role_filter` 数组当成「没传」：空过滤等于「什么都不返回」，
    /// 那对模型永远是错误意图。
    #[test]
    fn an_empty_role_filter_means_no_filter() {
        let query = parse_element_query(&serde_json::json!({"role_filter": []})).unwrap();
        assert!(query.role_filter.is_none());
    }

    /// 右键上下文菜单必须可得，且必须告诉模型「菜单是独立窗口」——
    /// 不说这一点，模型会以为右键没生效。
    #[test]
    fn show_menu_is_exposed_and_requires_permission() {
        let tool = AppComputerTool::new(1280, Duration::from_secs(2));
        let definition = tool.definition();
        let actions = definition.input_schema["properties"]["action"]["enum"]
            .as_array()
            .expect("action enum");
        assert!(actions.iter().any(|a| a == "show_menu"));
        // 弹菜单有副作用（可能改状态），与 click 同权限档。
        assert!(tool.needs_permission(&serde_json::json!({
            "action": "show_menu",
            "window_id": 5,
            "coordinate": [10, 20],
        })));
        assert!(
            definition.description.contains("show_menu"),
            "描述必须提到这个 action"
        );
        assert!(
            definition.description.contains("separate window"),
            "必须说明菜单是独立窗口，否则模型会以为没生效"
        );
    }

    /// 自动截图开关是**加性**的：不调用 `with_auto_screenshot` 时行为与从前
    /// 完全一致，避免 `[computer_use].auto_screenshot = false` 意外变成
    /// 「每步都附图」。
    #[test]
    fn auto_screenshot_is_opt_in() {
        let tool = AppComputerTool::new(1280, Duration::from_secs(2));
        assert_eq!(tool.auto_screenshot, None);
        let tool = tool.with_auto_screenshot(960);
        assert_eq!(tool.auto_screenshot, Some(960));
    }

    /// 元素观察必须留下每个 ref 的截图坐标，否则「AX 动作结构性失败」时
    /// 降级无坐标可用，只能把错误原样丢回模型。
    #[test]
    fn observation_records_element_centres_for_degradation() {
        let observation = ElementObservation {
            tree_id: 1,
            content_fingerprint: 2,
            centers: vec![Some((10, 20)), None, Some((300, 400))],
            query: wyj_computer::element::ElementQuery::default(),
        };
        assert_eq!(observation.center_of(ElementRef(0)), Some((10, 20)));
        // 没有几何的元素降级不了，必须如实返回 None 而不是编一个 (0,0)。
        assert_eq!(observation.center_of(ElementRef(1)), None);
        assert_eq!(observation.center_of(ElementRef(2)), Some((300, 400)));
        // 越界序号同样返回 None。
        assert_eq!(observation.center_of(ElementRef(99)), None);
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

    /// 只校验 `tree_id` 是不够的：`tree_id` 含遍历时刻，模型完全可以拿着一个
    /// **正确但过期**的 tree_id 回来。这条测试钉住「观察记录必须带内容指纹」，
    /// 否则 `execute_with_element` 就退化成按序号盲点。
    #[test]
    fn observation_carries_the_content_fingerprint_for_drift_detection() {
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
        tool.element_observations.lock().unwrap().insert(
            7,
            ElementObservation {
                tree_id: 11,
                content_fingerprint: 0xdead_beef,
                centers: vec![None; 8],
                query: wyj_computer::element::ElementQuery::default(),
            },
        );
        let (reference, observation) = tool
            .resolve_element(
                &target,
                &serde_json::json!({"element_ref": "e3", "tree_id": 11}),
            )
            .unwrap();
        assert_eq!(reference, ElementRef(3));
        assert_eq!(observation.tree_id, 11);
        // 指纹必须原样带出，动作侧才能拿它去和当前树比对。
        assert_eq!(observation.content_fingerprint, 0xdead_beef);

        // tree_id 对不上仍然要拒——两道校验都不能少。
        let error = tool
            .resolve_element(
                &target,
                &serde_json::json!({"element_ref": "e3", "tree_id": 12}),
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("stale"), "{error}");
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
