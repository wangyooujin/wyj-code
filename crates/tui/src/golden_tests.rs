//!
//! 为什么需要它：现有 339 个断言全是"这一行包含某个词"的形状，**颜色与排版
//! 完全没有覆盖**。于是「把某处硬编码青换成主题槽位」这类改动既不会变红，也
//! 没人看得出聊天区换了张脸——本轮把 80+ 处硬编码色迁进主题时正是靠它
//! 证明"文本一个字没动、变的只有色"。
//!
//! **只收 i18n 无关的帧**：`cargo test` 默认并行，测试里调 `set_locale` 会与同文件的
//! 其它用例串味（见 `render.rs` 里 `spinner_text_keys_resolve_with_their_placeholders`
//! 的同款说明），所以含 `tr()` 文案的帧一律不进快照，改用普通断言覆盖。
//!
//! 重新生成基线：`WYJ_CODE_UPDATE_GOLDEN=1 cargo test -p wyj-tui golden`

use std::path::PathBuf;

use crate::app::{ChatMessage, MessageRole};
use crate::render::build_pending_chat_lines;
use crate::test_support::{make_state, message, rendered_text};

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("{name}.txt"))
}

fn assert_golden(name: &str, actual: &str) {
    let path = golden_path(name);
    if std::env::var("WYJ_CODE_UPDATE_GOLDEN").is_ok() {
        std::fs::create_dir_all(path.parent().expect("golden dir")).expect("create golden dir");
        std::fs::write(&path, actual).expect("write golden file");
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "缺少快照基线 {}（{err}）。用 WYJ_CODE_UPDATE_GOLDEN=1 cargo test -p wyj-tui golden 生成",
            path.display()
        )
    });
    if expected != actual {
        panic!(
            "快照 `{}` 与基线不一致：\n{}\n基线：{}\n实际：{}",
            name,
            unified_diff(&expected, actual),
            path.display(),
            actual
        );
    }
}

/// 逐行标出首个分叉点前后各若干行——只说"不一致"没法定位。
fn unified_diff(expected: &str, actual: &str) -> String {
    let old: Vec<&str> = expected.lines().collect();
    let new: Vec<&str> = actual.lines().collect();
    let mut out = String::new();
    for i in 0..old.len().max(new.len()) {
        let o = old.get(i).copied();
        let n = new.get(i).copied();
        if o == n {
            continue;
        }
        let from = i.saturating_sub(2);
        let to = (i + 6).min(old.len().max(new.len()));
        for j in from..to {
            out.push_str(&format!(
                "{j:>4} - {}\n",
                old.get(j).copied().unwrap_or("<无此行>")
            ));
            out.push_str(&format!(
                "     + {}\n",
                new.get(j).copied().unwrap_or("<无此行>")
            ));
        }
        break;
    }
    if out.is_empty() {
        out.push_str("(行数相同但内容不同)\n");
    }
    out
}

/// 把一组消息渲染成一屏文本并与基线比对。
fn snapshot(name: &str, messages: Vec<ChatMessage>, width: usize) {
    snapshot_with(name, messages, width, false)
}

fn snapshot_with(name: &str, messages: Vec<ChatMessage>, width: usize, expanded: bool) {
    let mut state = make_state();
    for (idx, mut msg) in messages.into_iter().enumerate() {
        msg.id = idx as u64 + 1;
        state.messages.push(msg);
    }
    state.chat_follow_tail = true;
    // 欢迎页里有版本号与**轮换的 tip 文案**，两者都会让基线在无关改动下变红。
    // 快照只关心消息流的排版，欢迎页另有断言覆盖。
    state.welcome_frozen = true;
    state.output_expanded = expanded;
    let rendered = rendered_text(build_pending_chat_lines(&mut state, width)).join("\n");
    assert_golden(name, rendered.trim_end());
}

fn user(text: &str) -> ChatMessage {
    message(MessageRole::User, text)
}

fn assistant(text: &str) -> ChatMessage {
    message(MessageRole::Assistant, text)
}

