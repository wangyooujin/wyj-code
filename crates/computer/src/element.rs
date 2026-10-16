//! macOS Accessibility 元素索引：把「模型猜坐标」换成「模型指语义元素」。
//!
//! # 为什么需要它
//!
//! [`crate::accessibility::inspect`] 已经在读 AX 树，但它以**坐标**为入口
//! （`element_at` 走 `AXUIElementCopyElementAtPosition`）。于是模型必须先在
//! 下采样截图上猜对位置，才知道那是什么——grounding 完全依赖像素，AX 树只被
//! 当成了坐标验证器。真机实测这正是精度瓶颈：纯坐标路线在 ScreenSpot Pro 上
//! 只有 23.4（OpenAI CUA）/ 39.6（OmniParser）/ 61.6（UI-TARS）。
//!
//! 本模块提供 Anthropic `browser use tool` 同构的方案：遍历 AX 树产出带稳定
//! 引用的元素列表，模型引用 `e12` 而不是猜像素。macOS AX 树与 DOM accessibility
//! tree 同构，所以那套 `read_page` → `find` → `ref` 的模式可以直接搬过来。
//!
//! # 真机实测数据（`examples/ax_probe.rs`，2026-10-16）
//!
//! | App | 类型 | 节点 | 可交互 | 遍历耗时 |
//! |---|---|---|---|---|
//! | Chrome | Blink | 58 | 56（97%） | 2.1s（含 2.1s 截图） |
//! | Safari | WebKit | 127 | 24 | 2.3s |
//! | 访达 | AppKit | 3001 截断 | 393 | 9.3s |
//! | 同花顺 | 混合自绘 | 3001 截断 | 29（1%） | 2.8s |
//!
//! 两条由此确定的设计约束：
//!
//! 1. **性能**。逐属性 IPC 是 3000 节点 = 15000 次往返（访达 18.2s）。必须用
//!    `copy_multiple_attribute_values` 一次批量，把每节点 5 次 IPC 降到 2 次
//!    （实测 -49%）。剩余耗时主要由 `copy_action_names` 决定，无法批量。
//! 2. **过滤比遍历更重要**。同花顺 2349 个 `AXTextField` / 588 个 `AXRow` 是
//!    行情表格数据，真正控件只有 24 个 `AXButton`——「可写就保留」这条规则
//!    会把整张行情表灌进模型上下文。必须对同父节点下的同类 role 设数量阈值。

use crate::target::WindowTarget;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// 遍历预算。全部来自 `ax_probe` 实测，见 [`TraversalBudget::default`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TraversalBudget {
    /// 最大树深度。超出部分不再展开。
    pub max_depth: u16,
    /// 最大访问节点数。超出即停并置 `truncated`。
    pub max_nodes: usize,
    /// 最大耗时（含根定位与属性读取）。
    pub max_duration: std::time::Duration,
    /// 单个节点最多展开的子节点数，防病态表格。
    pub max_children_per_node: usize,
    /// 同一父节点下同类 role 最多保留几个。见 [`should_keep`]。
    pub max_siblings_per_role: usize,
}

impl Default for TraversalBudget {
    /// 实测依据：3000 节点 ≈ 9.3s，故 400 节点 ≈ 1.2s；截图固定开销 2.1s
    /// 留出余量故取 2s。同花顺的表格单元格埋在深层嵌套里，depth 不能低于 10。
    fn default() -> Self {
        Self {
            max_depth: 10,
            max_nodes: 400,
            max_duration: std::time::Duration::from_millis(2000),
            max_children_per_node: 50,
            max_siblings_per_role: 8,
        }
    }
}

/// 单次快照内稳定的元素引用。仅在同一次 `list_elements` 的返回内有效。
///
/// 对齐 Anthropic 的 `ref_N`：它同样无法跨快照复用，stale 时必须重新读取。
/// 刻意选 `u32` 序号而非内容哈希或 AX 路径——序号对模型最短、最好引用，
/// 而稳定性由 [`ElementSnapshot::tree_id`] 整体失效来保证（见
/// [`ElementSnapshot::is_stale`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ElementRef(pub u32);

