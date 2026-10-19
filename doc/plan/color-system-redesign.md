# TUI 配色系统与 Markdown 渲染样式重构

> 状态：已实施
> 设计基准背景：`#1E2127`（暗色终端）
> 相关代码：`crates/tui/src/theme.rs`、`crates/tui/src/markdown.rs`、`crates/tui/src/render.rs`

## 1. 为什么要改

配色架构本身没有问题：`theme.rs` 是全仓唯一允许出现颜色字面量的文件，27 个槽位按用途命名，
插件主题可整体覆盖，`NO_COLOR` 与非 truecolor 终端有统一降级路径。问题出在**取值**上。

### 1.1 两套色彩体系混用

11 个槽位是固定 RGB（`success` / `error` / `warning` / `claude` / `inactive` / `suggestion` /
`text` / `text_dim` / `border` / `code_fg` / `selected_bg`），另外 12 个是 ANSI 具名色
（`plan=Blue`、`danger=Red`、`hint`/`info`/`queue=Cyan`、`muted=DarkGray`、
代码高亮四件套 `Cyan`/`Green`/`Yellow`/`DarkGray`）。

具名色的真实 RGB 由**用户终端主题**决定，所以同一屏里「终端配的青」与「写死的橙」色温对不上；
非 truecolor 降级也只对 RGB 生效，具名色永远保持原样。结果是换一台机器、换一套终端主题，
界面观感就变一次，而插件主题作者无法预期。

### 1.2 色相过多、亮度普遍偏高

白、橙、蓝紫、浅蓝、青×3、绿、黄、红、粉红、琥珀、灰×3 —— 约 12 个色相，且多数是高亮色。
最典型的是**工具结果正文用 `#B1B9F9` 蓝紫**（`Theme::tool_result()`），而工具输出恰恰是屏幕上
出现频率最高的长文本，比正文更彩、更亮，读久了眼睛累。

### 1.3 层级倒置

Markdown h1 用品牌橙（感知亮度 ≈ 0.35），h2 用纯白（≈ 1.0），h3 用 `#999`（≈ 0.32）。
视觉上 **h2 比 h1 更重**，标题层级是反的。

### 1.4 品牌橙面积过大

assistant 前缀、工具调用行、h1、列表 bullet、选中行前景、光标底色、状态栏 `◆`、
所有管理面板标题 —— 整屏偏暖。品牌色的价值来自「稀有」，铺开之后既失去强调作用，也带来阅读疲劳。

### 1.5 三档灰在暗底上分不开

`inactive #999999` / `muted DarkGray` / `border #666666`。其中 `muted` 在不同终端可能是 `#555`
也可能是 `#808080`，而它承担行号、Todo 圆圈、URL 注释这类需要「明显更弱」的角色。

### 1.6 代码高亮是 VGA 配色

keyword 青 + BOLD、string 绿、number 黄、comment 深灰。饱和度和亮度都高于正文，
代码块比它承载的内容更抢眼。

### 1.7 对比度兜底实际失效

`selection_fg = Color::Black`，而 `relative_luminance()` 对具名色返回中性值 0.5
（无从得知终端会画成什么），于是 `ensure_contrast()` 对「黑 vs 白」的判断建立在错误前提上。

## 2. 设计依据

参考对象：glamour / glow 的 dark 主题、bat 与 delta 的语法主题、One Dark、Tokyo Night、
Catppuccin、GitHub Dark。共同做法：

1. **正文不用纯白**（`#d0d0d0`~`#d8d8d8`），把亮度预算留给强调色。
2. **层级靠明度**，四档中性色 + 一档边框足够；靠增加色相表达层级会让界面变成调色盘。
3. **品牌色只出现在小面积**：品牌标识、光标、当前选中。正文、工具输出、代码一律不染品牌色。
4. **语义色成对、同明度**（如 `#98C379` / `#E06C75`），不用纯 `Green` / `Red`。
5. **色盲安全**：diff 保留 `+`/`-`，状态保留 `✓`/`✗`，颜色只是加强而非唯一手段。
6. **结构与边框区分代码块与表格**，不靠高饱和前景。
7. **语法高亮用低饱和色板**，正文色统一为 `#ABB2BF`，只让四类 token 轻微偏离。

