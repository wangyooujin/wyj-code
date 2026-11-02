//! Bash 输出在进上下文之前的压缩。
//!
//! 两层，都发生在 30KB 头尾截断之前：
//!
//! 1. **每条命令都做**：去掉 ANSI，把 `\r` 进度条收成最终一行，再把连续重复行
//!    收成 `×N`。这几乎不丢信息。
//! 2. **只对单条 `cargo test` / `cargo clippy`**：丢掉编译进度和通过的用例，
//!    留下汇总、失败正文和 rustc 诊断。管道、重定向、`-v`、`--nocapture`、
//!    JSON 格式都不进这一层——模型在要原文。
//!
//! 语义过滤明显变短时，原文经 [`crate::spill::save_full`] 落盘，结果末尾留路径。
//! 滤完不比基线短就整段退回（`never_worse`）。被信号杀死的进程输出不完整，
//! 只走第 1 层。

/// 进程怎么结束的。`Signal` 时不做语义过滤。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExitClass {
    Exited(i32),
    Signal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Test,
    Clippy,
}

/// 省略超过这么多字节才值得附一行 spill 路径；更小的输出提示行本身更贵。
const SPILL_WORTH_BYTES: usize = 2_048;

/// 单次返回里最多展开多少条 rustc 诊断。其余在 spill 原文里。
const MAX_DIAGNOSTICS: usize = 20;

/// 过滤后的 stdout / stderr。空串表示这路没有要给模型看的内容。
pub(crate) fn condense(
    command: &str,
    stdout: &str,
    stderr: &str,
    exit: ExitClass,
) -> (String, String) {
    let cheap_out = cheap(stdout);
    let cheap_err = cheap(stderr);
    let kind = match exit {
        ExitClass::Signal => None,
        ExitClass::Exited(_) => classify(command),
    };
    let Some(kind) = kind else {
        return (cheap_out, cheap_err);
    };
    let success = matches!(exit, ExitClass::Exited(0));
    // 两路一起滤。stderr 上的进度如果单独滤，会变成 “no issues”，
    // 而真正的 warning 还在 stdout 上。
    let baseline = join_streams(&cheap_err, &cheap_out);
    let original = join_streams(stderr, stdout);
    let filtered = apply_semantic(&baseline, &original, kind, success, "bash-cargo");
    if filtered == baseline {
        return (cheap_out, cheap_err);
    }
    if filtered.is_empty() {
        let witness = last_progress_line(&cheap_out, &cheap_err);
        if !witness.is_empty() {
            return (witness, String::new());
        }
    }
    (filtered, String::new())
}

fn join_streams(first: &str, second: &str) -> String {
    if first.is_empty() {
        return second.to_string();
    }
    if second.is_empty() {
        return first.to_string();
    }
    let mut joined = String::with_capacity(first.len() + second.len() + 1);
    joined.push_str(first);
    if !first.ends_with('\n') {
        joined.push('\n');
    }
    joined.push_str(second);
    joined
}

fn apply_semantic(
    baseline: &str,
    original: &str,
    kind: Kind,
    success: bool,
    label: &str,
) -> String {
    let Some(filtered) = semantic(baseline, kind, success) else {
        return baseline.to_string();
    };
    if filtered.len() >= baseline.len() {
        return baseline.to_string();
    }
    with_recovery(filtered, original, label)
}

fn semantic(text: &str, kind: Kind, success: bool) -> Option<String> {
    match kind {
        Kind::Test => filter_test(text, success),
        Kind::Clippy => filter_clippy(text, success),
    }
}

fn cheap(text: &str) -> String {
    collapse_repeats(&apply_cr(&strip_ansi(text)))
}

fn with_recovery(filtered: String, original: &str, label: &str) -> String {
    if filtered.trim().is_empty() {
        return filtered;
    }
    if original.len().saturating_sub(filtered.len()) < SPILL_WORTH_BYTES {
        return filtered;
    }
    let Some(path) = crate::spill::save_full(label, original) else {
        return filtered;
    };
    let hinted = format!(
        "{filtered}\n…[filtered; full output saved to: {}]…",
        path.display()
    );
    if hinted.len() < original.len() {
        hinted
    } else {
        filtered
    }
}