impl std::fmt::Display for ElementRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "e{}", self.0)
    }
}

/// 轴对齐矩形。两种坐标系见 [`ElementInfo::frame`]。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Frame {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Frame {
    pub fn center(&self) -> (f64, f64) {
        (self.x + self.width / 2.0, self.y + self.height / 2.0)
    }

    pub fn is_empty(&self) -> bool {
        self.width <= 0.0 || self.height <= 0.0
    }
}

/// 单个元素的语义 + 几何。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElementInfo {
    /// 本次快照内的稳定引用，模型侧形如 `"e12"`。
    pub reference: ElementRef,
    pub depth: u16,
    /// `AXRole`，如 `"AXButton"`。
    pub role: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    /// `AXValue`，按 [`TraversalBudget`] 无关的长度上限截断。
    pub value: Option<String>,
    pub enabled: Option<bool>,
    pub focused: Option<bool>,
    pub selected: Option<bool>,
    /// 支持的 AX 动作名。含 `AXPress` 才可能后台点击。
    pub actions: Vec<String>,
    pub value_settable: bool,
    pub selected_text_settable: bool,
    /// 全局逻辑坐标（与 `AXPosition`/`AXSize` 同空间）。
    pub frame: Option<Frame>,
    /// 相对最近一次窗口截图的像素坐标，模型可直接与截图对照。
    ///
    /// 与 [`Self::frame`] 的区别：AX 报告的全局坐标可能落在窗口之外
    /// （实测 Ghostty 滚动条映射出截图坐标 2853 而截图宽 1280），这里已夹取
    /// 到捕获边界；完全落在窗口外的元素则不产出此字段。
    pub frame_in_screenshot: Option<Frame>,
}

/// 一次遍历的完整结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElementSnapshot {
    pub window_id: u32,
    /// 窗口身份代数，与 [`WindowTarget::generation`] 同源。
    pub generation: u64,
    /// 本次快照唯一 id。所有 [`ElementRef`] 只在它下面有效。
    pub tree_id: u64,
    pub elements: Vec<ElementInfo>,
    /// 因节点数 / 深度 / 耗时预算而截断。
    pub truncated: bool,
    /// 被同类 role 阈值丢弃的节点数（区别于 `truncated`）。
    pub filtered_out: usize,
    /// 遍历访问的节点总数（含被过滤的），用于诊断真实树规模。
    pub visited: usize,
    /// AX 树是否真的暴露了目标窗口的根元素。
    ///
    /// `false` 是**明确的降级信号**，不是「空列表」：调用方据此告诉模型
    /// 「这个 App 不暴露 AX 树，请改用坐标 + zoom」，避免模型反复重试。
    pub root_available: bool,
    /// 遍历耗时（不含截图）。
    pub elapsed_ms: u64,
}

impl ElementSnapshot {
    /// 窗口身份是否已经变了。变了则本快照全部 [`ElementRef`] 失效。
    pub fn is_stale(&self, target: &WindowTarget) -> bool {
        self.window_id != target.window_id || self.generation != target.generation
    }

    /// 某个引用是否仍指向本快照内的元素。
    pub fn contains(&self, reference: ElementRef) -> bool {
        self.elements.iter().any(|e| e.reference == reference)
    }

    pub fn get(&self, reference: ElementRef) -> Option<&ElementInfo> {
        self.elements.iter().find(|e| e.reference == reference)
    }
}

#[cfg(target_os = "macos")]
use crate::accessibility::AXUIElementRef;