本方案选择**冷灰中性底 + 暖橙品牌点缀**（One Dark / Tokyo Night 路线），只做暗色，
不做背景探测 —— 与现有架构一致。

## 3. 色板

### 3.1 中性色阶（四档 + 边框 + 两个背景辅助）

| 槽位 | 旧值 | 新值 | 用途 | 对比度（对 `#1E2127`） |
|---|---|---|---|---|
| `text` | `#FFFFFF` | `#D7DAE0` | 正文、输入框、h2、表头 | 11.5:1 |
| `text_dim` | `#B4C8FF` | `#A0A8B8` | 次级正文、工具结果、h3–h6、引用正文 | 7.0:1 |
| `inactive` | `#999999` | `#7A8397` | 主 dim、状态栏、thinking、未闭合围栏 | 4.2:1 |
| `muted` | `DarkGray` | `#5A6274` | 行号、Todo 圆圈、URL 注释、极弱标记 | 2.7:1 |
| `border` | `#666666` | `#39404F` | 框线、表格网格、引用竖线、分隔线 | 1.6:1（非文本） |
| `selected_bg` | `#424242` | `#2A3140` | 选中行背景 | — |
| `selection_fg` | `Black` | `#16191F` | 压在已知亮底上的前景 | — |
| `cursor` | `White` | `#1A1D23` | 块状光标字符（底色恒为品牌橙） | — |

四档中性色的间距刻意拉开：`text → text_dim` 约 1.6 倍亮度差，`text_dim → inactive` 约 1.7 倍，
`inactive → muted` 约 1.5 倍。这样「正文 / 次级 / 弱化 / 极弱」在任意暗色终端上都能一眼分层。

### 3.2 语义色

| 槽位 | 旧值 | 新值 | 用途 |
|---|---|---|---|
| `claude` | `#D77757` | `#D98E5F` | 品牌橙：assistant 前缀、工具调用行、h1、选中行前景、光标底、状态栏 `◆` |
| `suggestion` | `#B1B9F9` | `#A9B8F0` | 用户前缀 `❯`、权限对话框（**不再用于工具结果**） |
| `success` | `#4EBA65` | `#98C379` | 成功 `✓`、diff 新增、`!` bash 成功 |
| `error` | `#FF6B80` | `#E06C75` | 失败 `✗`、diff 删除、错误正文 |
| `warning` | `#C89B1E` | `#E5C07B` | 警告 `⚠`、Ctrl+C 待确认 |
| `danger` | `Red` | `#E5484D` | bypass 三处（状态栏芯片 / 标题栏 / 输入框边框）、破坏性确认 |
| `hint` | `Cyan` | `#56B6C2` | `@file` 引用、补全面板、超链接下划线 |
| `info` | `Cyan` | `#61AFEF` | System `⚙`、thinking 头、运行中任务、脚注 |
| `queue` | `Cyan` | `#56B6C2` | 状态栏排队 `●`（与 `hint` 同色，同属输入侧提示） |
| `plan` | `Blue` | `#C792EA` | `[plan]` 芯片、计划块、plan 模式边框 |

`danger` 与 `error` 是两个不同语义、两种不同红：`error` 是「这件事失败了」，
`danger` 是「你正在关闭安全网」。前者随内容出现，后者是常驻模式标识，必须更沉、更硬。

### 3.3 语法高亮

| 槽位 | 旧值 | 新值 |
|---|---|---|
| `code_fg` | `#93A5FF` | `#ABB2BF` |
| `code_keyword` | `Cyan` + BOLD | `#C678DD` + BOLD |
| `code_string` | `Green` | `#98C379` |
| `code_number` | `Yellow` | `#D19A66` |
| `code_comment` | `DarkGray` + ITALIC | `#6B7280` + ITALIC |
| `inline_code`（**新增**） | 复用 `code_fg` | `#E5C07B` |

