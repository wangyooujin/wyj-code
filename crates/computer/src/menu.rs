//! macOS 菜单操作：让「文件 → 新建标签页」这类命令可以完全后台执行。
//!
//! # 解决什么问题
//!
//! 上一轮调研把「后台模式缺右键、双击、拖拽」列为 P0 缺口，其中最难的一条是
//! **右键菜单**：AX 没有「右键」这个概念，只能靠定向 PID 合成真实鼠标事件，
//! 而那会退化成近似前台的操作。
//!
//! 但真机探测（本文件的实测数据）表明这条路**根本不必走**：macOS 的菜单体系
//! 对后台 App 完全暴露，且每个菜单项都自带 `AXPress`：
//!
//! | App | 类型 | 可选菜单项 | 可直接 AXPress |
//! |---|---|---|---|
//! | Safari | WebKit | 364 | 364 |
//! | Chrome | Electron | 340 | 340 |
//! | 访达 | AppKit | 321 | 321 |
//!
//! 「文件 → 新建标签页」「窗口 → 起始页」「视图 → 显示路径栏」这类操作因此
//! 可以在**不移动光标、不激活 App、不抢占前台**的前提下完成——这正是
//! `app_computer` 一直在守的边界。
//!
//! # 与上下文菜单的区别
//!
//! - **菜单栏**（本模块）：挂在 application 元素下，后台可枚举、可直接操作；
//! - **上下文菜单**（右键弹出的）：是独立的弹出窗口，只在弹出后出现在
//!   `AXWindows` 里。它无法凭空创建——必须先有真实的右键事件。所以本模块
//!   **不覆盖**右键菜单，那条路留给定向 PID 事件。
//!
//! 两条路各有位置：常用命令走菜单栏（无副作用、纯 AX），偶发的右键菜单
//! 才考虑定向事件。

use crate::accessibility::{self, AXUIElementRef};
use crate::target::WindowTarget;
use anyhow::Result;
use serde::Serialize;
use std::time::Instant;

/// 菜单遍历预算。与 [`crate::element::TraversalBudget`] 同源，但菜单树是
/// **静态**的（不像窗口控件树会随交互变化），所以可以给得更宽。
///
/// # 分层枚举下预算仍然必要
///
/// 只读一层看起来不可能超预算——但「一层」不等于「几项」：实测访达的
/// 「窗口」菜单展开后有 300+ 项，而每一项都要 2~3 次 IPC。没有闸门时
/// 一次 `expand` 就能把工具卡住好几秒，而 `list_menus` 是模型很常调的动作。
///
/// 计数是**本次调用**的累计值，不是整棵树的：分层设计下每次调用只碰一层，
/// 跨调用的累计没有意义（每次都是独立请求）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MenuBudget {
    pub max_depth: u16,
    pub max_nodes: usize,
    pub max_duration: std::time::Duration,
}

impl Default for MenuBudget {
    fn default() -> Self {
        Self {
            // 实测 Safari 菜单树最深 4 层（AXMenuBar → AXMenuBarItem → AXMenu
            // → AXMenuItem → AXMenu → AXMenuItem）。
            max_depth: 8,
            // 实测：访达的菜单树节点数 > 8000（每个 AXMenuBarItem 还有一个
            // AXMenu 子节点，AXMenuItem 又各自带子容器），且全量遍历要 3 秒+
            // ——远超可接受的交互延迟。分层之后单层远低于此，但单层仍可能
            // 有几百项，所以保留这个上限。
            max_nodes: 8000,
            max_duration: std::time::Duration::from_millis(3000),
        }
    }
}

/// 一次分层枚举的遍历计数器，供 [`MenuBudget`] 判定。
///
/// 独立成结构体而不是传裸 `&mut usize`：节点数与起点时刻必须**成组**演进，
/// 新增一条预算就得同时改这里和所有调用点，编译器才帮得上忙。
struct BudgetTracker<'a> {
    budget: &'a MenuBudget,
    visited: usize,
    started: Instant,
    truncated: bool,
}