fn last_progress_line(stdout: &str, stderr: &str) -> String {
    let mut last_finished = String::new();
    let mut last = String::new();
    for line in stdout.lines().chain(stderr.lines()) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        last = trimmed.to_string();
        if trimmed.starts_with("Finished") || trimmed.starts_with("error:") {
            last_finished = trimmed.to_string();
        }
    }
    if !last_finished.is_empty() {
        last_finished
    } else {
        last
    }
}

// --- 第 1 层：ANSI / 回车 / 重复行 -----------------------------------------

fn strip_ansi(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        // 转义解析按字节走。若上一段把 UTF-8 首字节吞掉，这里必须先对齐，
        // 否则 `input[start..]` 会在 U+0085 这类双字节字符中间 panic。
        if !input.is_char_boundary(i) {
            i = wyj_core::textutil::ceil_char_boundary(input, i);
            continue;
        }
        if bytes[i] != 0x1b {
            let start = i;
            i += 1;
            while i < bytes.len() && bytes[i] != 0x1b {
                i += 1;
            }
            let end = if input.is_char_boundary(i) {
                i
            } else {
                wyj_core::textutil::floor_char_boundary(input, i)
            };
            if end > start {
                out.push_str(&input[start..end]);
            }
            continue;
        }
        i += 1;
        if i >= bytes.len() {
            break;
        }
        match bytes[i] {
            b'[' => {
                i += 1;
                while i < bytes.len() {
                    let c = bytes[i];
                    // CSI 参数和终止字节都是 ASCII。换行、回车、>= 0x80 都不是
                    // 序列的一部分：残缺的 `ESC [` 不能把后面的本地命令输出吃掉，
                    // 也不能切进 U+0085 这类双字节字符。
                    if c == b'\n' || c == b'\r' || c >= 0x80 {
                        break;
                    }
                    i += 1;
                    if (0x40..=0x7e).contains(&c) {
                        break;
                    }
                }
            }
            b']' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\n' || bytes[i] == b'\r' {
                        break;
                    }
                    if bytes[i] == 0x07 {
                        i += 1;
                        break;
                    }
                    if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            // 双字节转义（ESC M、ESC 7）的第二字节是 ASCII。非 ASCII 只吞掉 ESC。
            c if c < 0x80 => i += 1,
            _ => {}
        }
    }
    out
}

/// `\r` 覆盖同一行时只留最后一段。`\r\n` 仍是换行，不会把整行清空。
fn apply_cr(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for segment in text.split_inclusive('\n') {
        let (body, newline) = segment
            .strip_suffix('\n')
            .map(|body| (body, true))
            .unwrap_or((segment, false));
        let body = body.strip_suffix('\r').unwrap_or(body);
        let shown = match body.rfind('\r') {
            Some(idx) => &body[idx + 1..],
            None => body,
        };
        out.push_str(shown);
        if newline {
            out.push('\n');
        }
    }
    out
}

fn collapse_repeats(text: &str) -> String {
    let mut out = String::new();
    let mut prev: Option<&str> = None;
    let mut count = 0usize;
    for line in text.lines() {
        if prev == Some(line) {
            count += 1;
            continue;
        }
        flush_repeat(&mut out, prev, count);
        prev = Some(line);
        count = 1;
    }
    flush_repeat(&mut out, prev, count);
    // `lines()` 会吃掉末尾换行与否；没有换行的输出（`printf plain`）不能被补上。
    if !text.ends_with('\n') && out.ends_with('\n') {
        out.pop();
    }
    out
}

