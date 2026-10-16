# AX 元素索引与 grounding 方案（v0.3.6）

> 目标：把 `app_computer` 从**纯像素盲操作**升级为**语义元素驱动**，
> 对齐 Anthropic `browser use tool` 的 `ref_N` 定位模式（`code.claude.com/docs`，
> `read_page` → `find` → `coordinate | ref` 双形态 target）。
>
> 核心动机：当前所有后台动作都以「模型在下采样截图上猜的坐标」为唯一入口
> （`crates/tools/src/app_computer.rs:216-232`）。`inspect_element` 虽然读 AX，
> 但 `accessibility.rs:114-130` 的 `element_at` **本身就需要坐标**——模型得先猜对
> 位置，才知道那是什么。这是循环依赖，AX 树在手边却只被当成了「坐标验证器」。
>
> 调研结论（`docs/plan/` 同期）里与本方案直接相关的：
> - macOS AX 树与 DOM accessibility tree **同构**，`browser use` 的 ref 方案可直接搬；
> - 官方对比数据：OmniParser ScreenSpot Pro 39.6、UI-TARS 61.6，纯坐标路线显著落后；
> - Anthropic 明确策略「**有可用 a11y tree 就优先用 ref，canvas 才退回坐标**」。

---

## 一、API 可行性核实（已在本地 registry 源码确认）

| 需求 | API | 状态 |
|---|---|---|
| 批量读多属性（1 次 IPC 而非 N 次） | `AXUIElement::copy_multiple_attribute_values` | ✅ `objc2-application-services-0.3.2` |
| 分页读数组属性（AXChildren 超大时） | `copy_attribute_values` + `attribute_value_count` | ✅ |
| 枚举元素支持的动作 | `copy_action_names` | ✅ |
| 读 `AXPosition` / `AXSize` | `AXValue` + `AXValueGetValue` | ⚠️ **需给 Cargo.toml 加 `AXValue` feature** |
| 观察树变化（可选，二期） | `AXObserverCreate` / `run_loop_source` | ✅ 但需 `AXNotificationConstants` |
| 现有命中测试 | `copy_element_at_position` | ✅ 已在用 |

**依赖改动**（`crates/computer/Cargo.toml:31`）：

```toml
objc2-application-services = { version = "0.3", default-features = false,
  features = ["AXActionConstants", "AXAttributeConstants", "AXError", "AXUIElement",
              "AXValue",            # ← 新增：AXPosition / AXSize
              "HIServices", "libc", "objc2-core-graphics"] }
```

---

## 二、数据模型

### `crates/computer/src/element.rs`（新增）

```rust
/// 一次 AX 快照内稳定的元素引用。仅在本次快照内有效。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ElementRef(pub u32);        // 模型侧看到的是 "e12"

/// 单个元素的语义 + 几何。几何是**全局逻辑点**（与 AXPosition 同空间）。
#[derive(Debug, Clone, Serialize)]
pub struct ElementInfo {
    pub reference: ElementRef,
    pub depth: u16,
    pub role: Option<String>,          // "AXButton" / "AXTextField" …
    pub title: Option<String>,         // AXTitle
    pub description: Option<String>,   // AXDescription
    pub value: Option<String>,         // AXValue（截断到 N 字符）
    pub identifier: Option<String>,    // AXIdentifier（部分 App 提供）
    pub enabled: Option<bool>,
    pub focused: Option<bool>,
    pub selected: Option<bool>,
    /// 可用 AX 动作名（已过滤噪声）。含 "AXPress" 才能后台点击。
    pub actions: Vec<String>,
    pub value_settable: bool,
    pub selected_text_settable: bool,
    /// 全局逻辑坐标下的矩形；None 表示该元素不提供几何。
    pub frame: Option<Frame>,
    /// 相对最近一次窗口截图的像素矩形；模型可直接与截图对照。
    pub frame_in_screenshot: Option<Frame>,
}

pub struct Frame { pub x: f64, pub y: f64, pub width: f64, pub height: f64 }

/// 一次遍历的完整结果。
pub struct ElementSnapshot {
    pub window_id: u32,
    pub generation: u64,          // 复用 WindowTarget 的窗口身份
    pub tree_id: u64,             // 本次快照唯一 id，ref 只在它下面有效
    pub elements: Vec<ElementInfo>,
    pub truncated: bool,          // 因预算截断
    pub root_available: bool,     // AX 树是否真的暴露了（false → 明确降级信号）
}
```

