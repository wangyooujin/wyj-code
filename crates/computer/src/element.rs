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
    /// 留出余量故取 2s。
    ///
    /// **`max_depth` 从 10 调到 24（2026-10-23 实测）**：深度 10 会漏掉
    /// Web/自绘应用的绝大部分树。实测深度直方图：
    ///
    /// | App | 总节点 | 深度 ≤10 | 最大深度 |
    /// |---|---|---|---|
    /// | Chrome | 2783 | **81（3%）** | 18 |
    /// | 抖音 | 1158 | **36（3%）** | 23 |
    /// | 访达 | 240 | 240（100%） | 7 |
    /// | 同花顺 | 20001 | 20000（100%） | 7 |
    ///
    /// WebView 的 DOM 包装层每层只贡献一个 `AXGroup`，真正的控件与文本都在
    /// 13~16 层。深度 10 时 Chrome 只访问到 81 个节点、1 个文本节点；放到 24
    /// 后同样的 400 节点预算能访问 400 个节点、82 控件 + 40 文本（实测
    /// `crates/computer/examples/` 的临时探针）。24 是实测最大深度 23 的上界，
    /// 再高只会撞节点预算而不会多拿到东西。**深度本身不是成本**——耗时由
    /// 访问节点数决定（Chrome 400 节点 depth=24 为 0.032s，depth=10 为
    /// 0.013s，差值完全来自访问数 81→400）。
    fn default() -> Self {
        Self {
            max_depth: 24,
            max_nodes: 400,
            max_duration: std::time::Duration::from_millis(2000),
            max_children_per_node: 50,
            max_siblings_per_role: 8,
        }
    }
}

/// 同父节点下同类文本 role 的默认上限，仅 `include_text` 打开时生效。
///
/// 取值来自实测：Chrome 有 1151 个 `AXStaticText`，访达 56 个，抖音 240 个。
/// 8 个远不够描述一个网页；40 个在 400 节点预算内刚好能覆盖一屏可见文本
/// （实测 Chrome 40 行 ≈ 7.3KB，约 2k token）。调高到 120 会让同样的窗口
/// 输出 15KB+，而模型并不会因为多读 80 行就判断更准。
pub const DEFAULT_TEXT_QUOTA: usize = 40;

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
    /// 树内容指纹，见 [`content_fingerprint`]。
    ///
    /// 与 `tree_id` 的分工是**必须分清**的一件事：`tree_id` 含遍历时刻，
    /// 每次 `list_elements` 都不同，因此它只能证明「这个 ref 来自哪一次
    /// 观察」，证明不了「那次观察之后界面没变」。真正能检测界面漂移的是
    /// 本字段——两次遍历得到同一指纹，才说明树内容确实没动。
    pub content_fingerprint: u64,
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
    /// 是否因为 `limit` 截断（还有更多元素没返回）。
    ///
    /// 与 [`Self::truncated`] 区分：那个是**遍历预算**耗尽（连树都没走完），
    /// 这个是调用方主动限流（树走完了，只是没全给）。模型对两者的反应不同
    /// ——前者要收窄范围重来，后者可以调大 `limit`。
    pub has_more: bool,
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

/// 该元素是**纯文本节点**（只提供内容、不提供交互）。
///
/// 与 [`is_interesting_role`] 的分工：那个白名单是「这个元素能不能被操作」，
/// 本函数是「这个元素能不能被读」。二者正交——`AXLink` 两边都算，`AXButton`
/// 只算前者，`AXStaticText` 只算后者。
///
/// **为什么它必须是独立的一类**：实测（2026-10-23，本机）文本节点占
/// 各应用树的绝大部分。
///
/// | App | `AXStaticText` | 其中带文本 | 采集耗时（全量） |
/// |---|---|---|---|
/// | Chrome | 1151 | 1151 | 0.21s |
/// | 抖音 | 240 | 240 | 0.13s |
/// | 访达 | 56 | 56 | 0.06s |
/// | 同花顺 | 15 | 15 | 0.92s（表格是 3953 个 `AXTextField`） |
///
/// 也就是说网页正文、列表条目、表格单元的文字**本来就在系统里、带精确坐标**，
/// 而旧白名单一个都不给模型——模型只能靠看 1280 宽的下采样截图去认字。
///
/// `AXCell` / `AXRow` 刻意**不在**此列：它们是容器，`AXValue` 实测 100% 为空
/// （Chrome 341 个 `AXCell`、访达 67 个、同花顺 989 个 `AXRow`，带文本数全为 0），
/// 放进来只会白占配额。
pub fn is_text_role(role: Option<&str>) -> bool {
    matches!(role, Some("AXStaticText" | "AXHeading" | "AXListMarker"))
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
    /// 本次调用是否把纯文本节点也算进来（[`ElementQuery::include_text`]）。
    ///
    /// 计数配额由调用方按**独立的一份** [`RoleBudget`] 管理，不与控件共享
    /// ——否则 40 个文本节点会把 8 个按钮的名额挤掉。
    pub include_text: bool,
}