/// `display` 形如 `Edit(src/main.rs)`——工具调用行渲染的就是它。
fn tool_call(display: &str, sequence_no: usize) -> ChatMessage {
    let mut msg = message(MessageRole::ToolCall, display);
    msg.sequence_no = Some(sequence_no);
    msg
}

fn tool_result(tool: &str, content: &str, sequence_no: usize, elapsed: f64) -> ChatMessage {
    let mut msg = message(MessageRole::ToolResult, content);
    msg.sequence_no = Some(sequence_no);
    msg.elapsed_secs = Some(elapsed);
    msg.tool_name = Some(tool.to_string());
    msg
}

/// 基础对话形态：用户分隔线 + 前缀、助手 markdown、错误消息。
#[test]
fn golden_conversation_basics() {
    snapshot(
        "conversation_basics",
        vec![
            user("帮我看下这个函数为什么慢"),
            assistant(
                "主要有两个原因：\n\n1. **重复读取**同一份配置\n2. 每次调用都新建连接\n\n> 先看第 1 条。",
            ),
            {
                let mut msg = assistant("模型返回了不完整的结果。");
                msg.is_error = true;
                msg
            },
        ],
        100,
    );
}

/// markdown 元素矩阵：各级标题、两种列表与嵌套、任务列表、右对齐表格、
/// 引用块、分隔线。一帧里塞满，是为了让「排版有没有变」变成可逐字比对的事。
#[test]
fn golden_markdown_elements() {
    snapshot(
        "markdown_elements",
        vec![
            user("渲染测试"),
            assistant(
                "# 一级标题\n\n## 二级标题\n\n### 三级标题\n\n\
                 段落里有 **粗体**、*斜体*、`inline_code` 与 ~~删除线~~。\n\n\
                 - 无序 A\n- 无序 B\n  - 嵌套 B1\n  - 嵌套 B2\n\n\
                 1. 有序一\n2. 有序二\n\n\
                 - [ ] 未完成任务\n- [x] 已完成任务\n\n\
                 | 名称 | 令牌 | 成本 |\n|---|---:|---:|\n\
                 | 输入 | 289,112 | ~$0.0867 |\n| 输出 | 40,440 | ~$0.0485 |\n\n\
                 \n> 引用块第二行 继续\n\n---\n\n结尾段落。",
            ),
        ],
        100,
    );
}

/// 代码块：带语言标签（有行号）/ 无标签（无行号）/ 超宽折行三态。
#[test]
fn golden_code_blocks() {
    snapshot(
        "code_blocks",
        vec![
            user("看这段"),
            assistant(
                "带语言标签：\n\n```rust\nfn main() {\n    let x = 42; // 注释\n    println!(\"hello {x}\");\n}\n```\n\n\
                 无语言标签：\n\n```\nplain text line\nsecond line\n```\n\n\
                 超宽折行：\n\n```text\n\
                 aaaa bbbb cccc dddd eeee ffff gggg hhhh iiii jjjj kkkk llll mmmm nnnn oooo pppp qqqq rrrr\n```",
            ),
        ],
        100,
    );
}

/// 一段较长的 Rust 源码 + 一处改动，用来覆盖 Edit diff 的**折叠态**
/// （默认 20 行正文预算，超出折成「… N 行（Ctrl+O 展开全文）」）。
#[test]
fn golden_edit_diff_collapsed() {
    snapshot(
        "edit_diff_collapsed",
        vec![
            user("改一下"),
            tool_call("Edit(src/main.rs)", 1),
            tool_result("Edit", &numbered_source_diff(), 1, 0.4),
        ],
        100,
    );
}

/// 同一份 diff 的展开态：`Ctrl+O` 之后应为全文，不再有行数上限。
#[test]
fn golden_edit_diff_expanded() {
    snapshot_with(
        "edit_diff_expanded",
        vec![
            user("改一下"),
            tool_call("Edit(src/main.rs)", 1),
            tool_result("Edit", &numbered_source_diff(), 1, 0.4),
        ],
        100,
        true,
    );
}