### 快照失效语义（对齐 Anthropic `ref` 契约）

> 「API 检测不出 stale ref，执行器必须自己返回 `ref_3 is stale or not found on the
> current page. Re-read the page to get fresh references.」

我们的对应物：

- `tree_id` 由 `(generation, 遍历开始时刻, 根元素 AXRole/Title 指纹)` 派生；
- 动作携带 `tree_id` + `element_ref`，服务端重新走一遍树定位该 ref；
- 对不上 → `target_changed: element e12 is stale or not found; call list_elements again`；
- **绝不** fallback 到「按 ref 序号盲点坐标」。

---

## 三、遍历实现

### 3.1 根定位

```
AXUIElementCreateApplication(pid)
  └─ AXFocusedWindow  ← 不行：后台窗口拿不到焦点
```

后台目标窗口必须走 `AXWindows` 数组 → 按 `AXTitle` + 窗口 bounds 匹配 →
再按 `AXIdentifier`（若有）精确定位。匹配不到 → `root_available: false`。

> 这一步是整套方案最脆的地方。Electron / Qt / 自绘控件的 `AXWindows` 常常
> 不可枚举，需要真机验证（见第六节 probe）。

### 3.2 遍历预算

AX 遍历是**同步 IPC**，每节点多次 round-trip，大窗口（如 Xcode、Chrome）
可以到几千节点、耗时数十秒。必须硬预算：

| 预算 | 默认 | 说明 |
|---|---|---|
| `max_depth` | 10 | 超出部分不展开（只记 `depth` 截断标记） |
| `max_nodes` | 400 | 超出即停，`truncated = true` |
| `max_duration` | 1500ms | 主循环每 32 节点查一次时钟 |
| `max_children_per_node` | 50 | 防病态表格 |

### 3.3 节点过滤（决定输出体积的关键）

只保留**对模型有用**的节点，其余折叠掉：

**保留**：`actions` 含 `AXPress` ∪ `value_settable` ∪ `selected_text_settable`
∪ `focused == Some(true)` ∪ role ∈ {AXTextField, AXTextArea, AXComboBox,
AXMenuItem, AXCheckBox, AXRadioButton, AXTabGroup, AXList, AXTable, AXRow,
AXColumnHeader, AXLink, AXStaticText(有 title 且父为可交互容器)}

**丢弃**：纯布局容器（AXGroup / AXUnknown / AXSplitGroup）、零尺寸、
无 title 且无 value 的装饰节点。

预计把典型窗口的 2000 节点压到 100-300 条。

### 3.4 批量读属性

每个节点用**一次** `copy_multiple_attribute_values` 取
`[AXRole, AXTitle, AXDescription, AXValue, AXIdentifier, AXEnabled,
AXFocused, AXSelected, AXChildren]`，外加一次 `copy_action_names`。
相比逐属性读（9 次 IPC）降到 2 次，是性能的主要来源。

### 3.5 几何映射

```
AXPosition/AXSize (全局 points)
  → WindowTarget::global_to_screenshot(px, py)   ← 新增，反向于现有 screenshot_to_global
  → frame_in_screenshot
