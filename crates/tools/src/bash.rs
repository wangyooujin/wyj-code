//! Bash 工具 — 在 shell 中执行命令

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt};
use wyj_api::types::ToolDefinition;
use wyj_core::tool::{Tool, ToolCallMeta, ToolContext, ToolOutputChunk, ToolResult};

use crate::textutil::truncate_head_tail;

const MAX_OUTPUT: usize = 30_000; // 输出截断阈值（字符）
const TIMEOUT_SECS: u64 = 120;

/// 实时输出快照的最小推送间隔。stdout / stderr 两个 pump 共享同一闸门，
/// 因此总速率上限是 1/INTERVAL 而不是两倍。
const STREAM_INTERVAL: Duration = Duration::from_millis(100);

/// 单次快照携带的字节数（stdout 与 stderr 各自保尾这么多）。
///
/// 快照只用于**运行中**的那块实时视图；工具跑完返回给模型的正文仍走
/// `MAX_OUTPUT` 的截断预算，两者互不影响。定 8KB 是因为实时视图最终只画
/// 最后 [`LIVE_TAIL_LINES`] 行（render 侧），再多纯属搬运。
const STREAM_TAIL_BYTES: usize = 8 * 1024;

/// pump 与 `child.wait()` 汇合后的收尾等待上限。正常路径下管道 EOF 与进程
/// 退出几乎同时到达，这个上限只在子进程留下孤儿写 pipe 的畸形情况下生效，
/// 避免工具 future 永久挂住。
const DRAIN_GRACE: Duration = Duration::from_secs(2);

pub struct BashTool;

/// 实时推送的节流闸门 + 两路输出的累计缓冲。
///
/// 两个 pump 共享一份「上次推送时刻 + 当时的累计字节数」：间隔没到、或这两
/// 毫秒内管道没吐出新字节就跳过本次推送。比较**累计字节数**而非快照长度是
/// 必需的——输出超过 `STREAM_TAIL_BYTES` 后快照长度会钉死在上限不变，只比
/// 长度会让持续刷新的命令（`cargo build`）视图冻结在半途中。持锁期间不做
/// await，也不做字符串拷贝。
struct OutputBuffers {
    stdout: Mutex<Vec<u8>>,
    stderr: Mutex<Vec<u8>>,
    /// `(上次推送时刻, 当时的累计字节数)`；`None` = 还没推过。
    throttle: Mutex<Option<(Instant, u64)>>,
}

impl OutputBuffers {
    fn new() -> Self {
        Self {
            stdout: Mutex::new(Vec::new()),
            stderr: Mutex::new(Vec::new()),
            throttle: Mutex::new(None),
        }
    }

    /// 累计字节总数（单调递增，作为「有没有新输出」的判据）。
    fn total(&self) -> u64 {
        self.stdout.lock().unwrap().len() as u64 + self.stderr.lock().unwrap().len() as u64
    }

    fn should_send(&self, total: u64) -> bool {
        let mut guard = self.throttle.lock().unwrap();
        if let Some((last_at, last_total)) = *guard {
            if total == last_total || last_at.elapsed() < STREAM_INTERVAL {
                return false;
            }
        }
        *guard = Some((Instant::now(), total));
        true
    }
}

/// 取缓冲区尾部最多 `max` 字节，并把起点回退到 UTF-8 字符边界。
///
/// 不能直接 `String::from_utf8_lossy(&bytes[len - max..])`：切在多字节字符
/// 中间会产出一串 U+FFFD 替换符（中文/emoji 高发）。判据是 UTF-8 的后继字节
/// 形如 `0b10xxxxxx`——从后往前跳过它们，停下来必然是首字节。
fn utf8_tail(bytes: &[u8], max: usize) -> &[u8] {
    if bytes.len() <= max {
        return bytes;
    }
    let mut start = bytes.len() - max;
    while start < bytes.len() && (bytes[start] & 0xC0) == 0x80 {
        start += 1;
    }
    &bytes[start..]
}

/// 合并 stdout / stderr 尾部成一份快照文本。
///
/// 保留「两路都有内容时用分隔行隔开」的写法，与跑完后回给模型的正文格式
/// 一致，避免用户先在实时视图里看到混作一坨的输出、结束后又被重新分段。
fn merge_tail(stdout: &[u8], stderr: &[u8]) -> String {
    let out = String::from_utf8_lossy(utf8_tail(stdout, STREAM_TAIL_BYTES)).into_owned();
    let err = String::from_utf8_lossy(utf8_tail(stderr, STREAM_TAIL_BYTES)).into_owned();
    match (out.is_empty(), err.is_empty()) {
        (true, true) => String::new(),
        (false, true) => out,
        (true, false) => err,
        (false, false) => format!("{out}\n--- stderr ---\n{err}"),
    }
}