/// 一个带**活句柄**的遍历结果：既能序列化成 [`ElementSnapshot`] 给模型看，
/// 也能在动作执行时把 `element_ref` 还原成真正的 `AXUIElement`。
///
/// 这层区分是 B 期的核心。模型看到的是纯数据的 ref，但执行 AXPress 时必须
/// 拿到元素对象——而 AX 句柄不能跨进程持久化，只能在同一次遍历内保留。
///
/// **刻意不是 `Send`/`Sync`**：`AXUIElement` 底层是 `UnsafeCell`，把它存进
/// 工具的 `Mutex<HashMap<…>>` 会让整个工具失去跨线程可用性（工具注册表按
/// `Arc<dyn Tool>` 共享）。因此句柄的完整生命周期由本 crate 内部掌管——
/// 见 [`execute_with_element`]：调用方只传可 `Send` 的 ref，动作在 crate
/// 内部重新定位后立刻执行，中间不跨线程边界。
pub struct ElementHandles {
    pub snapshot: ElementSnapshot,
    /// 与 `snapshot.elements` 同序的活句柄。
    ///
    /// 必须保留的另一个理由：**ref 只能对应到观察当时的那一个元素**。若在
    /// 动作时重新遍历再按序号取，同一个序号可能已指向别的元素——那正是
    /// Anthropic ref 契约明令禁止的「盲点坐标」。持有句柄让语义引用始终
    /// 指向模型看到过的东西。
    handles: Vec<AXUIElementRef>,
}

impl ElementHandles {
    /// 按 ref 取元素句柄。越界返回 `None`。
    pub fn handle(&self, reference: ElementRef) -> Option<&AXUIElementRef> {
        self.handles.get(reference.0 as usize)
    }

    pub fn len(&self) -> usize {
        self.handles.len()
    }

    pub fn is_empty(&self) -> bool {
        self.handles.is_empty()
    }

    /// 丢弃句柄，只留可序列化的快照。
    pub fn into_snapshot(self) -> ElementSnapshot {
        self.snapshot
    }
}

/// 单个字符串字段的截断上限。AX 的 `AXValue` 在表格/文本视图里可能是整篇文档，
/// 不截断会让单个元素吃掉大量 token。
pub const MAX_FIELD_CHARS: usize = 120;

/// 截断过长字段，超出部分以 `…` 结尾。
pub fn clamp_field(value: Option<String>) -> Option<String> {
    let value = value?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.chars().count() <= MAX_FIELD_CHARS {
        return Some(trimmed.to_string());
    }
    let mut out: String = trimmed.chars().take(MAX_FIELD_CHARS).collect();
    out.push('…');
    Some(out)
}

/// 该元素的 role 是否值得保留（与同父节点下的数量阈值正交）。
///
/// 保留条件按实测数据设计：行情表格的单元格多数「可写」，若只看可写性，
/// 同花顺 2349 个 `AXTextField` 会淹没上下文（实测可交互率 1%）。
/// 这里只认「真正的控件 role」，把表格数据留给 [`should_keep`] 的阈值规则
/// 决定是否少量采样。
pub fn is_interesting_role(role: Option<&str>) -> bool {
    matches!(
        role,
        Some(
            "AXButton"
                | "AXPopUpButton"
                | "AXCheckBox"
                | "AXRadioButton"
                | "AXTextField"
                | "AXTextArea"
                | "AXSecureTextField"
                | "AXComboBox"
                | "AXMenuButton"
                | "AXMenuItem"
                | "AXMenuBarItem"
                | "AXDisclosureTriangle"
                | "AXIncrementor"
                | "AXSlider"
                | "AXTabButton"
                | "AXLink"
                | "AXSearchField"
                | "AXToolbarButton"
                | "AXColorWell"
                | "AXSwitch"
        )
    )
}

/// 判定单个元素是否保留所需的全部输入。
///
/// 刻意收成结构体而不是七个平铺参数：这些字段必须**成组**演进（新增一个
/// 保留判据就要同时改签名和所有调用点），平铺参数会让编译器帮不上忙。
#[derive(Debug, Clone, Copy)]
pub struct KeepContext<'a> {
    pub role: Option<&'a str>,
    /// 是否有任何模型可读的内容（title / description / value 之一非空）。
    pub has_content: bool,
    /// 换算到最近一次窗口截图后的矩形；`None` 表示该元素落在窗口之外。
    pub frame_in_screenshot: Option<Frame>,
    /// 本元素是全树中该 role 的第几个（从 0 开始）。
    pub role_siblings_so_far: usize,
    pub max_siblings_per_role: usize,
}