/// 元素是否应进入输出列表。
///
/// 四条规则，从实测数据反推：
/// 1. 必须有实际内容——纯装饰节点永远丢弃；
/// 2. 必须可交互或是有意义的控件 role（[`is_interesting_role`]），**或者**
///    在 `include_text` 下是有文本的纯文本节点（[`is_text_role`]）；
/// 3. 同类 role 超过阈值时只保留前 N 个（[`TraversalBudget`] 的
///    `max_siblings_per_role`）——这是同花顺 1% 可交互率的直接对策：
///    2349 个 `AXTextField` 是行情表格数据，不压掉就会淹没上下文；
/// 4. 文本节点单独一份配额（调用方传 `max_siblings_per_role` 为文本上限），
///    默认 [`DEFAULT_TEXT_QUOTA`]。
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
    let readable = context.include_text && is_text_role(context.role);
    if !is_interesting_role(context.role) && !readable {
        return false;
    }
    context.role_siblings_so_far < context.max_siblings_per_role
}

/// 生成 [`ElementSnapshot::tree_id`]。
///
/// 由窗口身份 + 根元素指纹 + 遍历起点时刻派生：同一窗口在树未变时重跑会得到
/// 不同 `tree_id`（时刻参与哈希），这正是我们想要的——**引用永不跨快照复用**，
/// 每次 `list_elements` 强制模型重新确认当前界面。
///
/// **它不能用来检测界面漂移**：时刻参与哈希意味着每次调用都不同，于是「两次
/// 观察是否看到同一棵树」这个问题在 `tree_id` 上永远没有答案。那个问题归
/// [`content_fingerprint`] 管。
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