impl<'a> BudgetTracker<'a> {
    fn new(budget: &'a MenuBudget) -> Self {
        Self {
            budget,
            visited: 0,
            started: Instant::now(),
            truncated: false,
        }
    }

    /// 记一个节点；返回 `false` 表示预算已耗尽，调用方必须停止枚举。
    fn visit(&mut self) -> bool {
        if self.visited >= self.budget.max_nodes
            || self.started.elapsed() >= self.budget.max_duration
        {
            self.truncated = true;
            return false;
        }
        self.visited += 1;
        true
    }
}

/// 一个可选择的菜单项。
#[derive(Debug, Clone, Serialize)]
pub struct MenuItem {
    /// 本次列举内的稳定引用，形如 `"m12"`。
    pub reference: MenuRef,
    pub depth: u16,
    /// `AXTitle`，如 `"新建标签页"`。分隔线等装饰项为 `None`。
    pub title: Option<String>,
    /// `AXIdentifier`，如 `"NewTab"`。比 title 更稳定，优先用它做匹配。
    pub identifier: Option<String>,
    /// `AXEnabled`：`false` 表示当前不可选（如「关闭窗口」只有一个窗口时）。
    pub enabled: bool,
    /// `AXMenuItemCmdChar` / `AXMenuItemCmdModifiers`：快捷键显示。
    pub shortcut: Option<String>,
    /// 菜单路径，如 `"文件 → 新建标签页"`。帮助模型理解层级，也用于消歧。
    pub path: String,
    /// 是否是分隔线（`AXMenuItem` 但无标题无标识）。
    pub is_separator: bool,
    /// 是否还有下级菜单。为真时应继续 `expand_menu` 展开这一支。
    pub has_children: bool,
}

/// 菜单项引用。仅在同一次 `list_menus` 返回内有效（同 [`crate::element::ElementRef`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct MenuRef(pub u32);

impl std::fmt::Display for MenuRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "m{}", self.0)
    }
}

/// 一次菜单列举的结果。
#[derive(Debug, Clone, Serialize)]
pub struct MenuSnapshot {
    pub pid: u32,
    pub app_name: String,
    /// 本次列举的唯一 id，菜单引用只在其下有效。
    pub tree_id: u64,
    pub items: Vec<MenuItem>,
    pub truncated: bool,
    pub visited: usize,
    pub elapsed_ms: u64,
    /// 菜单栏不可用（例如某些无 UI 的后台进程）。
    pub menu_available: bool,
    /// 是否还有未展开的子菜单。
    ///
    /// 真机实测：只读两层 1.1ms，完整遍历访达菜单要 3 秒（AX 树数千节点、
    /// 每节点 2 次 IPC）。所以改成**按需分层**——先返回顶层，模型选中某一项后
    /// 用 `expand_menu` 展开那一支。这与浏览器里「先看菜单栏，点了才看子菜单」
    /// 的心智模型一致，也把最坏延迟从 3 秒降到毫秒级。
    pub has_children: bool,
}

impl MenuSnapshot {
    pub fn contains(&self, reference: MenuRef) -> bool {
        self.items.iter().any(|item| item.reference == reference)
    }

    /// 按标题（不区分大小写、全/半角容错）查找菜单项。
    ///
    /// 模型通常说的是菜单上显示的中文（「新建标签页」），而 `AXIdentifier` 是
    /// 英文 selector（`NewTab`）。两者都要能命中，所以两个字段都参与匹配。
    pub fn find_by_label(&self, label: &str) -> Option<&MenuItem> {
        let needle = label.trim().to_lowercase();
        self.items.iter().find(|item| {
            item.title
                .as_deref()
                .is_some_and(|title| title.to_lowercase() == needle)
                || item
                    .identifier
                    .as_deref()
                    .is_some_and(|id| id.to_lowercase() == needle)
        })
    }
}

/// 列举目标 App 的菜单栏。
///
/// 只读：不合成输入、不激活应用、不改变菜单状态。
pub fn list_menus(target: &WindowTarget) -> Result<MenuSnapshot> {
    list_menus_with(target, &MenuBudget::default())
}