/// 元素是否应进入输出列表。
///
/// 三条规则，从实测数据反推：
/// 1. 必须有实际内容——纯装饰节点永远丢弃；
/// 2. 必须可交互或是有意义的控件 role（[`is_interesting_role`]）；
/// 3. 同类 role 超过阈值时只保留前 N 个（[`TraversalBudget`] 的
///    `max_siblings_per_role`）——这是同花顺 1% 可交互率的直接对策：
///    2349 个 `AXTextField` 是行情表格数据，不压掉就会淹没上下文。
///
/// 纯函数，便于单测；调用方用 [`RoleBudget`] 维护计数。
pub fn should_keep(context: KeepContext<'_>) -> bool {
    if !context.has_content {
        return false;
    }
    // 零尺寸元素必然不可见，留着只会让模型误点。
    if let Some(frame) = context.frame_in_screenshot {
        if frame.is_empty() {
            return false;
        }
    }
    if !is_interesting_role(context.role) {
        return false;
    }
    context.role_siblings_so_far < context.max_siblings_per_role
}

/// 生成 [`ElementSnapshot::tree_id`]。
///
/// 由窗口身份 + 根元素指纹 + 遍历起点时刻派生：同一窗口在树未变时重跑会得到
/// 不同 `tree_id`（时刻参与哈希），这正是我们想要的——**引用永不跨快照复用**，
/// 每次 `list_elements` 强制模型重新确认当前界面。
pub fn tree_id_for(
    generation: u64,
    root_fingerprint: Option<&str>,
    started: std::time::SystemTime,
) -> u64 {
    // FNV-1a，与 target.rs 的 generation_for 保持同一套风格。
    let mut hash = 0xcbf29ce484222325u64;
    let mut push = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(0x100000001b3);
    };
    push(&generation.to_le_bytes());
    if let Some(fingerprint) = root_fingerprint {
        push(fingerprint.as_bytes());
    }
    match started.duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => push(&duration.as_nanos().to_le_bytes()),
        Err(_) => push(&0u64.to_le_bytes()),
    }
    hash
}

/// 按父节点统计同类 role 出现次数，供 [`should_keep`] 的阈值判断使用。
#[derive(Debug, Default)]
pub struct RoleBudget {
    counts: HashMap<String, usize>,
}

impl RoleBudget {
    /// 记录并返回这是该父节点下同类 role 的第几个（从 0 开始）。
    pub fn take(&mut self, role: Option<&str>) -> usize {
        let key = role.unwrap_or("<none>").to_string();
        let slot = self.counts.entry(key).or_default();
        let index = *slot;
        *slot += 1;
        index
    }
}

/// 把窗口目标翻译成一次元素快照（macOS 实现，见 `imp` 模块）。
///
/// 只读：不截图、不合成输入、不激活应用。
pub fn snapshot_elements(
    target: &WindowTarget,
    budget: &TraversalBudget,
) -> Result<ElementSnapshot> {
    Ok(imp::snapshot_handles(target, budget)?.into_snapshot())
}