fn flush_repeat(out: &mut String, line: Option<&str>, count: usize) {
    let Some(line) = line else {
        return;
    };
    if count == 0 {
        return;
    }
    if line.is_empty() {
        out.push('\n');
        return;
    }
    out.push_str(line);
    if count > 1 {
        out.push_str(" ×");
        out.push_str(&count.to_string());
    }
    out.push('\n');
}

// --- 命令识别 --------------------------------------------------------------

fn classify(command: &str) -> Option<Kind> {
    let words = shell_words(command.trim())?;
    let mut i = 0;
    while i < words.len() && is_env_assign(&words[i]) {
        i += 1;
    }
    if words.get(i).map(String::as_str) != Some("cargo") {
        return None;
    }
    i += 1;
    if words.get(i).is_some_and(|word| word.starts_with('+')) {
        i += 1;
    }
    let kind = match words.get(i).map(String::as_str) {
        Some("test") => Kind::Test,
        Some("clippy") => Kind::Clippy,
        _ => return None,
    };
    i += 1;
    if wants_verbatim(&words[i..]) {
        None
    } else {
        Some(kind)
    }
}

/// 引号外出现管道、重定向、替换或未闭合引号时返回 `None`（整段命令不识别）。
fn shell_words(command: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in command.chars() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else {
                cur.push(c);
            }
            continue;
        }
        match c {
            '\'' | '"' => quote = Some(c),
            ' ' | '\t' => {
                if !cur.is_empty() {
                    words.push(std::mem::take(&mut cur));
                }
            }
            '\\' | '$' | '`' | '|' | '&' | ';' | '<' | '>' | '(' | ')' | '\n' | '\r' => {
                return None;
            }
            _ => cur.push(c),
        }
    }
    if quote.is_some() {
        return None;
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    Some(words)
}