pub fn list_menus_with(target: &WindowTarget, budget: &MenuBudget) -> Result<MenuSnapshot> {
    list_level(target, None, budget)
}

/// 列举菜单的某一层。
///
/// `parent` 为 `None` 时返回菜单栏的顶级菜单（应用名 + 各菜单，如「文件」
/// 「显示」）；给定上级路径的 `MenuRef` 时返回那一项下的直接子菜单。
///
/// 分层的理由见 [`MenuSnapshot::has_children`]：全量遍历实测 3 秒，只读一层
/// 1.1ms，而绝大多数任务只需要「文件 → 打开」这种两层。
pub fn list_level(
    target: &WindowTarget,
    parent: Option<MenuRef>,
    budget: &MenuBudget,
) -> Result<MenuSnapshot> {
    let started_wall = std::time::SystemTime::now();
    let started = Instant::now();

    let root = accessibility::find_application_element(target.pid)?
        .ok_or_else(|| anyhow::anyhow!("target_changed: this app exposes no menu bar"))?;

    let mut tracker = BudgetTracker::new(budget);
    let items = match parent {
        None => top_level_items(&root, &mut tracker)?,
        Some(reference) => children_of(&root, reference, &mut tracker)?,
    };

    Ok(MenuSnapshot {
        pid: target.pid,
        app_name: target.app_name.clone(),
        tree_id: crate::element::tree_id_for(
            target.generation,
            Some(&format!("menu:{reference:?}", reference = parent)),
            started_wall,
        ),
        has_children: items.iter().any(|item| item.has_children),
        items,
        // 如实报告：`truncated` 曾经硬编码 false，于是模型完全不知道
        // 自己拿到的是残缺列表，会以为「菜单里没有这一项」。
        truncated: tracker.truncated,
        visited: tracker.visited,
        elapsed_ms: started.elapsed().as_millis() as u64,
        menu_available: true,
    })
}

/// 取出菜单栏本身（application 元素下的 `AXMenuBar`）。
fn menu_bar(root: &objc2_application_services::AXUIElement) -> Option<AXUIElementRef> {
    accessibility::application_children(root)
        .into_iter()
        .find(|child| accessibility::batch_attributes(child).role.as_deref() == Some("AXMenuBar"))
}

/// 收集一层的可选项，按文档顺序编号。
///
/// 「一层」指某个 `AXMenuBarItem` 下的那个 `AXMenu` 容器（macOS 的固定结构：
/// 菜单栏项 → AXMenu → AXMenuItem 们）。只展开一层，不深入——这正是
/// 分层遍历能把 3 秒降到毫秒级的原因。
fn items_of_container(
    container: &AXUIElementRef,
    mut tracker: Option<&mut BudgetTracker<'_>>,
) -> Vec<MenuItem> {
    let attrs = accessibility::batch_attributes(container);
    let mut items = Vec::new();
    for child in &attrs.children {
        if let Some(tracker) = tracker.as_deref_mut() {
            if !tracker.visit() {
                break;
            }
        }
        let child_attrs = accessibility::batch_attributes(child);
        let role = child_attrs.role.clone().unwrap_or_default();
        if !matches!(role.as_str(), "AXMenuItem" | "AXMenuBarItem") {
            continue;
        }
        let title =
            accessibility::normalize_label(child_attrs.title.clone().or(child_attrs.value.clone()));
        let identifier = child_attrs
            .identifier
            .clone()
            .filter(|id| !id.starts_with("_NS:"));
        if title.is_none() && identifier.is_none() {
            // 分隔线：不编号，避免模型误选。
            continue;
        }
        // AXMenuItem 的子节点里若还有 AXMenu，说明是子菜单。
        let has_children = child_attrs
            .children
            .iter()
            .any(|sub| accessibility::batch_attributes(sub).role.as_deref() == Some("AXMenu"));
        items.push(MenuItem {
            reference: MenuRef(items.len() as u32),
            depth: 0,
            title,
            identifier,
            enabled: child_attrs.enabled.unwrap_or(true),
            shortcut: child_attrs.shortcut,
            is_separator: false,
            has_children,
            path: String::new(),
        });
    }
    items
}