/// 从一条管道持续抽字节写进 `bufs`，并在节流允许时把尾部快照推给 UI。
///
/// `on_stdout` 区分两路：快照需要交叉读另一条缓冲，所以 stdout pump 要能
/// 看到 stderr 的内容，反之同理。
///
/// 抽字节与推快照合流在这一个循环里，而不是另起一个 ticker task：后者在回合
/// 被 ESC 中断（`h.abort()` 掉回合 future）时会变成孤儿，永远按周期往 TUI 推
/// 陈旧数据。前者的生命周期锚在管道 EOF 上——命令结束，泵自然结束。
async fn pump<R>(
    mut reader: R,
    bufs: &OutputBuffers,
    on_stdout: bool,
    tx: Option<&tokio::sync::mpsc::Sender<ToolOutputChunk>>,
    tool_use_id: &str,
) where
    R: AsyncRead + Unpin,
{
    let mut buf = [0u8; 8192];
    loop {
        let n = match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if on_stdout {
            bufs.stdout.lock().unwrap().extend_from_slice(&buf[..n]);
        } else {
            bufs.stderr.lock().unwrap().extend_from_slice(&buf[..n]);
        }

        // 无接收端（headless / 子 Agent）时只管抽字节，快照是纯开销。
        let Some(tx) = tx else { continue };
        let total = bufs.total();
        if !bufs.should_send(total) {
            continue;
        }
        let snapshot = merge_tail(&bufs.stdout.lock().unwrap(), &bufs.stderr.lock().unwrap());
        if snapshot.is_empty() {
            continue;
        }
        // try_send 而非 send：通道满时直接丢弃这次快照。快照语义下这是安全的
        // ——下一帧仍然是完整、自洽的累计输出，不会像增量那样留下数据缺口。
        // 绝不能改成 send/await：工具 future 被 drop 时会连带把工具挂死。
        let _ = tx.try_send(ToolOutputChunk {
            tool_use_id: tool_use_id.to_string(),
            tail: snapshot,
        });
    }
}

#[derive(Deserialize)]
struct Input {
    command: String,
    #[serde(default)]
    timeout: Option<u64>,
    #[serde(default)]
    run_in_background: bool,
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "Bash"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: crate::descriptions::BASH.to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": crate::descriptions::FIELD_BASH_COMMAND
                    },
                    "timeout": {
                        "type": "integer",
                        "description": crate::descriptions::FIELD_BASH_TIMEOUT,
                        "minimum": 1,
                        "maximum": 600
                    },
                    "description": {
                        "type": "string",
                        "description": crate::descriptions::FIELD_BASH_DESCRIPTION
                    },
                    "run_in_background": {
                        "type": "boolean",
                        "description": "Run in the background and return a shell id immediately; read output later with BashOutput and stop it with KillShell. Use for long-running processes like dev servers or watchers (default false)"
                    }
                },
                "required": ["command"]
            }),
            native: None,
        }
    }

    fn needs_permission(&self, _input: &Value) -> bool {
        true
    }

    fn action_summary(&self, input: &Value) -> String {
        input
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    }

    async fn run(&self, input: Value, ctx: &dyn ToolContext) -> Result<ToolResult> {
        self.exec(input, ctx, None).await
    }

    /// 拿 `tool_use_id` 换取实时输出快照。`Agent` 一律走本入口（`run_tool`），
    /// 直调 `run` 的测试路径退化成无流式，结果内容完全一致。
    async fn run_with_meta(
        &self,
        input: Value,
        ctx: &dyn ToolContext,
        meta: &ToolCallMeta,
    ) -> Result<ToolResult> {
        self.exec(input, ctx, Some(meta.tool_use_id.as_str())).await
    }
}