fn is_env_assign(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// 模型显式要了更详细或非人类格式的输出。
fn wants_verbatim(args: &[String]) -> bool {
    let mut after_dd = false;
    for arg in args {
        if arg == "--" {
            after_dd = true;
            continue;
        }
        if !after_dd {
            if is_verbose_flag(arg)
                || arg == "--nocapture"
                || arg == "--message-format"
                || arg.starts_with("--message-format=")
                || arg.starts_with("-Z")
            {
                return true;
            }
        } else if arg == "--nocapture"
            || is_verbose_flag(arg)
            || arg == "--format"
            || arg.starts_with("--format=")
        {
            return true;
        }
    }
    false
}

fn is_verbose_flag(arg: &str) -> bool {
    arg == "-v"
        || arg == "--verbose"
        || arg.starts_with("--verbose=")
        || (arg.len() >= 3 && arg.starts_with('-') && arg.chars().skip(1).all(|c| c == 'v'))
}

// --- cargo test / clippy ---------------------------------------------------

#[derive(Debug)]
struct Parsed {
    diags: Vec<Diag>,
    summaries: Vec<String>,
    test_results: Vec<String>,
    failed_tests: Vec<String>,
    failure_section: Vec<String>,
    /// 除了进度、空行和上面几类，没有认不出的行。
    recognized: bool,
}

#[derive(Debug)]
struct Diag {
    error: bool,
    lines: Vec<String>,
}

struct Suite {
    ok: bool,
    passed: u64,
    ignored: u64,
    filtered: u64,
    duration: Option<f64>,
}

fn filter_test(text: &str, success: bool) -> Option<String> {
    if text.trim().is_empty() {
        return None;
    }
    let parsed = parse_cargo(text)?;
    if !parsed.recognized {
        return None;
    }
    let rendered = render_test(&parsed, success)?;
    Some(rendered)
}

fn filter_clippy(text: &str, success: bool) -> Option<String> {
    if text.trim().is_empty() {
        return if success {
            Some("cargo clippy: no issues".to_string())
        } else {
            None
        };
    }
    let parsed = parse_cargo(text)?;
    if !parsed.recognized {
        return None;
    }
    if parsed.diags.is_empty() && parsed.summaries.is_empty() {
        return if success {
            Some("cargo clippy: no issues".to_string())
        } else {
            None
        };
    }
    Some(render_diags(
        "cargo clippy",
        &parsed.diags,
        &parsed.summaries,
    ))
}

fn render_test(parsed: &Parsed, success: bool) -> Option<String> {
    let all_ok = !parsed.test_results.is_empty()
        && parsed.failed_tests.is_empty()
        && parsed.failure_section.is_empty()
        && parsed
            .test_results
            .iter()
            .all(|line| parse_suite(line).is_some_and(|suite| suite.ok))
        && parsed.diags.iter().all(|diag| !diag.error);

    if all_ok && success {
        let summary =
            aggregate(&parsed.test_results).unwrap_or_else(|| parsed.test_results.join("\n"));
        if parsed.diags.is_empty() && parsed.summaries.is_empty() {
            return Some(summary);
        }
        let mut out = summary;
        out.push('\n');
        out.push_str(&render_diags("warnings", &parsed.diags, &parsed.summaries));
        return Some(out);
    }

    // 退出码非零，但这路输出看起来全是通过：失败原因在另一路或没被认出来。
    if !success && all_ok && parsed.diags.is_empty() && parsed.summaries.is_empty() {
        return None;
    }

    let progress_only = parsed.test_results.is_empty()
        && parsed.diags.is_empty()
        && parsed.summaries.is_empty()
        && parsed.failed_tests.is_empty()
        && parsed.failure_section.is_empty();
    if progress_only {
        return Some(String::new());
    }

    let mut lines = Vec::new();
    if !parsed.diags.is_empty() {
        // 不加「N errors」抬头：编译错误正文本身已经说明问题，多一行会让
        // 只有一条进度的短日志滤完反而更长，从而整段退回原文。
        lines.push(render_diag_blocks(&parsed.diags));
    }
    push_block(&mut lines, &parsed.failure_section);
    for name in &parsed.failed_tests {
        if !parsed.failure_section.iter().any(|line| line == name) {
            lines.push(name.clone());
        }
    }
    lines.extend(parsed.test_results.iter().cloned());
    lines.extend(parsed.summaries.iter().cloned());
    let rendered = lines.join("\n");
    if rendered.trim().is_empty() {
        None
    } else {
        Some(rendered)
    }
}

fn push_block(lines: &mut Vec<String>, block: &[String]) {
    if block.is_empty() {
        return;
    }
    if !lines.is_empty() {
        lines.push(String::new());
    }
    lines.extend(block.iter().cloned());
}

fn aggregate(results: &[String]) -> Option<String> {
    let mut passed = 0u64;
    let mut ignored = 0u64;
    let mut filtered = 0u64;
    let mut suites = 0u64;
    let mut duration = 0.0f64;
    let mut have_duration = true;
    for line in results {
        let suite = parse_suite(line)?;
        if !suite.ok {
            return None;
        }
        passed += suite.passed;
        ignored += suite.ignored;
        filtered += suite.filtered;
        suites += 1;
        match suite.duration {
            Some(secs) => duration += secs,
            None => have_duration = false,
        }
    }
    if suites == 0 {
        return None;
    }
    let mut parts = vec![format!("{passed} passed")];
    if ignored > 0 {
        parts.push(format!("{ignored} ignored"));
    }
    if filtered > 0 {
        parts.push(format!("{filtered} filtered out"));
    }
    let suites_label = if suites == 1 {
        "1 suite".to_string()
    } else {
        format!("{suites} suites")
    };
    let body = parts.join(", ");
    if have_duration {
        Some(format!(
            "cargo test: {body} ({suites_label}, {duration:.2}s)"
        ))
    } else {
        Some(format!("cargo test: {body} ({suites_label})"))
    }
}

fn parse_suite(line: &str) -> Option<Suite> {
    let rest = line.trim().strip_prefix("test result: ")?;
    let (status, rest) = rest.split_once(". ")?;
    let mut suite = Suite {
        ok: status == "ok",
        passed: 0,
        ignored: 0,
        filtered: 0,
        duration: None,
    };
    let mut saw_passed = false;
    for part in rest.split(';') {
        let part = part.trim();
        if let Some(n) = part.strip_suffix(" passed") {
            suite.passed = n.trim().parse().ok()?;
            saw_passed = true;
        } else if let Some(n) = part.strip_suffix(" ignored") {
            suite.ignored = n.trim().parse().ok()?;
        } else if let Some(n) = part.strip_suffix(" filtered out") {
            suite.filtered = n.trim().parse().ok()?;
        } else if let Some(rest) = part.strip_prefix("finished in ") {
            let num = rest.strip_suffix('s')?;
            suite.duration = num.parse().ok();
        }
    }
    if !saw_passed {
        return None;
    }
    Some(suite)
}

fn render_diags(header: &str, diags: &[Diag], summaries: &[String]) -> String {
    let errors = diags.iter().filter(|diag| diag.error).count();
    let warnings = diags.len() - errors;
    let mut out = format!("{header}: {errors} errors, {warnings} warnings\n");
    if !diags.is_empty() {
        out.push('\n');
        out.push_str(&render_diag_blocks(diags));
        out.push('\n');
    }
    for line in summaries {
        out.push_str(line);
        out.push('\n');
    }
    out.trim_end().to_string()
}

fn render_diag_blocks(diags: &[Diag]) -> String {
    let mut out = String::new();
    for diag in diags.iter().take(MAX_DIAGNOSTICS) {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&diag.lines.join("\n"));
    }
    if diags.len() > MAX_DIAGNOSTICS {
        out.push_str(&format!(
            "\n… +{} more diagnostics omitted",
            diags.len() - MAX_DIAGNOSTICS
        ));
    }
    out
}