/// 树**内容**指纹：同一窗口两次遍历结果一致时相等，界面变化时不同。
///
/// # 为什么必须有它
///
/// `tree_id` 含遍历时刻，每次 `list_elements` 都不同，所以它只能证明「这个
/// ref 来自哪一次观察」。而 ref 契约真正要防的是另一件事：**观察之后界面变了，
/// 模型却还拿着旧 ref**。此前 `tree_id` 的 `root_fingerprint` 传的是
/// `app_name`（一个与树内容无关的常量），于是这条防线形同虚设——
/// `execute_with_element` 只要序号还在范围内就照点，正是 Anthropic ref 契约
/// 明令禁止的「按序号盲点」。
///
/// 指纹只覆盖**模型看得见的东西**（过滤后的元素序列），因为「界面变了」对
/// 模型而言等价于「它看到的列表变了」。被过滤掉的表格单元格不参与，避免
/// 行情类应用每次刷新都让全部 ref 失效。
pub fn content_fingerprint(elements: &[ElementInfo], truncated: bool) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    let mut push = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(0x100000001b3);
    };
    push(&(elements.len() as u64).to_le_bytes());
    push(&[u8::from(truncated)]);
    for element in elements {
        push(&element.reference.0.to_le_bytes());
        push(&element.depth.to_le_bytes());
        for field in [&element.role, &element.title, &element.value] {
            match field {
                Some(text) => {
                    push(&[1]);
                    push(text.as_bytes());
                }
                None => push(&[0]),
            }
        }
        // 几何取整后参与哈希：亚像素抖动不该让 ref 失效，但「按钮换位置」
        // 必须让 ref 失效。
        match element.frame {
            Some(frame) => {
                push(&[1]);
                push(&(frame.x.round() as i64).to_le_bytes());
                push(&(frame.y.round() as i64).to_le_bytes());
                push(&(frame.width.round() as i64).to_le_bytes());
                push(&(frame.height.round() as i64).to_le_bytes());
            }
            None => push(&[0]),
        }
        push(&[
            u8::from(element.enabled == Some(true)),
            u8::from(element.focused == Some(true)),
            u8::from(element.selected == Some(true)),
            u8::from(element.value_settable),
        ]);
        push(element.actions.join(",").as_bytes());
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

/// 调用方对元素列表的收窄条件。
///
/// # 为什么必须有这个
///
/// `truncated: true` 曾经是死胡同：模型看到「列表不完整」，唯一能做的只有
/// 「改用坐标」或「把窗口弄小一点」——两者都不现实。真实的补救手段是**让
/// 模型自己收窄范围**：只看某个区域、只看某一类控件。计划里设计了这三个
/// 参数却一直没实现，于是大窗口（访达、同花顺）的元素列表永远是残缺的。
///
/// # 过滤发生在遍历期，不是结果期
///
/// 这一点决定了 `element_ref` 序号的含义：过滤在 `should_keep` 之前执行，
/// 序号仍然从 0 连续分配，所以「列表里的第 3 个」就是 `e3`，不会出现
/// 空洞。代价是同一窗口在不同过滤条件下 ref 编号不同——这没问题，
/// `tree_id` 每次 `list_elements` 都变，模型本来就必须重新取。
#[derive(Debug, Clone, Default)]
pub struct ElementQuery {
    /// 只保留**截图坐标空间**内与该矩形相交的元素。
    ///
    /// 用截图坐标而不是全局逻辑坐标：模型看到的就是截图，让它按自己看得见
    /// 的空间下指令，不用换算。
    pub region: Option<Frame>,
    /// 只保留这些 role。比较时忽略 `AX` 前缀与大小写，按**包含**匹配
    /// （`"button"` 命中 `AXButton` 与 `AXToolbarButton`）。
    ///
    /// 用包含而不是前缀：`AXToolbarButton` 去掉前缀后是 `ToolbarButton`，
    /// 前缀匹配会漏掉它——而模型说「按钮」时心里想的正是这两者都算。
    pub role_filter: Option<Vec<String>>,
    /// 最多返回多少个元素。达到上限即停止收集并置 `has_more`。
    pub limit: Option<usize>,
    /// 是否把纯文本节点（[`is_text_role`]）也列出来。
    ///
    /// 默认 `false`：默认输出的是「能点什么」，文本会把列表撑大且绝大多数
    /// 任务不需要。模型要读内容时显式打开——这是它唯一能拿到**精确坐标 +
    /// 原文**的通道，比看 1280 宽的下采样截图认字可靠得多。
    ///
    /// 文本节点与控件节点**各自独立配额**（见 [`DEFAULT_TEXT_QUOTA`]），
    /// 所以打开它不会挤掉可交互元素。
    pub include_text: bool,
}

/// 把 role 名归一化：去空白、去大小写敏感的 `AX` 前缀、转小写。
///
/// 必须**先转小写再去前缀**：`AXText` 与 `axtext` 是同一种写法，而
/// `strip_prefix("AX")` 对后者不生效——直接用它会让 `axtext` 过滤器变成
/// 「必须包含字面量 axtext」，永远命中不了 `AXTextField`。
fn normalize_role(raw: &str) -> String {
    let trimmed = raw.trim().to_ascii_lowercase();
    trimmed
        .strip_prefix("ax")
        .unwrap_or(&trimmed)
        .trim()
        .to_string()
}

impl ElementQuery {
    /// 该 role 是否通过 `role_filter`。无过滤时恒真。
    pub fn accepts_role(&self, role: Option<&str>) -> bool {
        let Some(filters) = &self.role_filter else {
            return true;
        };
        let Some(role) = role else {
            return false;
        };
        let role = normalize_role(role);
        filters.iter().any(|filter| {
            let filter = normalize_role(filter);
            !filter.is_empty() && role.contains(&filter)
        })
    }

    /// 该元素的截图矩形是否与 `region` 相交。无过滤时恒真。
    pub fn accepts_frame(&self, frame_in_screenshot: Option<Frame>) -> bool {
        let Some(region) = self.region else {
            return true;
        };
        let Some(frame) = frame_in_screenshot else {
            // 没有截图坐标就无从判断「在不在区域内」，保守丢弃：
            // 保留它会让模型以为区域外的东西也在区域里。
            return false;
        };
        // 相交判定与 `should_keep` 的零尺寸规则一致：退化矩形不算。
        frame.x < region.x + region.width
            && region.x < frame.x + frame.width
            && frame.y < region.y + region.height
            && region.y < frame.y + frame.height
    }
}

/// 把窗口目标翻译成一次元素快照（macOS 实现，见 `imp` 模块）。
///
/// 只读：不截图、不合成输入、不激活应用。
pub fn snapshot_elements(
    target: &WindowTarget,
    budget: &TraversalBudget,
) -> Result<ElementSnapshot> {
    snapshot_elements_with(target, budget, &ElementQuery::default())
}

/// 同 [`snapshot_elements`]，但按 [`ElementQuery`] 收窄结果。
pub fn snapshot_elements_with(
    target: &WindowTarget,
    budget: &TraversalBudget,
    query: &ElementQuery,
) -> Result<ElementSnapshot> {
    Ok(imp::snapshot_handles(target, budget, query)?.into_snapshot())
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
    /// AXShowMenu：弹出该元素的上下文菜单（右键菜单）。
    ShowMenu,
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
/// 本函数再做一次**独立复检**——序号必须仍落在当前树的元素范围内。
///
/// # `expected_fingerprint` 是这条契约的真正实现
///
/// 光校验序号**不够**：界面换了之后同一序号指向的是另一个控件，那正是盲点。
/// 调用方把 `list_elements` 当时的 [`ElementSnapshot::content_fingerprint`]
/// 传进来，本函数遍历完再算一次，不等即报 `target_changed` 要求重新列举。
/// 传 `None` 表示调用方没有观察（不该发生，保留给诊断工具）。
///
/// # `query` 必须与产生 ref 的那次 `list_elements` 完全一致
///
/// `element_ref` 的序号是「**过滤后**列表里的第 N 个」。带过滤的
/// `list_elements` 之后若不传同样的 `query`，同一个 `e3` 会指向完整树里的
/// 另一个元素——这正是 ref 契约禁止的按序号盲点。工具层把当次过滤条件
/// 存进 `ElementObservation` 并在动作时原样传回。
pub fn execute_with_element(
    target: &WindowTarget,
    budget: &TraversalBudget,
    reference: ElementRef,
    action: ElementAction<'_>,
    expected_fingerprint: Option<u64>,
    query: Option<&ElementQuery>,
) -> Result<String> {
    imp::execute_with_element(
        target,
        budget,
        reference,
        action,
        expected_fingerprint,
        query,
    )
}

#[cfg(target_os = "macos")]
use macos as imp;

#[cfg(not(target_os = "macos"))]
use unsupported as imp;

#[cfg(target_os = "macos")]
mod macos {
    use super::{
        clamp_field, content_fingerprint, tree_id_for, ElementInfo, ElementRef, ElementSnapshot,
        Frame, RoleBudget, TraversalBudget,
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
        expected_fingerprint: Option<u64>,
        query: Option<&super::ElementQuery>,
    ) -> Result<String> {
        // 动作路径必须用**与 list_elements 完全相同的过滤条件**遍历。
        //
        // 原因很硬：`element_ref` 的序号是「过滤后列表里的第 N 个」。若这里
        // 改用完整树遍历，同一个 `e3` 会指向另一个元素——正是 ref 契约禁止
        // 的「按序号盲点」，而且比界面漂移更隐蔽（指纹会因为列表长度不同而
        // 不等，反而能挡住；但如果模型恰好用不带过滤的 list 之后又用带过滤
        // 的 list，两次指纹不同也会被挡下）。
        //
        // 因此工具层必须把当次 `list_elements` 的过滤条件原样存进
        // `ElementObservation`，动作时传回来。传 `None` 表示当次没有过滤。
        let handles = snapshot_handles(
            target,
            budget,
            query.unwrap_or(&super::ElementQuery::default()),
        )?;
        if !handles.snapshot.root_available {
            anyhow::bail!(
                "target_changed: this app exposes no accessibility tree; use `coordinate` instead and do not retry list_elements"
            );
        }
        // 界面漂移检测：序号还在范围内只说明「树够大」，不说明「那个位置还是
        // 同一个控件」。指纹不等时必须拒绝——按序号继续执行正是 ref 契约
        // 明令禁止的盲点。
        if let Some(expected) = expected_fingerprint {
            anyhow::ensure!(
                handles.snapshot.content_fingerprint == expected,
                "target_changed: the window content changed since your last list_elements \
                 (fingerprint {expected} -> {}); call list_elements again instead of reusing `{reference}`",
                handles.snapshot.content_fingerprint
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
                // 元素可能在滚动区外，此时 `AXPress` 报 `ActionUnsupported`
                // ——那不是「这个控件不能按」，而是「它现在不可见」。先尝试
                // 把它滚进视野再按一次，能救回大量「列表里的第 20 项」这类
                // 明明存在却点不动的控件。
                match accessibility::press_handle(&handle) {
                    Ok(()) => Ok("clicked element with AXPress".to_string()),
                    Err(first_error) => {
                        if accessibility::scroll_to_visible_handle(&handle).unwrap_or(false) {
                            accessibility::press_handle(&handle).map_err(|second_error| {
                                anyhow!(
                                    "the element was scrolled into view with AXScrollToVisible \
                                     but AXPress still failed ({second_error}); the first attempt \
                                     failed with ({first_error})"
                                )
                            })?;
                            return Ok(
                                "scrolled the element into view with AXScrollToVisible, then \
                                 clicked it with AXPress"
                                    .to_string(),
                            );
                        }
                        Err(first_error)
                    }
                }
            }
            super::ElementAction::ShowMenu => {
                accessibility::show_menu_handle(&handle)?;
                Ok(
                    "opened the element's context menu with AXShowMenu; the menu is a separate \
                     window, so call `window_capture` action `list` to find it"
                        .to_string(),
                )
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
        query: &super::ElementQuery,
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
                        content_fingerprint: content_fingerprint(&[], false),
                        elements: Vec::new(),
                        truncated: false,
                        filtered_out: 0,
                        visited: 0,
                        root_available: false,
                        elapsed_ms: started.elapsed().as_millis() as u64,
                        has_more: false,
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
        let mut has_more = false;

        let mut queue: VecDeque<(AXUIElementRef, u16)> = VecDeque::new();
        queue.push_back((root.clone(), 0));
        // **全树共享**的同类 role 计数。
        //
        // 不能每个节点各带一份：新起一份会让阈值永远从 0 计数，等于没生效
        // （真机实测访达仍吐出 25 个 AXTextField 就是这个 bug）。
        // 共享池下，「同类 role 超过 N 个」是全树语义——对「表格单元格会把
        // 上下文淹没」这个真实问题也正是需要的语义。
        let mut siblings = RoleBudget::default();
        // 文本节点用**独立**的一份配额：两者共用一个 RoleBudget 时 40 个
        // `AXStaticText` 不会挤掉按钮（role 键不同），但把文本上限与控件上限
        // 混在同一个 `max_siblings_per_role` 里会让「按钮最多 8 个」这条规则
        // 的语义变得依赖 role 名，改一侧就影响另一侧。分开更直白。
        let mut text_siblings = RoleBudget::default();

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

            // AX 报告的元素矩形与窗口矩形毫无交集：多半是跨显示器残留或已
            // 不可见，交给模型只会产生无效点击目标。整块丢弃。
            //
            // 判据必须是**矩形相交**而不是「中心点在不在窗口内」：`global_to_screenshot`
            // 会把越界坐标夹到边界上，于是「窗口外的元素」在截图坐标里表现为
            // 紧贴边缘的一排假元素（实测 Ghostty 滚动条映射出 2853，窗口宽
            // 只有 1280）。而横跨窗口边界的大控件（整行表格）中心在窗口外、
            // 实际露着半截，用中心点判会误杀。
            let offscreen = match frame {
                Some(frame) => {
                    !target.rect_intersects_window(frame.x, frame.y, frame.width, frame.height)
                }
                None => false,
            };
            // 调用方的收窄条件。放在 `should_keep` 之前：区域/role 过滤是
            // 「这个元素是否属于本次请求的范围」，而 `should_keep` 是「这个
            // 元素是否值得给模型看」——两件事，顺序不能反。反了的话被区域
            // 排除的元素会先占掉同类 role 的配额，把区域内的元素挤掉。
            let in_scope = !offscreen
                && query.accepts_role(role.as_deref())
                && query.accepts_frame(frame_in_screenshot);
            if in_scope {
                // 文本节点从独立配额里取号，控件的阈值规则保持原样。
                let is_text = super::is_text_role(role.as_deref());
                let sibling_index = if is_text {
                    text_siblings.take(role.as_deref())
                } else {
                    siblings.take(role.as_deref())
                };
                if super::should_keep(super::KeepContext {
                    role: role.as_deref(),
                    has_content: has_content(&attrs),
                    frame_in_screenshot,
                    role_siblings_so_far: sibling_index,
                    max_siblings_per_role: if is_text {
                        super::DEFAULT_TEXT_QUOTA
                    } else {
                        budget.max_siblings_per_role
                    },
                    include_text: query.include_text,
                }) {
                    // `limit` 是调用方主动限流：达到上限就停，并置 `has_more`。
                    // 不能只是「不 push 继续遍历」——那会把整棵树走完再丢掉，
                    // 白付遍历成本。
                    if query.limit.is_some_and(|limit| elements.len() >= limit) {
                        has_more = true;
                        break;
                    }
                    handles.push(node.clone());
                    // `settable` 在这里才读：`AXUIElementIsAttributeSettable`
                    // 没有批量接口，每读一次就是一次往返。而它只对**留下来
                    // 的元素**有意义——`should_keep` 会淘汰掉绝大多数访问过的
                    // 节点（同花顺实测可交互率 1%），早读等于为筛掉的东西付钱。
                    let (settable, selected_text_settable) = accessibility::settable_flags(&node);
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
                        value_settable: settable,
                        selected_text_settable,
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
                content_fingerprint: content_fingerprint(&elements, truncated),
                elements,
                truncated,
                filtered_out,
                visited,
                root_available: true,
                elapsed_ms: started.elapsed().as_millis() as u64,
                has_more,
            },
            handles,
        })
    }
}

#[cfg(not(target_os = "macos"))]
mod unsupported {
    use super::{ElementQuery, ElementSnapshot, TraversalBudget};
    use crate::target::WindowTarget;
    use anyhow::{anyhow, Result};

    pub fn snapshot_handles(
        _target: &WindowTarget,
        _budget: &TraversalBudget,
        _query: &ElementQuery,
    ) -> Result<super::ElementHandles> {
        anyhow::bail!("AX 元素索引仅支持 macOS，当前平台不可用")
    }

    pub fn execute_with_element(
        _target: &WindowTarget,
        _budget: &TraversalBudget,
        _reference: super::ElementRef,
        _action: super::ElementAction<'_>,
        _expected_fingerprint: Option<u64>,
        _query: Option<&ElementQuery>,
    ) -> Result<String> {
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
            include_text: false,
        })
    }

    fn keep_text(
        role: Option<&str>,
        has_content: bool,
        frame: Option<Frame>,
        siblings: usize,
    ) -> bool {
        should_keep(KeepContext {
            role,
            has_content,
            frame_in_screenshot: frame,
            role_siblings_so_far: siblings,
            max_siblings_per_role: DEFAULT_TEXT_QUOTA,
            include_text: true,
        })
    }

    /// `include_text` 必须真的把静态文本放出来，且**只在**打开时放。
    ///
    /// 这条是「模型能不能读屏幕文字」的唯一开关。关掉时 `AXStaticText` 必须
    /// 仍然被拒——否则默认输出会突然膨胀（Chrome 单窗口 1151 个文本节点）。
    #[test]
    fn include_text_gates_static_text_nodes() {
        let solid = Frame {
            x: 0.0,
            y: 0.0,
            width: 40.0,
            height: 14.0,
        };
        for role in ["AXStaticText", "AXHeading", "AXListMarker"] {
            assert!(is_text_role(Some(role)), "{role} 应被识别为文本节点");
            // 关：仍然丢弃（默认输出的语义不变）。
            assert!(
                !keep(Some(role), true, Some(solid), 0),
                "{role} 关闭时不应保留"
            );
            // 开：保留。
            assert!(
                keep_text(Some(role), true, Some(solid), 0),
                "{role} 打开时应保留"
            );
            // 无内容仍丢弃——空文本节点没有任何价值。
            assert!(!keep_text(Some(role), false, Some(solid), 0));
        }
        // 容器 role 不在文本白名单里：实测 AXValue 全为空。
        assert!(!is_text_role(Some("AXCell")));
        assert!(!is_text_role(Some("AXRow")));
        // 文本配额用尽即停。
        assert!(!keep_text(
            Some("AXStaticText"),
            true,
            Some(solid),
            DEFAULT_TEXT_QUOTA
        ));
        // 控件 role 不受 `include_text` 影响。
        assert!(keep_text(Some("AXButton"), true, Some(solid), 0));
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
            content_fingerprint: 0,
            elements: Vec::new(),
            truncated: false,
            filtered_out: 0,
            visited: 0,
            root_available: true,
            elapsed_ms: 0,
            has_more: false,
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
        // 深度必须盖住 WebView 的 DOM 包装层。实测 Chrome 最大深度 18、
        // 抖音 23，深度 10 时两者都只能访问到 3% 的树。
        assert!(
            b.max_depth >= 24,
            "WebView 的 AX 树最深实测 23 层，低于它只能看到 3% 的节点"
        );
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

    fn element(reference: u32, title: &str) -> ElementInfo {
        ElementInfo {
            reference: ElementRef(reference),
            depth: 1,
            role: Some("AXButton".into()),
            title: Some(title.into()),
            description: None,
            value: None,
            enabled: Some(true),
            focused: None,
            selected: None,
            actions: vec!["AXPress".into()],
            value_settable: false,
            selected_text_settable: false,
            frame: Some(Frame {
                x: 10.0,
                y: 20.0,
                width: 30.0,
                height: 40.0,
            }),
            frame_in_screenshot: None,
        }
    }

    /// 指纹必须对**界面内容**敏感，否则「观察之后界面变了」这条最危险的漂移
    /// 无法检测，`element_ref` 就退化成按序号盲点。
    #[test]
    fn content_fingerprint_tracks_what_the_model_sees() {
        let base = vec![element(0, "Save"), element(1, "Cancel")];
        let fingerprint = content_fingerprint(&base, false);

        // 完全相同的树 → 相同指纹（这是「没有漂移」的唯一判据）。
        assert_eq!(fingerprint, content_fingerprint(&base.clone(), false));
        // 标题变了（同一个按钮换了语义）→ 必须不同。
        assert_ne!(
            fingerprint,
            content_fingerprint(&[element(0, "Delete"), element(1, "Cancel")], false)
        );
        // 元素数量变了 → 必须不同。
        assert_ne!(fingerprint, content_fingerprint(&base[..1], false));
        // 顺序变了 → 必须不同（序号指向的东西换了）。
        let mut swapped = base.clone();
        swapped.swap(0, 1);
        assert_ne!(fingerprint, content_fingerprint(&swapped, false));
        // 几何变了（按钮换位置）→ 必须不同。
        let mut moved = base.clone();
        if let Some(frame) = moved[0].frame.as_mut() {
            frame.x += 100.0;
        }
        assert_ne!(fingerprint, content_fingerprint(&moved, false));
        // 截断标记变了 → 必须不同（列表可能不完整）。
        assert_ne!(fingerprint, content_fingerprint(&base, true));
        // 亚像素抖动不该让全部 ref 失效。
        let mut jittered = base.clone();
        if let Some(frame) = jittered[0].frame.as_mut() {
            frame.x += 0.2;
        }
        assert_eq!(fingerprint, content_fingerprint(&jittered, false));
    }

    /// 指纹不能是「树未变就一定相等」的另一种写法——它必须与 `tree_id` 正交：
    /// 同一棵树重复观察 `tree_id` 不同、指纹相同。这条区分是本次修复的全部意义。
    #[test]
    fn content_fingerprint_is_stable_across_observations_unlike_tree_id() {
        let now = std::time::SystemTime::now();
        let later = now + std::time::Duration::from_millis(50);
        assert_ne!(
            tree_id_for(7, Some("Finder"), now),
            tree_id_for(7, Some("Finder"), later)
        );
        let elements = vec![element(0, "Save")];
        assert_eq!(
            content_fingerprint(&elements, false),
            content_fingerprint(&elements.clone(), false)
        );
    }

    /// role 过滤必须忽略 `AX` 前缀与大小写，并支持前缀匹配——模型写
    /// `"Button"` 时它应该同时命中 `AXButton` 与 `AXToolbarButton`。
    #[test]
    fn role_filter_ignores_the_ax_prefix_and_matches_prefixes() {
        let query = ElementQuery {
            role_filter: Some(vec!["Button".into()]),
            ..ElementQuery::default()
        };
        assert!(query.accepts_role(Some("AXButton")));
        assert!(query.accepts_role(Some("AXToolbarButton")));
        assert!(!query.accepts_role(Some("AXTextField")));
        // 无 role 的元素在过滤下不通过（它不匹配任何过滤器）。
        assert!(!query.accepts_role(None));

        // 大小写与 `AX` 前缀写法都接受。
        for raw in ["axtext", "AXText", "Text"] {
            let query = ElementQuery {
                role_filter: Some(vec![raw.into()]),
                ..ElementQuery::default()
            };
            assert!(
                query.accepts_role(Some("AXTextField")),
                "过滤器 {raw:?} 应命中 AXTextField"
            );
            assert!(
                query.accepts_role(Some("AXTextArea")),
                "过滤器 {raw:?} 应命中 AXTextArea"
            );
        }
    }

    #[test]
    fn no_role_filter_accepts_everything() {
        let query = ElementQuery::default();
        assert!(query.accepts_role(Some("AXButton")));
        assert!(query.accepts_role(None));
    }

    /// 区域过滤按**相交**判定：横跨区域边界的大控件必须保留，否则模型会
    /// 找不到明明露在区域里的东西。
    #[test]
    fn region_filter_keeps_intersecting_elements() {
        let query = ElementQuery {
            region: Some(Frame {
                x: 100.0,
                y: 100.0,
                width: 200.0,
                height: 200.0,
            }),
            ..ElementQuery::default()
        };
        let frame = |x: f64, y: f64, w: f64, h: f64| {
            Some(Frame {
                x,
                y,
                width: w,
                height: h,
            })
        };
        // 完全在内。
        assert!(query.accepts_frame(frame(150.0, 150.0, 20.0, 20.0)));
        // 横跨右边界：中心在外，但确实露着半截。
        assert!(query.accepts_frame(frame(280.0, 150.0, 100.0, 20.0)));
        // 完全在右边界外。
        assert!(!query.accepts_frame(frame(400.0, 150.0, 20.0, 20.0)));
        // 仅贴边（无面积重叠）。
        assert!(!query.accepts_frame(frame(300.0, 150.0, 20.0, 20.0)));
        // 没有截图坐标：无从判断，保守丢弃。
        assert!(!query.accepts_frame(None));
    }

    #[test]
    fn no_region_filter_accepts_everything() {
        let query = ElementQuery::default();
        assert!(query.accepts_frame(None));
        assert!(query.accepts_frame(Some(Frame {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1.0,
        })));
    }
}