行内代码单独开槽位的理由：代码块正文是「整块代码」，需要低饱和以保持可读；
行内代码是「正文里一个标识符」，需要轻微跳出来但**不能有背景色**（仓库刻意不画卡片）。
暖琥珀 `#E5C07B` 与冷灰正文形成足够区分，又不会像旧浅蓝那样和用户前缀、权限对话框撞色。

### 3.4 欢迎页

| 槽位 | 旧值 | 新值 |
|---|---|---|
| `welcome_logo_gradient_start` | `#D77757` | `#D98E5F` |
| `welcome_logo_gradient_mid` | `#E1A055` | `#E0A46B` |
| `welcome_logo_gradient_end` | `#F0C850` | `#E8C07A` |
| `welcome_tip` | `#E5C07B` | `#D9B36C` |

logo 渐变整体降一档饱和度，与新的品牌橙对齐，不再比正文亮出一大截。

## 4. Markdown 渲染规则

| 元素 | 旧规则 | 新规则 | 理由 |
|---|---|---|---|
| 正文 | `Style::default()`（透传终端白） | `Theme::body()`（`text`） | 显式冷灰，不再随终端前景色漂移 |
| h1 | `claude` + BOLD，前缀 `█ ` | 不变 | `█` 实心块表达最高层级 |
| h2 | `text` + BOLD，前缀 `▌ ` | 不变 | |
| h3–h6 | `inactive` + BOLD，前缀 `▎ ` | `text_dim` + BOLD | 旧值 `#999` 在暗底上过暗，与正文差距过大 |
| 粗体 / 斜体 / 删除线 | 仅加 Modifier | 不变 | 不换色是正确的 |
| 行内代码 | `code_fg`（浅蓝） | `inline_code`（琥珀），仍带反引号 | 与代码块解耦，且不与用户前缀撞色 |
| 代码块框线 | `border` | 不变 | |
| 代码块正文 | `code_fg` | `#ABB2BF` | 低饱和 |
| 行号 | `muted` | 不变 | |
| 列表 bullet | `assistant_prefix()`（橙 + BOLD） | `text_dim`，不加 BOLD | 每行一个橙点会让整段列表发光 |
| 任务列表 `☑`/`☐` | 同 bullet | 同上 | |
| 引用竖线 | `dim()` | `border` | 结构线用结构色 |
| 引用正文 | `inactive` | `text_dim` | 引用是「另一种正文」，不是「更弱的正文」 |
| 表格网格 | `border` | 不变 | |
| 表头 | `Style::default()` + BOLD | `text` + BOLD | 与正文同色，靠 BOLD 区分 |
| 单元格 | `Style::default()` | `text` | |
| 分隔线 | `dim()` | `border` | 同「结构线用结构色」 |
| 链接文字 | 无样式，`hyperlink.rs` 后处理 | 不变（`hint` + UNDERLINED） | |
| 链接 URL 注释 | `dim()` | `muted` | URL 是附注，比正文弱一档 |
| 脚注引用 / 定义标签 | `info` | 不变 | |
| thinking 整块 | `dim()` | 不变（`inactive` 新值） | thinking 是旁白 |
| thinking 代码围栏 | `border` 竖线 + `dim` 正文 | 不变 | |

## 5. 组件级样式