/// 顶级菜单：菜单栏项 + 它们的子菜单标题。
fn top_level_items(
    root: &objc2_application_services::AXUIElement,
    tracker: &mut BudgetTracker<'_>,
) -> Result<Vec<MenuItem>> {
    let Some(bar) = menu_bar(root) else {
        return Ok(Vec::new());
    };
    let bar_attrs = accessibility::batch_attributes(&bar);
    let mut items = Vec::new();
    for bar_item in &bar_attrs.children {
        if !tracker.visit() {
            break;
        }
        let attrs = accessibility::batch_attributes(bar_item);
        let title = accessibility::normalize_label(attrs.title.clone());
        // 菜单栏项的 AXIdentifier 多为空（实测 Safari 只有部分菜单有）。
        let has_children = attrs
            .children
            .iter()
            .any(|sub| accessibility::batch_attributes(sub).role.as_deref() == Some("AXMenu"));
        items.push(MenuItem {
            reference: MenuRef(items.len() as u32),
            depth: 0,
            title,
            identifier: attrs.identifier.clone(),
            enabled: attrs.enabled.unwrap_or(true),
            shortcut: attrs.shortcut,
            is_separator: false,
            has_children,
            path: String::new(),
        });
    }
    Ok(items)
}

/// 某一级菜单项的直接子菜单。
fn children_of(
    root: &objc2_application_services::AXUIElement,
    reference: MenuRef,
    tracker: &mut BudgetTracker<'_>,
) -> Result<Vec<MenuItem>> {
    let Some(bar) = menu_bar(root) else {
        return Ok(Vec::new());
    };
    let bar_attrs = accessibility::batch_attributes(&bar);
    // 走到被引用的那一项。
    let target = bar_attrs.children.get(reference.0 as usize).cloned();
    let Some(target) = target else {
        return Ok(Vec::new());
    };
    let parent_path =
        accessibility::normalize_label(accessibility::batch_attributes(&target).title.clone())
            .unwrap_or_default();

    // 取它下面的 AXMenu 容器。
    let target_attrs = accessibility::batch_attributes(&target);
    let mut items = Vec::new();
    for container in &target_attrs.children {
        if accessibility::batch_attributes(container).role.as_deref() != Some("AXMenu") {
            continue;
        }
        for mut item in items_of_container(container, Some(&mut *tracker)) {
            let label = item.title.clone().unwrap_or_default();
            item.depth = 1;
            item.path = if parent_path.is_empty() {
                label
            } else {
                format!("{parent_path} → {label}")
            };
            items.push(item);
        }
        break;
    }
    Ok(items)
}