fn parse_cargo(text: &str) -> Option<Parsed> {
    let mut diags = Vec::new();
    let mut summaries = Vec::new();
    let mut test_results = Vec::new();
    let mut failed_tests = Vec::new();
    let mut failure_section = Vec::new();
    let mut recognized = true;
    let mut in_failure = false;
    let mut current: Option<Diag> = None;

    let flush_diag = |current: &mut Option<Diag>, diags: &mut Vec<Diag>| {
        if let Some(diag) = current.take() {
            diags.push(diag);
        }
    };

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if in_failure {
                if failure_section
                    .last()
                    .is_some_and(|prev: &String| !prev.is_empty())
                {
                    failure_section.push(String::new());
                }
            } else {
                flush_diag(&mut current, &mut diags);
            }
            continue;
        }
        if is_progress(trimmed) || is_running_count(trimmed) {
            flush_diag(&mut current, &mut diags);
            continue;
        }
        if is_summary_line(trimmed) {
            flush_diag(&mut current, &mut diags);
            in_failure = false;
            summaries.push(trimmed.to_string());
            continue;
        }
        if let Some(status) = harness_status(trimmed) {
            flush_diag(&mut current, &mut diags);
            if status == HarnessStatus::Failed {
                failed_tests.push(trimmed.to_string());
            }
            continue;
        }
        if trimmed == "failures:" {
            flush_diag(&mut current, &mut diags);
            in_failure = true;
            failure_section.push(trimmed.to_string());
            continue;
        }
        if trimmed.starts_with("test result:") {
            flush_diag(&mut current, &mut diags);
            in_failure = false;
            test_results.push(trimmed.to_string());
            continue;
        }
        if in_failure {
            failure_section.push(line.trim_end().to_string());
            continue;
        }
        if is_rustc_footer(trimmed) {
            flush_diag(&mut current, &mut diags);
            continue;
        }
        if is_diag_start(trimmed) {
            flush_diag(&mut current, &mut diags);
            current = Some(Diag {
                error: trimmed.starts_with("error"),
                lines: vec![line.trim_end().to_string()],
            });
            continue;
        }
        // 诊断内部的源码行（`1 | ...`）没有统一前缀，空行才结束。
        if let Some(diag) = current.as_mut() {
            diag.lines.push(line.trim_end().to_string());
            continue;
        }
        recognized = false;
    }
    flush_diag(&mut current, &mut diags);

    if !recognized {
        return None;
    }
    Some(Parsed {
        diags,
        summaries,
        test_results,
        failed_tests,
        failure_section,
        recognized,
    })
}