/// 60 列窄屏：段落 / 表格 / 代码框三者在窄宽度下都不该溢出或错位。
#[test]
fn golden_narrow_terminal() {
    snapshot(
        "narrow_terminal",
        vec![
            user("窄屏也好看吗"),
            assistant(
                "段落文字，用于观察 60 列下的折行行为是否合理。\n\n\
                 | 名称 | 令牌 | 成本 |\n|---|---:|---:|\n| 输入 | 289,112 | ~$0.0867 |\n\n\
                 ```rust\nfn main() { let x = 42; println!(\"hello {x}\"); }\n```",
            ),
        ],
        60,
    );
}

/// 同一段代码在三个宽度下：行号 gutter 宽度与右边线必须都严格对齐。
#[test]
fn golden_width_60_code_block() {
    snapshot("width_60_code_block", width_code_block_messages(), 60);
}

#[test]
fn golden_width_80_code_block() {
    snapshot("width_80_code_block", width_code_block_messages(), 80);
}

#[test]
fn golden_width_120_code_block() {
    snapshot("width_120_code_block", width_code_block_messages(), 120);
}

/// 同一份 diff 在三个宽度下：双列行号与右边线都必须对齐，且不能丢 `+`/`-` 标记。
#[test]
fn golden_width_60_diff() {
    snapshot("width_60_diff", width_diff_messages(), 60);
}

#[test]
fn golden_width_80_diff() {
    snapshot("width_80_diff", width_diff_messages(), 80);
}

#[test]
fn golden_width_120_diff() {
    snapshot("width_120_diff", width_diff_messages(), 120);
}

fn width_code_block_messages() -> Vec<ChatMessage> {
    vec![
        user("宽度敏感"),
        assistant(
            "```rust\nfn compute(input: &str, depth: usize) -> Result<Vec<Token>, ParseError> {\n\
             \x20   let mut out = Vec::with_capacity(input.len()); // 预分配避免反复扩容\n\
             \x20   out\n}\n```",
        ),
    ]
}

/// 用**真实 `make_diff`**（为此 `wyj_tools` 导出了它）而不是手写样本——否则改了
/// 生成端而快照照抄一份过期样本，等于用假数据给自己背书。
fn width_diff_messages() -> Vec<ChatMessage> {
    let old = numbered(1, 40, |n| {
        format!("    let value_{n} = compute(\"payload\", {n})?;")
    });
    let new = numbered(1, 40, |n| {
        if n == 12 {
            "    let value_12 = compute(\"payload\", 12 + 1)?; // 修正 off-by-one".to_string()
        } else {
            format!("    let value_{n} = compute(\"payload\", {n})?;")
        }
    });
    vec![
        user("宽度敏感"),
        tool_call("Edit(src/lib.rs)", 1),
        tool_result("Edit", &wyj_tools::make_diff(&old, &new, 200), 1, 0.4),
    ]
}

/// 一段 50 行的编号源码，两处改动（第 6 行改值、第 41 行改值并追加一行），
/// 中间夹够长的未改动区以便出现 `… N unchanged lines`。
fn numbered_source_diff() -> String {
    let line = |n: usize| format!("    let value_{n} = {n};");
    let old = (1..=50).map(line).collect::<Vec<_>>().join("\n");
    let new = (1..=50)
        .map(|n| match n {
            6 => "    let value_5 = 6; // 修 off-by-one".to_string(),
            41 => "    let value_40 = 41;\n    let extra = 0;".to_string(),
            other => line(other),
        })
        .collect::<Vec<_>>()
        .join("\n");
    wyj_tools::make_diff(&old, &new, 200)
}

fn numbered(from: usize, to: usize, f: impl Fn(usize) -> String) -> String {
    // 前两行是函数签名与空行，于是 `let value_N` 落在第 N+1 行——diff 块头
    // （`@@ -3,7 +3,7 @@`）与双侧行号都以此为基准。
    let mut lines = vec![
        "fn compute(payload: &str) -> Result<T, E> {".to_string(),
        String::new(),
    ];
    lines.extend((from..=to).map(f));
    lines.join("\n")
}