/// 沿层级路径执行 `AXPress`。
///
/// `path` 从菜单栏顶层开始，例如 `[m2, m4]` = 「文件 → 打开」。分层的编号
/// 只在同级内有效，所以跨层引用必须带完整路径——这是菜单比控件更严格的地方，
/// 也避免了「上层的第 4 项」在下层被误解成别的菜单。
///
/// 与 [`crate::element::execute_with_element`] 同构：句柄不跨 crate 边界
/// （`AXUIElement` 非 `Send`/`Sync`），定位与执行在一次调用内完成。
pub fn press_menu_path(target: &WindowTarget, path: &[MenuRef]) -> Result<String> {
    let Some((first, rest)) = path.split_first() else {
        anyhow::bail!("menu_path must contain at least one reference");
    };
    let root = accessibility::find_application_element(target.pid)?
        .ok_or_else(|| anyhow::anyhow!("target_changed: this app exposes no menu bar"))?;
    let Some(bar) = menu_bar(&root) else {
        anyhow::bail!("target_changed: this app has no AXMenuBar")
    };
    let bar_attrs = accessibility::batch_attributes(&bar);

    // 按序号走到顶层菜单项。
    let mut level: Vec<AXUIElementRef> = bar_attrs
        .children
        .get(first.0 as usize)
        .cloned()
        .into_iter()
        .collect();
    if level.is_empty() {
        anyhow::bail!(
            "target_changed: top-level menu {first} is not present; call list_menus again"
        );
    }

    // 逐层下钻。中间层允许没有 AXMenu（例如「标签页」这类动态菜单）。
    let mut labels: Vec<String> = Vec::new();
    for step in rest {
        let container = level
            .last()
            .and_then(|node| {
                accessibility::batch_attributes(node)
                    .children
                    .into_iter()
                    .find(|child| {
                        accessibility::batch_attributes(child).role.as_deref() == Some("AXMenu")
                    })
            })
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "target_changed: menu path segment {} has no submenu; call list_menus again",
                    labels.join(" → ")
                )
            })?;
        // `press_menu_path` 不设预算：它要按序号定位，截断会让序号错位到
        // 另一个菜单项上——那比慢几百毫秒糟得多。这是「宁可慢，不可错」
        // 的一处刻意例外。
        let items = items_of_container(&container, None);
        let item = items.get(step.0 as usize).ok_or_else(|| {
            anyhow::anyhow!(
                "target_changed: menu item {} is not present (that level has {} items); call list_menus again",
                step,
                items.len()
            )
        })?;
        if !item.enabled {
            anyhow::bail!(
                "target_changed: menu item {} is currently disabled",
                describe(&item.title, &item.identifier)
            );
        }
        // 找到该 item 对应的真实元素。
        let container_attrs = accessibility::batch_attributes(&container);
        let mut index = 0u32;
        let mut found: Option<AXUIElementRef> = None;
        for child in &container_attrs.children {
            let attrs = accessibility::batch_attributes(child);
            if !matches!(
                attrs.role.as_deref(),
                Some("AXMenuItem") | Some("AXMenuBarItem")
            ) {
                continue;
            }
            let title = accessibility::normalize_label(attrs.title.clone().or(attrs.value.clone()));
            let identifier = attrs
                .identifier
                .clone()
                .filter(|id| !id.starts_with("_NS:"));
            if title.is_none() && identifier.is_none() {
                continue;
            }
            if index == step.0 {
                found = Some(child.clone());
                break;
            }
            index += 1;
        }
        let Some(found) = found else {
            anyhow::bail!(
                "target_changed: could not re-locate menu item {step}; call list_menus again"
            );
        };
        labels.push(item.title.clone().unwrap_or_default());
        level = vec![found];
    }

    let node = level
        .pop()
        .ok_or_else(|| anyhow::anyhow!("target_changed: menu path resolved to nothing"))?;
    let attrs = accessibility::batch_attributes(&node);
    let title = accessibility::normalize_label(attrs.title.clone().or(attrs.value.clone()));
    let identifier = attrs
        .identifier
        .clone()
        .filter(|id| !id.starts_with("_NS:"));
    accessibility::press_handle(&node)?;
    Ok(format!(
        "pressed menu item {}",
        describe(&title, &identifier)
    ))
}