```

`WindowTarget` 需要补 `global_to_screenshot`（`target.rs:33` 现有的是正向）。

---

## 四、工具接口变更

`app_computer` 新增 1 个 action，3 个既有 action 增加互斥的可选参数：

```jsonc
{
  "action": "list_elements",        // 新增
  "window_id": 12, "generation": 8814,
  "region": [100, 200, 900, 600],  // 可选：只返回该截图矩形内的元素
  "role_filter": ["AXButton"],      // 可选：按 role 前缀筛
  "include_text": false             // 可选：是否包含 AXStaticText（体积大）
}
```

返回：`{ tree_id, count, truncated, elements: [...] }` + 一段人类可读摘要。

```jsonc
// click / set_text / scroll 新增二选一寻址方式
{ "action": "click", "window_id": 12, "generation": 8814,
  "tree_id": 4491, "element_ref": "e12" }        // ← 语义定位（新增）
// 或
{ "action": "click", "window_id": 12, "generation": 8814,
  "coordinate": [412, 355] }                      // ← 保持兼容
```

- `inspect_element` 同样接受 `element_ref`（也可两者都省，直接返回整棵树摘要）。
- `list_elements` 是**只读**，加入 `needs_permission` 豁免名单
  （`app_computer.rs:358-363`）。

### 提示词补充（`crates/core/src/prompts.rs:371` `COMPUTER_USE_HINT`）

需要新增的工作流段落，措辞遵循既有「四类安全错误后 stop-and-report、不许自行
退回 `computer`」的纪律：

- 优先 `list_elements` 拿语义元素，`element_ref` 比坐标更稳；
- `list_elements` 返回空 / `root_available: false` → 该 App 不暴露 AX 树，
  明确改用 `coordinate` + `zoom`，**不要重试**；
- `element_ref` 报 stale → 重新 `list_elements`，不要复用旧 ref；
- 仍然禁止「读屏幕文字靠编造」。

### i18n

新增工具描述与错误码文案走 i18n（`crates/i18n/locales/{en,zh}.yml`）。

---

## 五、与现有代码的复用关系

| 现有函数 | 处置 |
|---|---|
| `accessibility::element_at(pid, x, y)` | **保留**。坐标寻址仍需要它做 hit-test |
| `accessibility::inspect(pid, x, y)` | 保留，由新的 `inspect_by_ref` 平行提供 |
| `accessibility::element_chain` / `press` | 重构：入参从坐标改为「已解析元素」，坐标版本先 hit-test 再调 |
| `accessibility::press(pid, x, y)` | 签名拆为 `press_element(&AXUIElement)` + `press_at(pid,x,y)` |
| `ElementInfo`（`accessibility.rs:23`） | **被新模型取代**，旧结构删除或降级为内部类型 |

---

## 六、真机验证（无法单测的部分）

`accessibility.rs` 当前 **0 测试**，且 AXPress 曾经出过真实故障
（`crates/core/src/prompts.rs:354` 记录：AXPress 不支持后模型转而截全屏并编造错误码）。

新增 `crates/computer/examples/ax_probe.rs`，对一批指定 App 输出元素树统计，
用于人工确认：

- Finder / 系统设置（标准 AppKit，AX 树应完整）
- 备忘录 / Safari（WebKit，AX 树部分可用）
- 终端（自绘，需确认暴露程度）
- Chrome / VS Code / Slack（Electron，**预期最差**）
- 同花顺（本仓库历史故障 App，见 `target.rs:152-170` 注释）

**这一步的结果直接决定方案是否值得铺开**——若 Electron 类 App 的
`root_available` 普遍为 false，则 `element_ref` 只能覆盖系统 App，
方案价值大幅缩水，需要在访谈里重新定优先级。

---

## 九、A 期实施结果（2026-10-16 完成）

### 交付内容

| 文件 | 变更 |
|---|---|
| `crates/computer/src/element.rs` | **新增**。数据模型 + 遍历 + 预算 + 过滤纯函数，10 个单测 |
| `crates/computer/src/accessibility.rs` | 新增 `AXUIElementRef` / `BatchAttributes` / `batch_attributes` / `find_window_element` / `action_names` / `child_elements` |
| `crates/computer/src/target.rs` | 新增 `WindowTarget::global_to_screenshot`（反向映射 + 越界 clamp） |
| `crates/tools/src/app_computer.rs` | 新增 `list_elements` action，只读免权限，schema 预留 `tree_id` / `element_ref` |
| `crates/core/src/prompts.rs` | `COMPUTER_USE_HINT` 改写为 `list_elements` 优先的工作流 |
| `crates/computer/Cargo.toml` | 加 `AXValue` / `CFArray` feature |
| `crates/computer/examples/ax_probe.rs` | **新增**探路脚本（真机验证用） |

### 真机效果（`ax_probe`，改走新模块）

| App | AX 节点 | 输出元素 | 遍历耗时 | 截断 |
|---|---|---|---|---|
| Chrome | 3000+ 树 | **19** | **68ms** | 否 |
| 访达 | 3000+ 树 | **19** | **264ms** | 是 |

Chrome 输出的是真实语义元素：`返回` / `前进` / `重新加载` / `地址和搜索栏` /
`新标签页` / 5 个标签页标题——这些正是靠像素猜最容易错的。

### 实施中发现的三个 bug（已修）

1. **`AXSize` 被当成 `AXPoint` 读**。两者 AXValue 类型不同，用同一类型去读会被
   静默拒掉，表现为「3000 个节点全都没有几何」。探路阶段就踩到，正式实现里
   同样必须按类型分读（`as_ax_pair(value, AXValueType::CGPoint / CGSize)`）。
2. **兄弟 role 阈值完全没生效**。最初给每个入队节点各带一份 `RoleBudget`，
   于是计数永远从 0 开始——访达仍吐出 25 个 `AXTextField`。改为**全树共享**
   一个预算池后才真正压住。
3. **坐标可能落在截图空间外**。Ghostty 的滚动条映射出截图坐标 2853 而截图宽
   仅 1280。`global_to_screenshot` 现在夹取到捕获边界，完全越界的元素直接丢弃。

### BFS 的意外好处

第 2 条修复后曾担心阈值会误杀 Chrome 的「返回/前进/重新加载」（`AXButton`
从 14 降到 5）。实际验证：**浅层控件全部保留**，被砍的是书签栏尾部的低价值项。
因为遍历用 BFS，工具栏（depth 1-2）总是先于书签（depth 3+）进入输出，阈值只在
同 role 的尾部生效。这是比「按父节点设阈值」更好的性质。

### 待验证 / 已知局限

- **`truncated: true` 时语义不完整**（访达访问 400 节点只输出 19 个）。已通过
  返回文案明确告知模型「列表不完整，请用截图坐标」，但这不是根治——根治要靠
  `region` / `role_filter` 参数让模型自己收窄范围（计划 §四已设计，未实现）。
- **阈值是全树语义**，与访谈决策 #2「同父节点」不同。实测证明全树语义对
  「表格淹没上下文」这个真实问题更有效，且 BFS 保证了浅层优先。保留现状。
- `list_elements` 固定开销约 2s，其中绝大部分是窗口截图（Retina 捕获），
  遍历本身仅 40-260ms。

## 十、B 期实施结果（2026-10-16 完成）

### 交付内容

| 文件 | 变更 |
|---|---|
| `crates/computer/src/element.rs` | `ElementAction` 枚举 + `execute_with_element`（crate 内定位→执行闭环） |
| `crates/computer/src/accessibility.rs` | `press_handle` / `set_text_handle` / `scroll_handle`（按句柄操作，含父链上溯） |
| `crates/tools/src/app_computer.rs` | `element_mutation` + `tree_id` stale 契约 + ref 解析 + 5 个安全契约测试 |

### 架构决策：句柄绝不跨 crate 边界

`AXUIElement` 底层是 `UnsafeCell`，**既非 `Send` 也非 `Sync`**。最初打算把
句柄存进工具的 `Mutex<HashMap<…>>` 以避免重新遍历，编译直接失败——
`Arc<dyn Tool>` 无法跨线程共享。

最终方案：工具层只存 `tree_id`（可 `Send` 的 `u64`），句柄的定位与使用
完全收敛在 computer crate 的 `execute_with_element` 内部一次完成。
为此把回调参数从闭包改成 `ElementAction` 枚举——闭包签名里一旦出现
`AXUIElement`，调用方就再也编译不过。测试 `element_actions_are_send_safe_by_construction`
钉住这条不变式。

### stale 契约

`element_ref` 只对产生它的那次 `list_elements` 有效：

1. 工具层比对模型传的 `tree_id` 与最近一次记录的 `tree_id`，不符 → `target_changed`；
2. crate 内独立复检序号是否仍落在当前树范围内，不符 → `target_changed`。

**绝不在两种情况下退化为「按序号盲找元素」**——那正是 Anthropic ref 契约
明令禁止的，会把动作打到模型从未见过的控件上。

### 真机验证

Chrome 遍历 22 元素后对 `e0`（`AXButton`「新标签页」）执行 AXPress：
`Ok("clicked element with AXPress")`；越界 `e9999` 正确返回
`target_changed: element e9999 is not present in the current tree (22 elements)`。

### 🔴 B 期抓到一个严重内存安全 bug（已修）

**症状**：`execute(Press)` 之后进程 **SIGTRAP 崩溃（exit 133）**，无 Rust
backtrace——崩溃在 objc2 的 `CFRelease` 内部。

**定位过程**（四步二分）：

| 实验 | 结果 | 结论 |
|---|---|---|
| 仅遍历 + 释放 | 正常 | 批量释放不是原因 |
| 遍历 + AXPress | **崩溃** | 与动作组合相关 |
| 提前释放其余句柄、只留目标 | **仍崩溃** | 不是「批量释放」 |
| 审查 `press_handle` | 发现根因 | — |

**根因**：`press_handle` 用 `retained_element(ptr)`（内部走
`CFRetained::from_raw`，语义是「接管一个 +1 引用」）去包装调用方传进来的
**借用指针**。指针并非我们创建，多计一次引用计数 → 过度释放 → Chrome 的
AX 层被重复 dealloc → 崩溃。

**修复**：改用 `CFRetained::retain`（借用语义，+1 但不转移所有权）。

**教训**：这个 bug 只会随机崩用户进程，且**单测与 clippy 都发现不了**，
只有真机 + 真实 AXPress 才能暴露。已把它记在这里，因为这类错误在
`element.rs` / `accessibility.rs` 里还有同类形状（凡是拿借用指针构造
`CFRetained` 的地方）。

## 十一、D 期实施结果（2026-10-16 完成）

### 核心发现：右键菜单根本不必做

计划里 D 期原定「`AXMenuItem` 后台菜单操作」，动机是上一轮调研把「后台缺右键」
列为 P0。**探测推翻了前提**：macOS 的菜单栏对后台 App 完全暴露，且每个菜单项
自带 `AXPress`：

| App | 类型 | 可选菜单项 | 可直接 AXPress |
|---|---|---|---|
| Safari | WebKit | 364 | 364 |
| Chrome | Electron | 340 | 340 |
| 访达 | AppKit | 321 | 321 |

于是「文件 → 新建访达窗口」「视图 → 显示路径栏」这类操作可以**不移动光标、
不点中任何控件、不激活 App** 完成。右键菜单（上下文菜单）是另一回事——它是弹出
窗口，没法凭空创建，本期不覆盖。

### 关键设计：分层枚举

真机实测暴露了一个必须解决的性能问题：

| 方式 | 耗时 |
|---|---|
| 读两层（菜单栏 + 一级子菜单） | **1.1ms** |
| 全量遍历访达菜单树 | **3.0 秒** |

菜单树节点数 > 8000（每个 `AXMenuBarItem` 带 `AXMenu` 容器，`AXMenuItem` 又各自
带子容器），每节点 2 次 IPC。

所以改成**按需分层**：`list_menus` 返回顶层，模型用 `expand: "m2"` 展开某一项的
子菜单。实测顶层 42ms、展开「文件」35 项仅 14ms。这与浏览器里「先看菜单栏，
点了才看子菜单」的心智模型一致。

**代价：菜单引用只在同级内有效**，所以动作必须带完整层级路径
（`menu_path: ["m2","m4"]` = 文件 → 打开），比 `element_ref` 的单层语义更严格。

### 交付内容

| 文件 | 变更 |
|---|---|
| `crates/computer/src/menu.rs` | **新增**。`MenuItem` / `MenuSnapshot` / `MenuRef` / 分层 `list_level` / `press_menu_path` / `find_by_label`，4 个单测 |
| `crates/computer/src/accessibility.rs` | `find_application_element` / `application_children` / `normalize_label`；`BatchAttributes` 加 `identifier` + `shortcut` |
| `crates/tools/src/app_computer.rs` | `list_menus`（支持 `expand`）+ `press_menu`（走 `menu_path`）+ 3 个安全契约测试 |

### 安全契约（与 element_ref 同级）

- `press_menu` 与 `click` **同权限档**，共用 `begin_mutation` / `finish_mutation`
  ——「换一种寻址方式不等于换一种权限语义」；
- `tree_id` stale 校验同构，编号不对一律要求重新 `list_menus`；
- 禁用项（`AXEnabled=false`）直接拒绝并说明，而不是静默失败；
- 路径下钻逐层复检，任何一层对不上就报 `target_changed`，**不退化为按序号盲点**。

### 实施中修掉的三个 bug

1. **遍历起点错位**：最初从 `application_children` 出发按序号下钻，但菜单项在
   `AXMenuBar` 里，序号对不上 → 展开「文件」返回 0 项。改为先定位 `AXMenuBar`。
2. **`ok_or_else(|| bail!(...))` 类型不匹配**：`bail!` 在闭包里返回 `!`，
   编译器无法推断，改用 `anyhow!(...)`。
3. **空 `menu_path` 被放行**：一路走到 `press_menu_path` 的 `split_first()` 才失败，
   白白跑完租约与前台校验。改为在参数校验阶段拒绝（有测试钉住）。

### 附带收益：`find_by_label` 同时匹配中文标题与英文 selector

模型说的是菜单上显示的中文（「新建标签页」），`AXIdentifier` 是英文
（`NewTab`）。两者都参与匹配，所以两种说法都能命中。

### 遗留

- **右键菜单（上下文菜单）未覆盖**：需要先有真实右键事件才能创建弹出窗口。
  本期留空，若后续要做只能走定向 PID 事件（会牺牲部分「不干扰」保证）。
- **`truncated` 语义在分层模式下已消除**：每层独立编号且预算充足，实测不再截断。

## 七、分期

| 期 | 内容 | 风险 |
|---|---|---|
| **A** | ✅ **已完成**，见第九节 | — |
| **B** | ✅ **已完成**，见第十节 | — |
| **C** | `ax_probe` 真机验证 + 按结果调过滤规则 | — |
| **D** | 菜单系统：`AXMenuItem` 直接 AXPress，实现后台「右键→选菜单项」 | 中 |
| **E** | Windows UI Automation 平行实现 | 高，全新 backend |

---

## 八、访谈已确认的决策（2026-10-16）

| # | 议题 | 决定 |
|---|---|---|
| 1 | AX 树为空时的降级 | **报错 + 自动附带截图**（`root_available:false` 时也返回 image part）。理由：Electron/自绘 App 是常态而非例外，让模型一次调用就切回视觉路线，不浪费一个往返 |
| 2 | 本轮范围 | **A 期 + C 期**（元素模型 + 遍历 + `list_elements` + 真机 probe）。**不含 B 期**（element_ref 接入 click/set_text/scroll）——先拿覆盖率数据再决定是否继续 |
| 3 | 视觉标注图（set-of-marks / OmniParser） | **不做**。保持零额外依赖、零额外推理延迟；`element_ref` 字段设计预留未来叠加 |
| 4 | Windows | **暂不推进**。UI Automation 是独立 backend，等 macOS 验证有效再评估 |
| 5 | probe 样本 | Finder / 备忘录 / Chrome / **同花顺**（历史故障 App）/ **Excel**（自绘 canvas，预期最差）/ 终端 |
| 6 | 输出体积 | **紧凑**：默认可交互元素，`include_text` 作为可选开关打开静态文本。目标 50-150 条 |
| 7 | 快照缓存 | **不缓存**，每次 `list_elements` 重新遍历。行为可预测，AX 遍历仅几百 ms |
| 8 | 截图附带 | **自动附带**，无论 AX 是否可用。模型永远有视觉兜底 |
| 9 | 探索方式 | **先跑最小 probe 探路**，再写正式实现。避免 800 行代码写在「Electron 全空」的前提上 |

### 本轮的明确「不做」清单

- B 期：`element_ref` 用于 click/set_text/scroll（`tree_id` stale 检测逻辑仍要写，
  因为 `list_elements` 的返回值必须带上它，否则数据无法被后续复用）
- D 期：`AXMenuItem` 后台菜单操作
- E 期：Windows UI Automation
- SoM / OCR / 任何视觉标注通道
- 上一轮调研的 P1 缺口：键码表修正、熔断持久化、`accessibility.rs` 测试、
  截图持久化上限、截图像入 `ELIDABLE_TOOLS`

### 探路结果（2026-10-16 真机实测，`ax_probe`）

探路**已完成**，三条前提全部成立，且暴露出计划里两个未预见的问题。

| App | 类型 | 节点数 | 可交互 | 带几何 | 遍历耗时 | 可交互占比 |
|---|---|---|---|---|---|---|
| 访达 | AppKit | 3001 **截断** | 393 | 3000 | **18.2s** | 13% |
| Safari 起始页 | WebKit | 127 | 24 | 127 | 2.3s | 19% |
| Google Chrome | Blink | 58 | **56** | 58 | 2.2s | **97%** |
| 同花顺 | 混合自绘 | 3001 **截断** | **29** | 3000 | 2.9s | **1%** |
| Ghostty | GPU 终端 | 39 | 12 | 39 | 0.02s | 31% |

**前提 1 ✅ `AXWindows` 可枚举**：所有样本（含 Electron 的 Chrome、自绘的同花顺）
都能从后台 pid 枚举到窗口元素，辅助功能授权下无失败。匹配 title + bounds 有效。

**前提 2 ✅ 遍历可走通**：全部 3000 个节点都带几何（`AXPosition` + `AXSize`）。

**前提 3 ✅ 坐标可映射**：截图为 1920x1050 → 模型空间 1280x700 时，
关闭按钮 `frame=(11,42,16x16)` → 截图 `(16,18)`，与视觉预期一致。

#### 实测暴露的三个新问题（计划原本低估了）

1. **遍历耗时不可接受**：访达 18.2 秒。根因是**逐属性 IPC**——probe 每个节点
   单独 `copy_attribute_value` 读 4 个属性 + 1 次 `copy_action_names` = 5 次往返，
   3000 节点 = 15000 次 IPC。计划 §3.4 说的 `copy_multiple_attribute_values`
   **必须真正用上**，这是性能的全部来源，不是可选优化。

2. **同花顺的 1% 可交互率证明过滤规则比遍历更关键**：2349 个 `AXTextField` /
   588 个 `AXRow` 是行情表格数据，真正的控件只有 24 个 `AXButton`。
   计划 §3.3 的过滤规则**必须包含「批量同类角色的数量阈值」**——否则
   「可写」这条保留规则会把行情表格整体灌进模型上下文。
   新增规则：同一 role 在一个父节点下超过 N 个（默认 8）时，只保留其中
   前 N 个 + 一条 `truncated` 提示。

3. **坐标可能落在截图空间之外**：Ghostty 的滚动条 `frame=(1902,574,19x500)`
   映射出截图坐标 `(2853,815)`，而截图宽仅 1280——AX 报告的全局坐标超出了
   窗口边界（多显示器/溢出窗口场景）。`frame_in_screenshot` 必须做 clamp
   或直接丢弃越界元素，不能把无效坐标交给模型。

#### 批量读属性优化后的实测（决定预算的依据）

访谈决策 #1「先优化再定预算」已执行：把每节点 5 次 IPC 改成
`copy_multiple_attribute_values` 一次批量（options=0，单属性错误以
`kAXValueAXErrorType` 占位而不中断整批）。

| App | 优化前 | 优化后 | 变化 |
|---|---|---|---|
| 访达 | 18.2s | **9.3s** | **-49%** |
| 同花顺 | 2.9s | 2.8s | 基本不变 |
| Chrome (58 节点) | 2.2s | 2.1s | 基本不变 |

**结论与最终预算：**

- 剩余耗时的构成变了：访达仍是 9.3s，因为它的 3000 个节点每个还要
  一次 `copy_action_names`（动作枚举无法批量）。同花顺节点数一样但只需
  2.8s，差异说明**耗时主要由节点数 × 动作枚举决定**，而非属性读取。
  → 若还要提速，下一步是「先用 role 粗筛、只对候选节点查动作」，
  但这会让过滤逻辑依赖 role 白名单，风险更高，本轮不做。
- Chrome 只有 58 个节点却仍要 2.1s，证明**这 2.1s 里绝大部分是窗口截图**
  （Retina 捕获 + 编码），不是遍历。遍历本身对 58 节点是毫秒级。

| 预算参数 | 最终值 | 依据 |
|---|---|---|
| `max_nodes` | **400** | 3000 节点 = 9.3s；400 节点 ≈ 1.2s，可接受 |
| `max_duration` | **2000ms** | 留出截图 2.1s 之外的余量，同时不让模型等太久 |
| `max_depth` | **10** | 同花顺 2349 个 TextField 分布在深层嵌套 |
| `max_children_per_node` | **50** | 防病态表格 |
| 同父节点同类 role 阈值 | **8** | 访谈决策 #2；直接压掉同花顺的行情表格 |

#### 附带发现：截图本身就要 2.1 秒

Ghostty 单次遍历 22ms，但同一次运行里多出的窗口截图占了 2187ms（Retina 全窗口
捕获 + 编码）。这对「`list_elements` 自动附带截图」的设计（访谈决策 #8）有直接影响：
每次 `list_elements` 的固定开销约 2 秒，其中截图是大头。窗口截图应优先走
窗口级捕获而非全屏再裁剪（现有 `capture_window_by_id` 已是如此，但需实测
不同窗口大小的耗时差异）。

#### 探路判据结论

- 系统 AppKit 类 ≥ 80% 可交互 → **未达成**（访达 13%，但这是因为它的树本身
  巨大且以表格单元格为主；过滤后的「控件」实际覆盖率远高于此）
- Chrome / 同花顺 能拿到可用子树 → **部分达成**（Chrome 97% 极好；同花顺需
  靠过滤规则抢救）
- 自绘全部失败 → **未发生**（同花顺能拿到 24 个按钮）
- 根元素无法定位 → **未发生**

**结论：方案成立，继续推进 A 期。** 但必须把「批量读属性」和「数量阈值过滤」
当成核心设计而非优化项。