| 组件 | 位置 | 新规则 |
|---|---|---|
| 工具结果正文 | `Theme::tool_result()` | `suggestion` → `text_dim`（**最关键的一处**） |
| 用户消息首行 | `Theme::highlight()` | `text` + BOLD |
| assistant 错误 | `render.rs` | `error` |
| System 标记 | `render.rs` | `warning` / `info` |
| `!` bash 成功 / 正文 | `render.rs` | `success` / `text_dim` |
| diff 上下文行 | `render.rs` | `dim()` → `text_dim` |
| diff 新增 / 删除 | `render.rs` | `success` / `error` |
| PlanProposal | `render.rs` | `plan` 紫 |
| Todo | `render.rs` | Pending `muted`、InProgress `info` + BOLD、Completed `muted` + DIM |
| 状态栏 | `Theme::status_bar()` | `inactive`，无背景（不变） |
| 选中行 | `Theme::selected_row()` | `selected_bg` + `claude` + BOLD |
| 块状光标 | `render.rs` | `selection_on(claude)`，前景深色 |
| 权限对话框 | `Theme::permission_dialog()` | `suggestion` + BOLD |
| bypass 三处 | 状态栏 / 标题栏 / 输入框 | `danger` 同色（硬不变式保留） |
| Auto 模式 | 状态栏 / 标题栏 / 边框 | `inactive`（硬不变式保留：不用绿色） |

## 6. 保持不变的不变式

本次只改取值与少数样式归属，以下规则一条不动：

1. 生产代码不出现裸 `Color::`（`theme_tests::no_hardcoded_colors_outside_theme`）。
2. 新增槽位必须同时接 `ThemePalette` 字段、`Default`、`from_json`、getter、`degraded()`、
   `theme_tests::SLOTS`。
3. 颜色断言断槽位不断具体值。
4. Bypass 三处同色为 `danger`。
5. 状态栏不设显式背景色。
6. Auto 模式用中性灰而非 `success` 绿。
7. 代码块无背景、无卡片；引用块顶格；`⏺`/`⎿` 图标体系。
8. `NO_COLOR` 全槽位 `Color::Reset`；非 truecolor 时 RGB 量化到 xterm256。
9. 对比度兜底阈值 3.0，只用于背景已知的场合。

## 7. 实施清单

1. `crates/tui/src/theme.rs`
   - 替换常量表全部取值，全部改为 `Color::Rgb`。
   - 新增 `inline_code` 槽位：字段、`Default`、`from_json`（键 `inlinecode` / 别名 `inline`）、
     getter、`degraded()`、`Theme::inline_code()` Style。
   - `tool_result()` 改 `text_dim`；`body()` 改 `text`。
2. `crates/tui/src/markdown.rs`
   - `cur_style()`：正文 `Theme::body()`；h3+ `text_dim`；引用正文 `text_dim`。
   - `block_prefix()`：bullet 改 `text_dim`；引用竖线改 `border`。
   - `Event::Code`：改 `Theme::inline_code()`。
   - 链接 URL 注释改 `muted`；分隔线改 `border`；表头改 `text` + BOLD；未闭合围栏改 `text_dim`。
3. `crates/tui/src/render.rs`
   - diff 上下文行改 `text_dim`。
4. `crates/tui/src/theme_tests.rs`
   - `SLOTS` 与 `slot_color` 增加 `inline_code`。
   - 新增对比度测试、标题层级测试、工具结果中性色测试。
5. `crates/tui/CLAUDE.md` 补充色板设计原则。
6. 验证：`cargo fmt`、`cargo test -p wyj-tui`、`cargo clippy -p wyj-tui`。

## 8. 已知取舍

- **正文改为显式冷灰**后，浅色终端下会不可读。本次明确只做暗色；将来若要支持亮色，
  需要整套亮色调色板，而不是单点修补。
- **全部改 RGB** 后，非 truecolor 终端走 256 色量化。现有 `quantize_to_256` 对中性灰走
  24 级灰阶、其余走 6×6×6 立方体，比 ANSI 具名色更可控。
- **品牌橙面积缩小**会改变现有辨识度：assistant 前缀、工具调用行、h1 仍保留橙色，
  列表 bullet 与工具结果退出橙色。
- **插件主题兼容**：槽位名与 `from_json` 别名均不变，现有主题继续生效；只新增 `inlinecode` 键。