/// 对一个元素可以做的语义动作。
///
/// 刻意做成**枚举而非闭包**：`AXUIElement` 来自 objc2，既不是 `Send` 也不是
/// `Sync`，一旦出现在跨 crate 的回调签名里，调用方（工具注册表要按
/// `Arc<dyn Tool>` 跨线程共享）就再也编译不过。用枚举把「做什么」表达清楚，
/// 把「怎么做」封死在 computer crate 内部。
#[derive(Debug, Clone, Copy)]
pub enum ElementAction<'a> {
    /// AXPress（含父链上溯）。
    Press,
    /// 写 AXValue / AXSelectedText。
    SetText(&'a str),
    /// AXIncrement / AXDecrement。
    Scroll { direction: &'a str, amount: u32 },
}

/// 遍历 AX 树并在**同一次遍历内**对 `reference` 指向的元素执行动作。
///
/// # 为什么重新遍历而不是缓存句柄
///
/// `AXUIElement` 不是 `Send`/`Sync`（底层 `UnsafeCell`），存不进工具层的
/// `Mutex<HashMap<…>>`——那会让 `Arc<dyn Tool>` 失去跨线程可用性。因此句柄
/// 的完整生命周期收敛在本函数内：遍历 → 按序号取句柄 → 立刻执行 → 返回。
///
/// # 为什么不违反 stale 契约
///
/// Anthropic 的 ref 契约禁止「拿旧 ref 去当前页面盲点」。这里不是盲点：
/// 调用方必须先用自己记住的 `tree_id` 校验过 ref 属于哪一次观察，
/// 本函数再做一次**独立复检**——序号必须仍落在当前树的元素范围内，
/// 否则报 `target_changed` 要求重新列举。界面漂移导致的错位由
/// [`ElementSnapshot::tree_id`] 的整体失效兜底。
pub fn execute_with_element(
    target: &WindowTarget,
    budget: &TraversalBudget,
    reference: ElementRef,
    action: ElementAction<'_>,
) -> Result<String> {
    imp::execute_with_element(target, budget, reference, action)
}

/// 遍历 AX 树并只做列举（不保留任何跨调用状态）。
pub fn snapshot_elements_standalone(target: &WindowTarget) -> Result<ElementSnapshot> {
    snapshot_elements(target, &TraversalBudget::default())
}

#[cfg(target_os = "macos")]
use macos as imp;

#[cfg(not(target_os = "macos"))]
use unsupported as imp;

#[cfg(target_os = "macos")]
mod macos {
    use super::{
        clamp_field, tree_id_for, ElementInfo, ElementRef, ElementSnapshot, Frame, RoleBudget,
        TraversalBudget,
    };
    use crate::accessibility::{self, AXUIElementRef, BatchAttributes};
    use crate::target::WindowTarget;
    use anyhow::{anyhow, Result};
    use std::collections::VecDeque;
    use std::time::Instant;

    /// 节点是否带有模型可读的内容。纯装饰节点（无任何文本/值）一律丢弃。
    fn has_content(attrs: &BatchAttributes) -> bool {
        attrs.title.is_some() || attrs.description.is_some() || attrs.value.is_some()
    }

    /// 按序号执行动作。句柄不跨函数边界，全程留在本 crate 内。
    pub fn execute_with_element(
        target: &WindowTarget,
        budget: &TraversalBudget,
        reference: ElementRef,
        action: super::ElementAction<'_>,
    ) -> Result<String> {
        let handles = snapshot_handles(target, budget)?;
        if !handles.snapshot.root_available {
            anyhow::bail!(
                "target_changed: this app exposes no accessibility tree; use `coordinate` instead and do not retry list_elements"
            );
        }
        if handles.snapshot.get(reference).is_none() {
            anyhow::bail!(
                "target_changed: element {reference} is not present in the current tree ({} elements); call list_elements again",
                handles.len()
            );
        }
        let handle = handles
            .handle(reference)
            .ok_or_else(|| anyhow!("target_changed: element {reference} has no live AX handle"))?
            .clone();

        match action {
            super::ElementAction::Press => {
                accessibility::press_handle(&handle)?;
                Ok("clicked element with AXPress".to_string())
            }
            super::ElementAction::SetText(text) => {
                let attribute = accessibility::set_text_handle(&handle, text)?;
                Ok(format!("set text on element using {attribute}"))
            }
            super::ElementAction::Scroll { direction, amount } => {
                if accessibility::scroll_handle(&handle, direction, amount)? {
                    Ok(format!(
                        "scrolled element {direction} x{amount} using Accessibility"
                    ))
                } else {
                    anyhow::bail!(
                        "requires_foreground_takeover: element does not expose AXIncrement/AXDecrement for `{direction}`"
                    )
                }
            }
        }
    }

    pub fn snapshot_handles(
        target: &WindowTarget,
        budget: &TraversalBudget,
    ) -> Result<super::ElementHandles> {
        let started_wall = std::time::SystemTime::now();
        let started = Instant::now();

        let root = match accessibility::find_window_element(target)? {
            Some(root) => root,
            None => {
                return Ok(super::ElementHandles {
                    snapshot: ElementSnapshot {
                        window_id: target.window_id,
                        generation: target.generation,
                        tree_id: tree_id_for(target.generation, None, started_wall),
                        elements: Vec::new(),
                        truncated: false,
                        filtered_out: 0,
                        visited: 0,
                        root_available: false,
                        elapsed_ms: started.elapsed().as_millis() as u64,
                    },
                    handles: Vec::new(),
                })
            }
        };

        let mut elements: Vec<ElementInfo> = Vec::new();
        let mut handles: Vec<AXUIElementRef> = Vec::new();
        let mut visited = 0usize;
        let mut filtered_out = 0usize;
        let mut truncated = false;

        let mut queue: VecDeque<(AXUIElementRef, u16)> = VecDeque::new();
        queue.push_back((root.clone(), 0));
        // **全树共享**的同类 role 计数。
        //
        // 不能每个节点各带一份：新起一份会让阈值永远从 0 计数，等于没生效
        // （真机实测访达仍吐出 25 个 AXTextField 就是这个 bug）。
        // 共享池下，「同类 role 超过 N 个」是全树语义——对「表格单元格会把
        // 上下文淹没」这个真实问题也正是需要的语义。
        let mut siblings = RoleBudget::default();

        while let Some((node, depth)) = queue.pop_front() {
            if visited >= budget.max_nodes {
                truncated = true;
                break;
            }
            if depth > 0 && started.elapsed() >= budget.max_duration {
                truncated = true;
                break;
            }
            visited += 1;

            let attrs = accessibility::batch_attributes(&node);
            let role = attrs.role.clone();

            let frame = match (attrs.position, attrs.size) {
                (Some((x, y)), Some((width, height))) => Some(Frame {
                    x,
                    y,
                    width,
                    height,
                }),
                _ => None,
            };
            let frame_in_screenshot = frame.and_then(|frame| {
                let (sx, sy) = target.global_to_screenshot(frame.x, frame.y)?;
                let (cx, cy) =
                    target.global_to_screenshot(frame.x + frame.width, frame.y + frame.height)?;
                Some(Frame {
                    x: sx,
                    y: sy,
                    width: (cx - sx).max(0.0),
                    height: (cy - sy).max(0.0),
                })
            });

            // AX 报告的元素中心落在窗口矩形之外：多半是跨显示器残留或已不可见，
            // 交给模型只会产生无效点击目标。整块丢弃。
            let offscreen = frame_in_screenshot.is_none() && frame.is_some();
            if !offscreen {
                let sibling_index = siblings.take(role.as_deref());
                if super::should_keep(super::KeepContext {
                    role: role.as_deref(),
                    has_content: has_content(&attrs),
                    frame_in_screenshot,
                    role_siblings_so_far: sibling_index,
                    max_siblings_per_role: budget.max_siblings_per_role,
                }) {
                    handles.push(node.clone());
                    elements.push(ElementInfo {
                        reference: ElementRef(elements.len() as u32),
                        depth,
                        role,
                        title: clamp_field(attrs.title),
                        description: clamp_field(attrs.description),
                        value: clamp_field(attrs.value),
                        enabled: attrs.enabled,
                        focused: attrs.focused,
                        selected: attrs.selected,
                        actions: attrs.actions,
                        value_settable: attrs.settable,
                        selected_text_settable: attrs.selected_text_settable,
                        frame,
                        frame_in_screenshot,
                    });
                } else {
                    filtered_out += 1;
                }
            } else {
                filtered_out += 1;
            }

            if depth >= budget.max_depth {
                truncated = true;
                continue;
            }
            let mut children = attrs.children;
            children.truncate(budget.max_children_per_node);
            for child in children {
                queue.push_back((child, depth + 1));
            }
        }

        Ok(super::ElementHandles {
            snapshot: ElementSnapshot {
                window_id: target.window_id,
                generation: target.generation,
                tree_id: tree_id_for(target.generation, Some(&target.app_name), started_wall),
                elements,
                truncated,
                filtered_out,
                visited,
                root_available: true,
                elapsed_ms: started.elapsed().as_millis() as u64,
            },
            handles,
        })
    }
}

#[cfg(not(target_os = "macos"))]
mod unsupported {
    use super::{ElementSnapshot, TraversalBudget};
    use crate::target::WindowTarget;
    use anyhow::{anyhow, Result};

    pub fn snapshot_elements(
        target: &WindowTarget,
        _budget: &TraversalBudget,
    ) -> Result<ElementSnapshot> {
        anyhow::bail!("AX 元素索引仅支持 macOS，当前平台不可用")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> WindowTarget {
        WindowTarget {
            window_id: 1,
            pid: 2,
            app_name: "App".into(),
            title: "Title".into(),
            x: 100,
            y: 50,
            width: 800,
            height: 600,
            target_width: 400,
            target_height: 300,
            generation: 7,
            focused: false,
        }
    }

    #[test]
    fn global_to_screenshot_is_the_inverse_of_screenshot_to_global() {
        let t = target();
        let (gx, gy) = t.screenshot_to_global(200, 150).unwrap();
        let (sx, sy) = t.global_to_screenshot(gx, gy).unwrap();
        assert!((sx - 200.0).abs() < 0.01, "{sx}");
        assert!((sy - 150.0).abs() < 0.01, "{sy}");
    }

    #[test]
    fn global_to_screenshot_clamps_out_of_bounds() {
        // 实测 Ghostty 滚动条：AX 报告全局 x=1902 映射出截图坐标 2853，
        // 而截图宽仅 1280——必须夹取而不是让模型去点不存在的位置。
        let t = target();
        let (sx, sy) = t.global_to_screenshot(100.0 + 2000.0, 50.0).unwrap();
        assert!((0.0..=399.0).contains(&sx), "{sx}");
        assert!((0.0..=299.0).contains(&sy), "{sy}");
    }

    #[test]
    fn global_to_screenshot_needs_a_capture() {
        let mut t = target();
        t.target_width = 0;
        t.target_height = 0;
        assert!(t.global_to_screenshot(150.0, 100.0).is_none());
    }

    #[test]
    fn clamp_field_truncates_and_drops_blanks() {
        assert_eq!(clamp_field(Some("  ".into())), None);
        assert_eq!(clamp_field(None), None);
        assert_eq!(clamp_field(Some("Save".into())), Some("Save".into()));
        let long = "x".repeat(MAX_FIELD_CHARS + 40);
        let clamped = clamp_field(Some(long)).unwrap();
        assert_eq!(clamped.chars().count(), MAX_FIELD_CHARS + 1);
        assert!(clamped.ends_with('…'));
    }

    #[test]
    fn interesting_roles_exclude_table_cells() {
        // 同花顺实测：2349 个 AXTextField 是行情数据，可交互率仅 1%。
        // AXCell / AXStaticText / AXRow 不应被当成「控件」无条件保留。
        assert!(is_interesting_role(Some("AXButton")));
        assert!(is_interesting_role(Some("AXTextField")));
        assert!(!is_interesting_role(Some("AXCell")));
        assert!(!is_interesting_role(Some("AXRow")));
        assert!(!is_interesting_role(Some("AXStaticText")));
        assert!(!is_interesting_role(Some("AXGroup")));
        assert!(!is_interesting_role(None));
    }

    fn keep(role: Option<&str>, has_content: bool, frame: Option<Frame>, siblings: usize) -> bool {
        should_keep(KeepContext {
            role,
            has_content,
            frame_in_screenshot: frame,
            role_siblings_so_far: siblings,
            max_siblings_per_role: 8,
        })
    }

    #[test]
    fn should_keep_applies_sibling_role_threshold() {
        let frame = Frame {
            x: 1.0,
            y: 1.0,
            width: 10.0,
            height: 10.0,
        };
        assert!(keep(Some("AXButton"), true, Some(frame), 0));
        assert!(keep(Some("AXButton"), true, Some(frame), 7));
        // 全树第 9 个同类 role 起丢弃。
        assert!(!keep(Some("AXButton"), true, Some(frame), 8));
    }

    #[test]
    fn should_keep_drops_decorative_and_zero_sized() {
        let empty_frame = Frame {
            x: 0.0,
            y: 0.0,
            width: 0.0,
            height: 0.0,
        };
        let solid = Frame {
            x: 0.0,
            y: 0.0,
            width: 4.0,
            height: 4.0,
        };
        // 无任何内容
        assert!(!keep(Some("AXButton"), false, Some(solid), 0));
        // 零尺寸
        assert!(!keep(Some("AXButton"), true, Some(empty_frame), 0));
        // 非控件 role（行情表格单元格）
        assert!(!keep(Some("AXCell"), true, Some(solid), 0));
        assert!(!keep(None, true, Some(solid), 0));
    }

    #[test]
    fn role_budget_counts_per_parent() {
        let mut budget = RoleBudget::default();
        assert_eq!(budget.take(Some("AXButton")), 0);
        assert_eq!(budget.take(Some("AXButton")), 1);
        assert_eq!(budget.take(Some("AXTextField")), 0);
        assert_eq!(budget.take(Some("AXButton")), 2);
    }

    #[test]
    fn tree_id_changes_across_snapshots_and_binds_to_generation() {
        let now = std::time::SystemTime::now();
        let a = tree_id_for(7, Some("Finder"), now);
        let b = tree_id_for(8, Some("Finder"), now);
        assert_ne!(a, b, "generation 变化必须换 tree_id");
        // 同一时刻、不同窗口也必须不同。
        assert_ne!(a, tree_id_for(7, Some("Safari"), now));
        // 引用永不跨快照复用：即使所有输入相同，时刻不同也不同。
        let later = now + std::time::Duration::from_micros(1);
        assert_ne!(a, tree_id_for(7, Some("Finder"), later));
    }

    #[test]
    fn snapshot_staleness_follows_window_generation() {
        let snapshot = ElementSnapshot {
            window_id: 1,
            generation: 7,
            tree_id: 99,
            elements: Vec::new(),
            truncated: false,
            filtered_out: 0,
            visited: 0,
            root_available: true,
            elapsed_ms: 0,
        };
        let mut t = target();
        assert!(!snapshot.is_stale(&t));
        t.generation = 8;
        assert!(snapshot.is_stale(&t));
    }

    #[test]
    fn budget_defaults_match_the_measured_envelope() {
        let b = TraversalBudget::default();
        // 3000 节点 ≈ 9.3s，故 400 节点 ≈ 1.2s。
        assert_eq!(b.max_nodes, 400);
        assert_eq!(b.max_duration, std::time::Duration::from_millis(2000));
        // 同花顺的行情单元格埋在深层嵌套里。
        assert!(b.max_depth >= 10);
        assert_eq!(b.max_siblings_per_role, 8);
    }

    #[test]
    fn element_ref_renders_as_short_token() {
        assert_eq!(ElementRef(12).to_string(), "e12");
    }

    #[test]
    fn element_actions_are_send_safe_by_construction() {
        // `ElementAction` 不含任何 AX 句柄，因此工具层可以把它跨线程传递。
        // 这条不变式是 B 期的架构前提：`AXUIElement` 非 Send/Sync，一旦泄漏到
        // 工具的回调签名里，`Arc<dyn Tool>` 就无法跨线程共享（曾真实编译失败）。
        fn assert_send<T: Send>() {}
        assert_send::<ElementAction<'static>>();
        assert_send::<ElementRef>();
    }
}