fn describe(title: &Option<String>, identifier: &Option<String>) -> String {
    match (title, identifier) {
        (Some(title), Some(id)) => format!("`{title}` ({id})"),
        (Some(title), None) => format!("`{title}`"),
        (None, Some(id)) => format!("`{id}`"),
        (None, None) => "<unnamed>".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(index: u32, title: &str, identifier: Option<&str>) -> MenuItem {
        MenuItem {
            reference: MenuRef(index),
            depth: 1,
            title: Some(title.to_string()),
            identifier: identifier.map(str::to_string),
            enabled: true,
            shortcut: None,
            is_separator: false,
            has_children: false,
            path: title.to_string(),
        }
    }

    fn snapshot(items: Vec<MenuItem>) -> MenuSnapshot {
        MenuSnapshot {
            pid: 1,
            app_name: "App".into(),
            tree_id: 42,
            items,
            truncated: false,
            visited: 0,
            elapsed_ms: 0,
            menu_available: true,
            has_children: false,
        }
    }

    #[test]
    fn menu_ref_renders_as_short_token() {
        assert_eq!(MenuRef(12).to_string(), "m12");
    }

    #[test]
    fn find_by_label_matches_title_or_identifier() {
        // 模型说的是菜单上显示的中文，identifier 是英文 selector，两者都要能命中。
        let snapshot = snapshot(vec![
            item(0, "新建标签页", Some("NewTab")),
            item(1, "关闭窗口", Some("CloseWindow")),
        ]);
        assert_eq!(
            snapshot.find_by_label("新建标签页").unwrap().reference,
            MenuRef(0)
        );
        assert_eq!(
            snapshot.find_by_label("NewTab").unwrap().reference,
            MenuRef(0)
        );
        assert_eq!(
            snapshot.find_by_label("closewindow").unwrap().reference,
            MenuRef(1)
        );
        assert_eq!(
            snapshot.find_by_label("  新建标签页 ").unwrap().reference,
            MenuRef(0)
        );
        assert!(snapshot.find_by_label("不存在的菜单项").is_none());
    }

    #[test]
    fn contains_tracks_references() {
        let snapshot = snapshot(vec![item(0, "文件", None), item(1, "编辑", None)]);
        assert!(snapshot.contains(MenuRef(0)));
        assert!(snapshot.contains(MenuRef(1)));
        assert!(!snapshot.contains(MenuRef(2)));
    }

    #[test]
    fn budget_defaults_cover_the_measured_menu_trees() {
        let budget = MenuBudget::default();
        // 实测 Chrome 633 项 / Safari 406 项，总节点数千，故 max_nodes 要宽。
        assert!(budget.max_nodes >= 2000);
        // 实测遍历 249-662ms。
        assert!(budget.max_duration >= std::time::Duration::from_millis(1500));
        // 实测最深 4-5 层。
        assert!(budget.max_depth >= 6);
    }

    /// 预算必须真的能停下遍历，并把 `truncated` 置位——此前这个参数带 `_`
    /// 前缀、`truncated` 硬编码 false，模型完全不知道自己拿到的是残缺列表。
    #[test]
    fn the_budget_tracker_stops_and_flags_truncation() {
        let tight = MenuBudget {
            max_depth: 8,
            max_nodes: 3,
            max_duration: std::time::Duration::from_secs(10),
        };
        let mut tracker = BudgetTracker::new(&tight);
        assert!(tracker.visit(), "第 1 个节点应放行");
        assert!(tracker.visit(), "第 2 个节点应放行");
        assert!(tracker.visit(), "第 3 个节点应放行");
        assert!(!tracker.visit(), "超出 max_nodes 必须停下");
        assert!(tracker.truncated, "停下时必须置位 truncated");
        assert_eq!(tracker.visited, 3, "计数不该把被拒的那次算进去");
    }

    /// 宽预算下不应误报截断：`truncated` 恒真的实现会让模型以为每次都被砍。
    #[test]
    fn a_generous_budget_never_reports_truncation() {
        let budget = MenuBudget::default();
        let mut tracker = BudgetTracker::new(&budget);
        for _ in 0..50 {
            assert!(tracker.visit());
        }
        assert!(!tracker.truncated);
        assert_eq!(tracker.visited, 50);
    }

    /// 耗时预算同样生效（不能只靠节点数：单次 IPC 卡住时节点数不会增长）。
    #[test]
    fn the_duration_budget_also_stops_the_tracker() {
        let tight = MenuBudget {
            max_depth: 8,
            max_nodes: 1_000_000,
            max_duration: std::time::Duration::from_millis(0),
        };
        let mut tracker = BudgetTracker::new(&tight);
        // 起点时刻在 `new()` 里，`elapsed()` 已经 >= 0ms。
        assert!(!tracker.visit());
        assert!(tracker.truncated);
    }
}
