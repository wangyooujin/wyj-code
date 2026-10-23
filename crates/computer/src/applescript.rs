//! 白名单式 AppleScript 执行：只跑编译进二进制的固定脚本，参数走 argv。
//!
//! # 为什么不是「给模型一个执行 AppleScript 的工具」
//!
//! 两条硬理由：
//!
//! 1. **安全面**。AppleScript 能驱动应用做 Bash 做不了的事，而
//!    `core::auto_rules::is_shell_or_interpreter` 已把 `osascript` 列为解释器
//!    ——`curl | osascript` 是被硬拦的。开放通用脚本执行等于自己开一个绕过
//!    `hard_deny` 的口子，且脚本正文由模型生成，审计不了。
//! 2. **可靠性**。`osascript` 的输出是人读格式：多值用 `, ` 拼接、空值是
//!    字面量 `missing value`、标题里还会混入 U+2068 / U+2069 这类方向标记
//!    （实测访达窗口标题就被这对标记包裹过）。通用解析必然脆弱，而每条固定
//!    脚本可以在脚本内就把结果规格化成 `key=value` 行。
//!
//! 所以这里只有一张**编译期固定的 recipe 表**：模型能选的只有 op 名与参数
//! 值，脚本正文永远来自本文件。
//!
//! # 参数必须走 argv，不能拼进脚本文本
//!
//! `osascript -e SCRIPT -- arg1 arg2` 把参数作为 `argv` 交给 `on run argv`。
//! 实测 `he said "hi" ; echo injected` 这类输入原样作为字符串到达，不会被执行
//! ——它是**参数**，不是脚本片段。把参数拼进脚本文本则恰好相反：一个带引号的
//! 文件名就能改写整条脚本。这两者的差别是本模块唯一的安全边界。
//!
//! # 延迟实测（本机 macOS 27.0.1，热态）
//!
//! | 调用 | 耗时 |
//! |---|---|
//! | `tell app "Finder" to get name of every window` | 0.116s |
//! | `tell process "Finder" to get name of every menu bar item` | 0.146s |
//! | `tell process "Finder" to get entire contents of window 1` | 0.36s（**3 万字符**） |
//! | 对照：native AX 抽取 Chrome 窗口 1172 个带坐标文本节点 | 0.26s |
//!
//! 最后两行是本模块**只做语义级 op、不做 UI scripting** 的依据：走
//! `System Events` 的 UI scripting 能力不超过已有的 native AX 实现，却慢
//! 2~10 倍、没有坐标、输出还得二次解析。真正的增量只在应用自己的 scripting
//! dictionary（`sdef`）——那是 AX 完全够不到的地方。

use anyhow::{anyhow, bail, Result};
use std::process::Command;

/// 一次 recipe 调用的结果。
#[derive(Debug, Clone)]
pub struct RecipeOutput {
    /// 脚本输出的原始文本（已 trim 尾部空白）。
    pub stdout: String,
    /// 脚本 stderr 的**前几行**：AppleScript 的错误信息通常在第一行，后面是
    /// 调用栈噪音。截断是为了不让一次失败灌满上下文。
    pub stderr: String,
}

/// 一条白名单 recipe。
#[derive(Debug, Clone, Copy)]
pub struct Recipe {
    /// 模型侧使用的 op 名，也是查表键。
    pub op: &'static str,
    /// 这条 recipe 需要几个参数（按位置传给 `on run argv`）。
    pub arity: usize,
    /// 脚本正文。**编译期常量，永不来自模型输入**。
    script: &'static str,
    /// 给模型看的一句话说明：它能做什么、有什么限制。
    pub summary: &'static str,
}

/// 参数数量不符。
///
/// 独立成类型而不是 `anyhow!` 字符串：调用方要把它和「脚本执行失败」区分开
/// ——前者是模型的调用错误（应当原样告诉它正确用法），后者可能是授权问题
/// （应当提示用户去系统设置里开权限）。
#[derive(Debug)]
pub struct ArityError {
    pub op: &'static str,
    pub expected: usize,
    pub actual: usize,
}

impl std::fmt::Display for ArityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "recipe `{}` takes {} argument(s), got {}",
            self.op, self.expected, self.actual
        )
    }
}

impl std::error::Error for ArityError {}