fn is_progress(trimmed: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "Compiling",
        "Checking",
        "Downloading",
        "Downloaded",
        "Finished",
        "Blocking",
        "Updating",
        "Locking",
        "Documenting",
        "Building",
        "Created",
        "Fresh",
        "Dirty",
        "Running ",
        "Doc-tests ",
    ];
    PREFIXES.iter().any(|prefix| trimmed.starts_with(prefix))
}

fn is_running_count(trimmed: &str) -> bool {
    let Some(rest) = trimmed.strip_prefix("running ") else {
        return false;
    };
    let mut parts = rest.split_whitespace();
    let count = parts.next().and_then(|n| n.parse::<u64>().ok());
    let word = parts.next();
    count.is_some() && matches!(word, Some("test") | Some("tests")) && parts.next().is_none()
}

fn is_summary_line(trimmed: &str) -> bool {
    trimmed.starts_with("error: could not compile")
        || trimmed.starts_with("error: test failed")
        || trimmed.starts_with("error: aborting")
        || trimmed.contains("aborting due to")
        || (trimmed.contains(" generated ")
            && (trimmed.starts_with("warning:") || trimmed.starts_with("error:")))
}

fn is_diag_start(trimmed: &str) -> bool {
    (trimmed.starts_with("error:")
        || trimmed.starts_with("error[")
        || trimmed.starts_with("warning:")
        || trimmed.starts_with("warning["))
        && !is_summary_line(trimmed)
}