impl BashTool {
    async fn exec(
        &self,
        input: Value,
        ctx: &dyn ToolContext,
        stream_id: Option<&str>,
    ) -> Result<ToolResult> {
        let inp: Input = serde_json::from_value(input)?;

        // 后台执行：立即返回任务 id，输出经 BashOutput 增量读取
        if inp.run_in_background {
            let manager = crate::bash_session::BashSessionManager::global();
            return match manager.spawn(&inp.command, ctx.cwd()) {
                Ok(id) => Ok(ToolResult::ok(format!(
                    "Started background shell {id}. Use BashOutput to read its output and KillShell to stop it."
                ))),
                Err(e) => Ok(ToolResult::err(format!("后台启动失败: {e}"))),
            };
        }

        let timeout = inp.timeout.unwrap_or(TIMEOUT_SECS);
        let mut command = tokio::process::Command::new("/bin/bash");
        command
            .arg("-c")
            .arg(&inp.command)
            .current_dir(ctx.cwd())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        #[cfg(unix)]
        command.process_group(0);

        // 实时推送只在「能拿到调用 id」且「调用方确实挂了接收端」时启用。
        // headless / 子 Agent / ACP 走无推送路径，结果内容与本特性引入前一致。
        let stream_tx: Option<(tokio::sync::mpsc::Sender<ToolOutputChunk>, String)> =
            stream_id.and_then(|id| ctx.tool_output().map(|tx| (tx.clone(), id.to_string())));

        let mut child = match command.spawn() {
            Ok(c) => c,
            Err(e) => return Ok(ToolResult::err(format!("执行失败: {e}"))),
        };
        let pgid = child.id().unwrap_or(0);

        // 管道必须自己抽：`wait()` 不读管道，子进程写满 64KB pipe buffer 后
        // 会与这里互等，死锁。这条与是否推送实时输出无关，两条路径都需要。
        let bufs = Arc::new(OutputBuffers::new());
        let mut pumps = Vec::new();
        let streams: Vec<(Option<Box<dyn AsyncRead + Send + Unpin>>, bool)> = vec![
            (
                child
                    .stdout
                    .take()
                    .map(|r| Box::new(r) as Box<dyn AsyncRead + Send + Unpin>),
                true,
            ),
            (
                child
                    .stderr
                    .take()
                    .map(|r| Box::new(r) as Box<dyn AsyncRead + Send + Unpin>),
                false,
            ),
        ];
        for (reader, on_stdout) in streams
            .into_iter()
            .filter_map(|(reader, on_stdout)| reader.map(|r| (r, on_stdout)))
        {
            let bufs = Arc::clone(&bufs);
            let (tx, id) = match &stream_tx {
                Some((tx, id)) => (Some(tx.clone()), id.clone()),
                None => (None, String::new()),
            };
            pumps.push(tokio::spawn(async move {
                pump(reader, &bufs, on_stdout, tx.as_ref(), &id).await;
            }));
        }

        let waited = tokio::time::timeout(Duration::from_secs(timeout), child.wait()).await;

        // 超时后必须杀掉整个进程组：只放弃等待的话，`bash -c` 及其子进程会
        // 继续在后台跑完，而模型和用户都以为这条命令已经结束了。TERM → 2s →
        // KILL 的三段式与 `bash_session::kill` 一致。
        if waited.is_err() {
            crate::bash_session::signal_group(pgid, "TERM");
            if tokio::time::timeout(Duration::from_secs(2), child.wait())
                .await
                .is_err()
            {
                crate::bash_session::signal_group(pgid, "KILL");
                let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
            }
        }

        // 汇合 pump：EOF 早于 `wait()` 返回，但两者的先后没有硬保证，给一个
        // 兜底上限，绝不让工具 future 因为残留写端永久挂住。
        let drained = async {
            for p in pumps {
                let _ = p.await;
            }
        };
        let _ = tokio::time::timeout(DRAIN_GRACE, drained).await;

        let status = match waited {
            Err(_) => {
                return Ok(ToolResult::err(format!(
                    "命令超时（{}s）: {}",
                    timeout, inp.command
                )));
            }
            Ok(Err(e)) => return Ok(ToolResult::err(format!("执行失败: {e}"))),
            Ok(Ok(st)) => st,
        };

        let stdout = String::from_utf8_lossy(&bufs.stdout.lock().unwrap()).into_owned();
        let stderr = String::from_utf8_lossy(&bufs.stderr.lock().unwrap()).into_owned();
        let mut result = String::new();

        // stdout/stderr 共享同一截断预算：先到先得，避免一路很长时
        // 另一路为空而浪费一半配额。保头 60% + 尾 40%（报错几乎总在尾部）。
        let stderr_budget = if stdout.is_empty() {
            MAX_OUTPUT
        } else {
            MAX_OUTPUT.saturating_sub(stdout.len()).max(MAX_OUTPUT / 2)
        };
        let stdout_budget = if stderr.is_empty() {
            MAX_OUTPUT
        } else {
            MAX_OUTPUT.saturating_sub(stderr.len()).max(MAX_OUTPUT / 2)
        };
        if !stdout.is_empty() {
            result.push_str(&truncate_head_tail(
                &stdout,
                stdout_budget * 6 / 10,
                stdout_budget * 4 / 10,
            ));
        }
        if !stderr.is_empty() {
            if !result.is_empty() {
                result.push_str("\n--- stderr ---\n");
            }
            result.push_str(&truncate_head_tail(
                &stderr,
                stderr_budget * 6 / 10,
                stderr_budget * 4 / 10,
            ));
        }
        if result.is_empty() {
            result.push_str("（无输出）");
        }

        let is_error = !status.success();
        if is_error {
            // `code()` 为 None = 被信号杀死（SIGKILL / SIGTERM），不是「退出码 -1」。
            // 报成 -1 会让模型以为这是一次普通的非零退出从而直接重跑命令——而
            // 被信号打断恰恰是**状态不明**的典型：`rm -rf` 杀在 `rm` 之后、
            // 数据库迁移杀在写了一半、测试杀在临时文件已生成之后。重跑一次可能
            // 把半完成状态变成两遍。这里明确说出来，让模型先探查现状再决定。
            result = match status.code() {
                Some(code) => format!("退出码 {code}\n{result}"),
                None => format!(
                    "命令被信号终止（可能已产生部分副作用——先检查当前状态再决定是否重跑）\n{result}"
                ),
            };
        }
        Ok(ToolResult {
            content: result,
            is_error,
            parts: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    struct TestContext {
        cwd: PathBuf,
        /// 挂上后即启用前台 Bash 的实时输出推送（真实 TUI 形态）。
        tool_output_tx: Option<tokio::sync::mpsc::Sender<ToolOutputChunk>>,
    }

    impl TestContext {
        fn plain(cwd: PathBuf) -> Self {
            Self {
                cwd,
                tool_output_tx: None,
            }
        }
    }

    #[async_trait]
    impl ToolContext for TestContext {
        fn cwd(&self) -> &Path {
            &self.cwd
        }

        fn is_allowed(&self, _name: &str, _input: &Value) -> bool {
            true
        }

        fn tool_output(&self) -> Option<&tokio::sync::mpsc::Sender<ToolOutputChunk>> {
            self.tool_output_tx.as_ref()
        }
    }

    #[test]
    fn schema_exposes_background_flag() {
        let definition = BashTool.definition();
        let property = &definition.input_schema["properties"]["run_in_background"];
        assert_eq!(property["type"], "boolean");
        assert!(property["description"]
            .as_str()
            .unwrap()
            .contains("background"));
    }

    #[test]
    fn utf8_tail_never_splits_a_multibyte_char() {
        // "中文abc" = 中(0..3) 文(3..6) a b c，共 9 字节
        let bytes = "中文abc".as_bytes();
        // max=4 起点落在「文」的后继字节上，必须继续前移到字符边界
        assert_eq!(String::from_utf8_lossy(utf8_tail(bytes, 4)), "abc");
        // max=6 起点正好是「文」的首字节，原样保留
        assert_eq!(String::from_utf8_lossy(utf8_tail(bytes, 6)), "文abc");
        // 短于上限时原样返回
        assert_eq!(utf8_tail(b"abc", 100), b"abc");
    }

    #[test]
    fn merge_tail_keeps_streams_separated_only_when_both_present() {
        assert_eq!(
            merge_tail(
                b"a
", b""
            ),
            "a\n"
        );
        assert_eq!(merge_tail(b"", b"e\n"), "e\n");
        assert_eq!(
            merge_tail(b"out\n", b"err\n"),
            "out\n\n--- stderr ---\nerr\n"
        );
        assert_eq!(merge_tail(b"", b""), "");
    }

    #[test]
    fn throttle_allows_first_send_and_rejects_identical_total() {
        let bufs = OutputBuffers::new();
        bufs.stdout.lock().unwrap().extend_from_slice(b"hello");
        let total = bufs.total();
        assert!(bufs.should_send(total), "首次推送必须放行");
        // 内容没变：即便过了间隔也不重复推
        assert!(!bufs.should_send(total), "无新输出时不应重复推送");
        // 有新字节但间隔没到：抑制
        bufs.stderr.lock().unwrap().push(b'x');
        assert!(
            !bufs.should_send(bufs.total()),
            "间隔内的第二次推送必须抑制"
        );
        std::thread::sleep(STREAM_INTERVAL);
        assert!(
            bufs.should_send(bufs.total()),
            "过了间隔且有新字节后应恢复推送"
        );
    }

    #[tokio::test]
    async fn bash_runs_command_and_captures_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = TestContext::plain(dir.path().to_path_buf());
        let result = BashTool
            .run(
                serde_json::json!({"command": "printf hello-from-bash"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("hello-from-bash"));
    }

    #[tokio::test]
    async fn bash_reports_exit_code_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = TestContext::plain(dir.path().to_path_buf());
        let result = BashTool
            .run(serde_json::json!({"command": "exit 7"}), &ctx)
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("退出码 7"));
    }

    #[tokio::test]
    async fn bash_reports_stderr_separately() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = TestContext::plain(dir.path().to_path_buf());
        let result = BashTool
            .run(
                serde_json::json!({"command": "printf out; printf boom >&2"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("out"), "{}", result.content);
        assert!(
            result.content.contains("--- stderr ---"),
            "{}",
            result.content
        );
        assert!(result.content.contains("boom"), "{}", result.content);
    }

    #[tokio::test]
    async fn bash_times_out_and_kills_the_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = TestContext::plain(dir.path().to_path_buf());
        // 子进程 sleep 的 pid 写文件；命令超时返回后轮询等它消失，
        // 若进程组没被杀掉，文件会在超时后继续被写入。
        let marker = dir.path().join("marker");
        let cmd = format!("( sleep 1; echo late > {} ) & wait", marker.display());
        let result = BashTool
            .run(serde_json::json!({"command": cmd, "timeout": 1}), &ctx)
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("命令超时"), "{}", result.content);

        // 超时后 1s 足够子进程写完；kill 生效则文件永远不出现
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        assert!(
            !marker.exists(),
            "超时后进程组必须被杀干净，子进程仍在写文件：{}",
            marker.display()
        );
    }

    #[tokio::test]
    async fn streaming_bash_pushes_snapshots_and_keeps_result_identical() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let ctx = TestContext {
            cwd: dir.path().to_path_buf(),
            tool_output_tx: Some(tx),
        };
        let meta = ToolCallMeta {
            tool_use_id: "toolu_abc".to_string(),
        };
        let result = BashTool
            .run_with_meta(
                serde_json::json!({
                    // 10 行 × ~50ms ≈ 0.5s，保证跨过若干个 100ms 推送窗口；
                    // 命令太快会在第一帧后就结束，测不出「期间推送过」这件事
                    "command": "for i in $(seq 1 10); do echo line-$i; sleep 0.05; done"
                }),
                &ctx,
                &meta,
            )
            .await
            .unwrap();
        assert!(!result.is_error, "{}", result.content);

        let mut snapshots = Vec::new();
        while let Ok(chunk) = rx.try_recv() {
            snapshots.push(chunk);
        }
        assert!(
            snapshots.len() >= 2,
            "流式路径必须跨越多个推送窗口持续刷新，否则运行中看不到输出，只得到 {} 帧",
            snapshots.len()
        );
        for chunk in &snapshots {
            assert_eq!(
                chunk.tool_use_id, "toolu_abc",
                "快照必须带调用 id 供接收端配对"
            );
            assert!(!chunk.tail.is_empty(), "空快照没有意义");
        }
        // 快照是「累计输出的尾部」而非增量，因此单调变长
        for pair in snapshots.windows(2) {
            assert!(
                pair[1].tail.len() > pair[0].tail.len(),
                "后一帧必须是更长的累计视图，不能原地不动：{:?} → {:?}",
                pair[0].tail,
                pair[1].tail
            );
        }
        // 每帧快照都必须与最终正文同源（输出未超 8KB 保尾上限时即为其前缀）——
        // 这条比「末帧含最后一行」更强：末帧允许比最终输出落后不到一个推送
        // 窗口（EOF 之后不再推送，由随后的 ToolEnd 结果块接手）。
        for chunk in &snapshots {
            assert!(
                result.content.starts_with(&chunk.tail),
                "快照必须是最终正文的前缀：{:?}",
                chunk.tail
            );
        }
        // 流式不得改变回给模型的正文
        assert!(result.content.contains("line-1"), "{}", result.content);
        assert!(result.content.contains("line-10"), "{}", result.content);
    }

    #[tokio::test]
    async fn no_receiver_means_no_push_but_same_result() {
        let dir = tempfile::tempdir().unwrap();
        // TestContext 未挂 channel：等同 headless 表面，必须静默降级
        let ctx = TestContext::plain(dir.path().to_path_buf());
        let result = BashTool
            .run(serde_json::json!({"command": "printf plain"}), &ctx)
            .await
            .unwrap();
        assert!(!result.is_error);
        assert_eq!(result.content, "plain");
    }

    #[tokio::test]
    async fn background_spawn_returns_shell_id() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = TestContext::plain(dir.path().to_path_buf());
        let result = BashTool
            .run(
                serde_json::json!({
                    "command": "sleep 1",
                    "run_in_background": true
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!result.is_error, "{:?}", result.content);
        assert!(result.content.contains("Started background shell"));
        crate::bash_session::BashSessionManager::global().kill_all();
    }
}