/// 白名单表。
///
/// # 选品标准
///
/// 只收**AX 做不到、且确实是日常任务**的 op。每条都要能回答「为什么不能
/// 用现有的 `app_computer` 完成」：
///
/// - `finder_new_folder`：AX 能点「新建文件夹」菜单项，但**不能给新文件夹
///   命名**——命名要输入文本，而访达的新建项处于内联编辑态，`set_text` 在
///   那里会命中错误的元素。AppleScript 一句 `make new folder with properties
///   {name:...}` 一步到位。
/// - `notes_new_note`：备忘录的正文是富文本 `NSTextView`，AX 写入只能拿到
///   光标位置插入，建不了「新的一条笔记」。这是 `sdef` 独有的能力。
/// - `chrome_open_url`：用 AX 需要「点地址栏 → 全选 → 输入 → 回车」四步，
///   且地址栏在 Chrome 的 AX 树里是 `AXTextField` 但 `set_text` 常被拒；
///   AppleScript 一句 `set URL of active tab`。注意它需要「允许 JavaScript
///   来自 Apple 事件」是**不需要**的——那条限制只对 `execute javascript` 生效。
///
/// # 刻意不收的
///
/// - 任何 `System Events` 的 UI scripting：见模块注释的延迟实测，它比 native
///   AX 更慢更弱。
/// - 任何 `execute javascript`：本机实测 Chrome 与 Safari **都默认关闭**
///   （错误码 12 / 8），要求用户手工打开开发者选项，不是可依赖的路径。
/// - 任何 `do shell script`：那是 Bash 的职责，且会绕过 `hard_deny`。
pub const RECIPES: &[Recipe] = &[
    Recipe {
        op: "finder_new_folder",
        arity: 2,
        summary: "Create a new folder in Finder. args: [parent_posix_path, folder_name]. \
                  Does not need the Finder window to be visible.",
        script: r#"
on run argv
    set parentPath to item 1 of argv
    set folderName to item 2 of argv
    set parentAlias to POSIX file parentPath as alias
    tell application "Finder"
        if exists folder folderName of parentAlias then
            return "status=exists"
        end if
        make new folder at parentAlias with properties {name:folderName}
    end tell
    return "status=created"
end run
"#,
    },
    Recipe {
        op: "notes_new_note",
        arity: 2,
        summary: "Create a note in Notes.app. args: [title, body]. Returns the note id. \
                  The body is plain text; each newline becomes a paragraph break.",
        // Notes 的 `body` 是 **HTML**，不是纯文本：直接传 `第一行\n第二行` 会在
        // 渲染时把换行压成空格（实测两次都变成同一段）。必须自己把换行折成
        // `<br>`，这也解释了为什么这条 op 不能靠 AX 写入完成——那里没有
        // 「设置整篇正文」的语义动作。
        //
        // **两步必须分开，不能连写**：`text items` / `as string` 用的是**同一个**
        // 分隔符。写成「设 linefeed → `(text items of s) as string`」等于
        // 按换行切开又用换行拼回去，结果原样不动（实测第一次就这么错了）。
        // 正确做法是先把切好的列表**存下来**，再换分隔符拼成 `<br>`。
        script: r#"
on run argv
    set noteTitle to item 1 of argv
    set noteBody to item 2 of argv
    set AppleScript's text item delimiters to linefeed
    set bodyLines to text items of noteBody
    set AppleScript's text item delimiters to "<br>"
    set bodyHTML to bodyLines as string
    set AppleScript's text item delimiters to ""
    tell application "Notes"
        set newNote to make new note at folder "Notes" of default account with properties {name:noteTitle, body:bodyHTML}
        return "id=" & (id of newNote as string)
    end tell
end run
"#,
    },
    Recipe {
        op: "chrome_open_url",
        arity: 1,
        summary: "Open a URL in a new tab of the front Chrome window. arg: [url]. \
                  Requires an existing Chrome window; use `open -a` first otherwise.",
        script: r#"
on run argv
    set targetURL to item 1 of argv
    tell application "Google Chrome"
        if (count of windows) is 0 then
            make new window
        end if
        tell front window
            set newTab to make new tab at end of tabs with properties {URL:targetURL}
        end tell
        return "url=" & targetURL
    end tell
end run
"#,
    },
];

/// 按 op 名查表。
pub fn find(op: &str) -> Option<&'static Recipe> {
    RECIPES.iter().find(|recipe| recipe.op == op)
}