fn is_rustc_footer(trimmed: &str) -> bool {
    trimmed.starts_with("For more information about this error")
        || trimmed.starts_with("Some errors have detailed explanations")
        || trimmed.starts_with("To learn more, run the command again")
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HarnessStatus {
    Ok,
    Ignored,
    Failed,
}

fn harness_status(trimmed: &str) -> Option<HarnessStatus> {
    let rest = trimmed.strip_prefix("test ")?;
    if rest.ends_with("... ok") {
        Some(HarnessStatus::Ok)
    } else if rest.ends_with("... FAILED") {
        Some(HarnessStatus::Failed)
    } else if rest.contains("... ignored") {
        Some(HarnessStatus::Ignored)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_ansi_and_keeps_text() {
        let raw = "\u{1b}[31mred\u{1b}[0m plain";
        assert_eq!(strip_ansi(raw), "red plain");
    }

    /// 旁路由 SSH 输出里出现过 `ESC` 紧挨 U+0085（UTF-8 是 `C2 85`）。
    /// 旧实现把 ESC 后的一个字节当成转义第二字节，切在 `85` 上，
    /// `input[start..]` 直接 panic，整轮任务被掐断。
    #[test]
    fn esc_before_nel_does_not_slice_inside_the_character() {
        let mut raw = String::from("rule ");
        raw.push('\u{1b}');
        raw.push('\u{85}');
        raw.push_str("direct");
        assert_eq!(strip_ansi(&raw), "rule \u{85}direct");
    }

    #[test]
    fn broken_csi_does_not_eat_the_following_utf8() {
        assert_eq!(strip_ansi("\u{1b}[旁路由"), "旁路由");
    }

    /// 没写完的 OSC / CSI 必须在换行处停。否则本地 `cat` / `git show` 里一个
    /// 孤立的 `ESC ]` 会把这一行之后的全部输出吞掉。
    #[test]
    fn unterminated_escape_stops_at_newline() {
        assert_eq!(strip_ansi("\u{1b}]0;title\nkeep me\n"), "\nkeep me\n");
        assert_eq!(strip_ansi("\u{1b}[32\nkeep me\n"), "\nkeep me\n");
    }

    /// 每条本地命令都会走 `cheap`。ESC 后面接上 Latin-1（含上次 panic 的
    /// U+0085）、中文和 emoji，确认不会再在字符中间切片。
    #[test]
    fn cheap_filter_does_not_panic_on_local_command_bytes() {
        let samples = [
            "plain",
            "\u{1b}[31mred\u{1b}[0m",
            "\u{1b}]8;;file:///tmp/a\u{1b}\\link\u{1b}]8;;\u{1b}\\",
            "进度\r完成\n下一行",
            "同一行\n同一行\n同一行\n",
            "中文\u{1b}尾",
        ];
        for sample in samples {
            let _ = cheap(sample);
            let _ = condense("ls -la", sample, sample, ExitClass::Exited(0));
        }
        let mut prefixed = String::new();
        for ch in '\0'..='\u{FF}' {
            prefixed.push('\u{1b}');
            prefixed.push(ch);
            prefixed.push('\n');
        }
        prefixed.push('\u{1b}');
        prefixed.push('中');
        prefixed.push('\u{1b}');
        prefixed.push('🦀');
        prefixed.push_str("\n\u{1b}[\n尾部还在\n\u{1b}]0;x\r尾部也在\n");
        let out = cheap(&prefixed);
        assert!(out.contains("尾部还在"), "{out}");
        assert!(out.contains("尾部也在"), "{out}");
    }

    #[test]
    fn carriage_return_keeps_the_final_segment() {
        assert_eq!(apply_cr("10%\r20%\r100%\nnext\n"), "100%\nnext\n");
        assert_eq!(apply_cr("line\r\n"), "line\n");
    }

    #[test]
    fn consecutive_duplicates_collapse() {
        assert_eq!(collapse_repeats("a\na\na\nb\n\n\n"), "a ×3\nb\n\n");
        assert_eq!(collapse_repeats("plain"), "plain");
        assert_eq!(collapse_repeats("a\na"), "a ×2");
    }

    #[test]
    fn classify_accepts_toolchain_and_env_prefix() {
        assert_eq!(
            classify("RUST_BACKTRACE=1 cargo +nightly test -p foo -- --test-threads=1"),
            Some(Kind::Test)
        );
        assert_eq!(
            classify("cargo clippy -p wyj-tools -- -D warnings"),
            Some(Kind::Clippy)
        );
    }

    #[test]
    fn classify_rejects_pipelines_verbatim_flags_and_quotes() {
        assert_eq!(classify("cargo test | tail"), None);
        assert_eq!(classify("cargo test && cargo clippy"), None);
        assert_eq!(classify("cargo test 2>&1"), None);
        assert_eq!(classify("cargo test -v"), None);
        assert_eq!(classify("cargo test -- --nocapture"), None);
        assert_eq!(classify("cargo test --message-format=json"), None);
        assert_eq!(classify("cargo clippy -- -v"), None);
        assert_eq!(classify("echo cargo test"), None);
        assert_eq!(classify("cargo build"), None);
    }

    #[test]
    fn successful_tests_become_one_line_and_drop_progress() {
        let stdout = "\
running 2 tests
test alpha ... ok
test beta ... ok

test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.02s
";
        let stderr = "\
   Compiling demo v0.1.0
    Finished `test` profile [unoptimized + debuginfo] target(s) in 1.20s
";
        let (out, err) = condense("cargo test -p demo", stdout, stderr, ExitClass::Exited(0));
        assert_eq!(out, "cargo test: 2 passed (1 suite, 0.02s)");
        assert_eq!(err, "");
    }

    #[test]
    fn failed_tests_keep_the_failure_body() {
        let stdout = "\
running 2 tests
test alpha ... ok
test beta ... FAILED

failures:

---- beta stdout ----
assertion `left == right` failed
  left: 1
 right: 2

failures:
    beta

test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
";
        let (out, _) = condense("cargo test", stdout, "", ExitClass::Exited(101));
        assert!(out.contains("assertion `left == right` failed"), "{out}");
        assert!(out.contains("test result: FAILED"), "{out}");
        assert!(!out.contains("test alpha ... ok"), "{out}");
        assert!(!out.contains("running 2 tests"), "{out}");
    }

    #[test]
    fn compile_errors_drop_compiling_lines() {
        let stderr = "\
   Compiling demo v0.1.0
error[E0425]: cannot find value `missing`
 --> src/lib.rs:1:5
  |
1 |     missing
  |     ^^^^^^^ not found in this scope

error: could not compile `demo` (lib test) due to 1 previous error
";
        let (out, err) = condense("cargo test --lib", "", stderr, ExitClass::Exited(101));
        assert_eq!(err, "");
        assert!(out.contains("cannot find value `missing`"), "{out}");
        assert!(out.contains("could not compile"), "{out}");
        assert!(!out.contains("Compiling"), "{out}");
    }

    #[test]
    fn clippy_without_diagnostics_is_one_line() {
        let stderr = "    Checking demo v0.1.0\n    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.40s\n";
        let (out, err) = condense("cargo clippy", "", stderr, ExitClass::Exited(0));
        assert_eq!(err, "");
        assert_eq!(out, "cargo clippy: no issues");
    }

    #[test]
    fn clippy_progress_on_stderr_does_not_hide_warnings() {
        let stdout = "\
warning: unused variable: `x`
 --> src/lib.rs:1:5
  |
1 |     let x = 1;
  |         ^
";
        let stderr = "    Checking demo v0.1.0\n    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.10s\n";
        let (out, err) = condense("cargo clippy", stdout, stderr, ExitClass::Exited(0));
        assert_eq!(err, "");
        assert!(out.contains("unused variable"), "{out}");
        assert!(!out.contains("no issues"), "{out}");
        assert!(!out.contains("Checking"), "{out}");
    }

    #[test]
    fn clippy_keeps_warning_blocks() {
        let stdout = "\
    Checking demo v0.1.0
warning: unused variable: `x`
 --> src/lib.rs:2:9
  |
2 |     let x = 1;
  |         ^ help: if this is intentional, prefix it with an underscore: `_x`
  |
  = note: `#[warn(unused_variables)]` on by default

warning: `demo` (bin \"demo\") generated 1 warning
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.20s
";
        let (out, _) = condense(
            "cargo clippy -- -D warnings",
            stdout,
            "",
            ExitClass::Exited(0),
        );
        assert!(out.contains("unused variable"), "{out}");
        assert!(out.contains("src/lib.rs:2:9"), "{out}");
        assert!(!out.contains("Checking"), "{out}");
        assert!(!out.contains("Finished"), "{out}");
    }

    #[test]
    fn verbose_and_signal_keep_passing_tests() {
        let stdout = "test alpha ... ok\ntest alpha ... ok\n";
        let (verbose, _) = condense("cargo test -v", stdout, "", ExitClass::Exited(0));
        assert!(verbose.contains("test alpha ... ok ×2"), "{verbose}");
        let (signaled, _) = condense("cargo test", stdout, "", ExitClass::Signal);
        assert!(signaled.contains("test alpha ... ok ×2"), "{signaled}");
    }

    #[test]
    fn filter_never_emits_more_than_the_raw_stream() {
        // 空输出上再写 "no issues" 比原文更长，两路都保持为空。
        let (out, err) = condense("cargo clippy", "", "", ExitClass::Exited(0));
        assert_eq!(out, "");
        assert_eq!(err, "");
    }

    #[test]
    fn two_suites_sum_their_counts() {
        let stdout = "\
running 1 test
test a ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s

running 2 tests
test b ... ok
test c ... ignored

test result: ok. 1 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.03s
";
        let (out, _) = condense("cargo test", stdout, "", ExitClass::Exited(0));
        assert_eq!(out, "cargo test: 2 passed, 1 ignored (2 suites, 0.04s)");
    }
}