/// 执行一条 recipe。
///
/// # 不做授权预检
///
/// macOS 的 AppleEvents 授权（TCC `AppleEvents`）没有「只查不弹」的公开 API
/// 绑定在本 crate 的依赖里，而**首次调用必然弹一次系统模态框**。因此本函数
/// 不做预检，直接执行：
///
/// - 使用者（工具层）必须在**有真实交互面**时才允许调用——弹框需要有人点。
/// - 授权被拒时 `osascript` 会以非零码退出并在 stderr 给出
///   `Not authorized to send Apple events to ...`，本函数据此返回结构化错误，
///   由调用方翻译成「去系统设置 > 隐私与安全性 > 自动化 里勾选」。
///
/// 不把这条路径做成「静默降级到 AX」：降级的语义是「换一种方式做同一件事」，
/// 而这里失败的原因是「用户没授权」，静默换成另一套动作会让用户失去对
/// 「什么在控制我的应用」的知情权。
pub fn run(recipe: &Recipe, args: &[String]) -> Result<RecipeOutput> {
    if args.len() != recipe.arity {
        return Err(anyhow!(ArityError {
            op: recipe.op,
            expected: recipe.arity,
            actual: args.len(),
        }));
    }
    let output = Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(recipe.script)
        // `--` 之后的一切都是 argv，不会被当作脚本或选项解析。
        .arg("--")
        .args(args)
        .output()
        .map_err(|error| anyhow!("could not launch osascript: {error}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr_raw = String::from_utf8_lossy(&output.stderr);
    let stderr = summarize_stderr(&stderr_raw);

    if !output.status.success() {
        return Err(anyhow!(classify_failure(&stderr, recipe.op)));
    }
    Ok(RecipeOutput { stdout, stderr })
}

/// 把 stderr 压成一行到三行。
///
/// `osascript` 的失败输出通常是 `<行号>:<列号>: execution error: <原因> (<错误码>)`，
/// 后面偶尔跟一行说明。取前 3 行足够定位，且不会因为某个脚本打印了大段调试
/// 信息就灌满上下文。
fn summarize_stderr(raw: &str) -> String {
    let joined: Vec<&str> = raw
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(3)
        .collect();
    joined.join(" / ")
}

/// 把 `osascript` 的失败归到一个可操作的类别上。
///
/// 三类失败的处置**完全不同**，混成一句「脚本执行失败」会让模型反复重试一个
/// 永远不会成功的调用：
///
/// - **未授权**（`Not authorized` / `-1743`）：需要用户去系统设置，重试无用。
/// - **目标应用未运行**（`-1728` / `Can't get ...`）：可以先启动再试。
/// - **其他**：交给调用方按普通错误处理。
fn classify_failure(stderr: &str, op: &str) -> String {
    let lower = stderr.to_ascii_lowercase();
    if lower.contains("not authorized")
        || stderr.contains("-1743")
        || lower.contains("erraeeventnotpermitted")
    {
        format!(
            "appleevents_permission_required: recipe `{op}` is not authorized to control the \
             target application. macOS asks once per (this app, target app) pair; the user must \
             enable it in System Settings > Privacy & Security > Automation. Do not retry — \
             report this to the user. ({stderr})"
        )
    } else if stderr.contains("-1728") || lower.contains("can't get") || lower.contains("can’t get")
    {
        format!(
            "recipe_target_unavailable: the target application is not running or has no open \
             document; start it first and try again. ({stderr})"
        )
    } else {
        format!("recipe_failed: `{op}` failed: {stderr}")
    }
}

/// 模型侧可见的 recipe 清单，供工具 schema 与描述复用。
///
/// 单独成函数是为了让「有哪些 op」只有一个定义处：schema 的 `enum`、描述文本
/// 与运行时分派都从它派生，加一条 recipe 只改 [`RECIPES`] 一处。
pub fn op_names() -> Vec<&'static str> {
    RECIPES.iter().map(|recipe| recipe.op).collect()
}

/// 校验 op 名并返回 recipe；未知名给出「有哪些可用」的清单。
///
/// 报错里必须带完整清单：模型只有看到可选集合才能自我纠正，而 schema 的
/// `enum` 在某些客户端会被忽略。
pub fn require(op: &str) -> Result<&'static Recipe> {
    find(op).ok_or_else(|| {
        anyhow!(
            "unknown recipe `{op}`; available: {}",
            op_names().join(", ")
        )
    })
}

/// 参数列表的构造辅助：把 `Vec<String>` 从 JSON 里收好，顺便挡掉空参数。
///
/// 空字符串不是「合法但没意义」——它会让脚本里的 `POSIX file "" as alias`
/// 直接抛错，且错误信息对模型毫无指导性。在入口处拒绝更省一次往返。
pub fn validate_args<'a>(recipe: &Recipe, args: &'a [String]) -> Result<&'a [String]> {
    if args.len() != recipe.arity {
        bail!(ArityError {
            op: recipe.op,
            expected: recipe.arity,
            actual: args.len(),
        });
    }
    if let Some(index) = args.iter().position(|arg| arg.trim().is_empty()) {
        bail!("recipe `{}`: argument {} is empty", recipe.op, index + 1);
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_recipe_is_findable_and_unique() {
        for recipe in RECIPES {
            assert!(find(recipe.op).is_some(), "{} 必须能按名字查到", recipe.op);
            assert!(!recipe.summary.is_empty(), "{} 缺少说明", recipe.op);
            // 脚本正文必须是编译期常量，且真的声明了 argv。
            assert!(
                recipe.script.contains("on run argv"),
                "{} 的脚本必须通过 argv 取参",
                recipe.op
            );
            assert!(
                !recipe.script.contains("do shell script"),
                "{} 不得调用 shell：那是 Bash 的职责，且会绕过 hard_deny",
                recipe.op
            );
        }
        let mut names = op_names();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "op 名不得重复");
    }

    /// 参数个数不符要给结构化错误，且错误里带正确的期望值。
    #[test]
    fn arity_mismatch_is_reported_with_the_expected_count() {
        let recipe = find("notes_new_note").unwrap();
        let error = run(recipe, &["only-one".to_string()]).unwrap_err();
        let arity = error
            .downcast_ref::<ArityError>()
            .expect("必须是 ArityError");
        assert_eq!(arity.expected, 2);
        assert_eq!(arity.actual, 1);
        assert!(error.to_string().contains("takes 2 argument"));
    }

    /// 空参数在入口就拒绝：空路径到脚本里会变成毫无指导性的 alias 错误。
    #[test]
    fn empty_arguments_are_rejected_before_running() {
        let recipe = find("chrome_open_url").unwrap();
        let error = validate_args(recipe, &["  ".to_string()]).unwrap_err();
        assert!(error.to_string().contains("argument 1 is empty"), "{error}");
        assert!(validate_args(recipe, &["https://example.com".to_string()]).is_ok());
    }

    /// 未知 op 的报错必须列出全部可用 recipe —— schema 的 enum 在某些客户端
    /// 会被忽略，模型只能靠这条报错自我纠正。
    #[test]
    fn unknown_op_lists_every_available_recipe() {
        let error = require("does_not_exist").unwrap_err().to_string();
        for recipe in RECIPES {
            assert!(
                error.contains(recipe.op),
                "报错里缺少 `{}`：{error}",
                recipe.op
            );
        }
    }

    /// 未授权必须被单独归类：它需要用户去系统设置，重试永远不会成功。
    #[test]
    fn permission_failures_are_classified_separately() {
        for stderr in [
            "execution error: Not authorized to send Apple events to Finder. (-1743)",
            "12:34: execution error: errAEEventNotPermitted",
        ] {
            let message = classify_failure(stderr, "finder_new_folder");
            assert!(
                message.starts_with("appleevents_permission_required"),
                "{stderr} -> {message}"
            );
            assert!(message.contains("Do not retry"), "{message}");
        }
        // 目标未运行是另一类：可以先启动再试。
        let unavailable = classify_failure(
            "execution error: Google Chrome got an error: Can’t get window 1. (-1728)",
            "chrome_open_url",
        );
        assert!(
            unavailable.starts_with("recipe_target_unavailable"),
            "{unavailable}"
        );
        // 其他错误保持通用措辞。
        let other = classify_failure("something exploded", "notes_new_note");
        assert!(other.starts_with("recipe_failed"), "{other}");
    }

    #[test]
    fn stderr_is_truncated_to_three_lines() {
        let raw = "line one\nline two\nline three\nline four\nline five";
        assert_eq!(summarize_stderr(raw), "line one / line two / line three");
        assert_eq!(summarize_stderr("  \n\n single \n"), "single");
        assert_eq!(summarize_stderr(""), "");
    }
}
