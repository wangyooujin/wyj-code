//! Bash 后台任务管理：run_in_background 的进程生命周期、输出缓冲与增量读取。
//!
//! 进程级单例（仿 SubAgentHub）：主 Agent 与子 Agent 共享同一管理器，
//! TUI 退出与 headless 结束时统一 `kill_all()` 防孤儿进程。
//! 进程以独立 process group 启动，kill 时对整个进程组发信号（杀掉子进程树）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

/// 单个任务的输出缓冲上限（字节）：超限丢弃最旧内容（保尾）
const MAX_BUFFER_BYTES: usize = 2_000_000;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum JobStatus {
    Running,
    Exited(i32),
}

struct JobBuffer {
    data: String,
    /// BashOutput 增量读游标（相对 data 起点；data 被裁剪时同步左移）
    read_offset: usize,
    /// 因缓冲上限被丢弃的字节数（提示用）
    dropped_bytes: usize,
}

pub struct BackgroundJob {
    pub command: String,
    /// 进程组 id（= 子进程 pid，spawn 时 process_group(0)）
    pgid: u32,
    started: Instant,
    buffer: Mutex<JobBuffer>,
    status: Mutex<JobStatus>,
}

impl BackgroundJob {
    pub fn status(&self) -> JobStatus {
        *self.status.lock().unwrap()
    }

    pub fn elapsed_secs(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }

    fn append(&self, chunk: &str) {
        let mut buf = self.buffer.lock().unwrap();
        buf.data.push_str(chunk);
        // 超限保尾：丢最旧内容，读游标同步左移
        if buf.data.len() > MAX_BUFFER_BYTES {
            let excess = buf.data.len() - MAX_BUFFER_BYTES;
            let mut cut = excess;
            while cut < buf.data.len() && !buf.data.is_char_boundary(cut) {
                cut += 1;
            }
            buf.data.drain(..cut);
            buf.dropped_bytes += cut;
            buf.read_offset = buf.read_offset.saturating_sub(cut);
        }
    }

    /// 取自上次读取以来的新输出，推进游标
    pub fn read_new(&self) -> (String, usize) {
        let mut buf = self.buffer.lock().unwrap();
        let new = buf.data[buf.read_offset..].to_string();
        let dropped = buf.dropped_bytes;
        buf.read_offset = buf.data.len();
        (new, dropped)
    }
}

#[derive(Default)]
pub struct BashSessionManager {
    jobs: Mutex<HashMap<String, Arc<BackgroundJob>>>,
    next_id: AtomicUsize,
}

impl BashSessionManager {
    /// 进程级单例
    pub fn global() -> &'static BashSessionManager {
        static INSTANCE: OnceLock<BashSessionManager> = OnceLock::new();
        INSTANCE.get_or_init(BashSessionManager::default)
    }

    /// 启动后台命令，返回任务 id（如 "bash_1"）
    pub fn spawn(&self, command: &str, cwd: &std::path::Path) -> anyhow::Result<String> {
        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-c")
            .arg(command)
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        #[cfg(unix)]
        cmd.process_group(0);

        let mut child = cmd.spawn()?;
        let pgid = child.id().unwrap_or(0);
        let id = format!("bash_{}", self.next_id.fetch_add(1, Ordering::Relaxed) + 1);

        let job = Arc::new(BackgroundJob {
            command: command.to_string(),
            pgid,
            started: Instant::now(),
            buffer: Mutex::new(JobBuffer {
                data: String::new(),
                read_offset: 0,
                dropped_bytes: 0,
            }),
            status: Mutex::new(JobStatus::Running),
        });
        self.jobs.lock().unwrap().insert(id.clone(), job.clone());

        // stdout/stderr 泵：合流写入同一缓冲
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        for pipe in [stdout.map(PipeReader::Out), stderr.map(PipeReader::Err)]
            .into_iter()
            .flatten()
        {
            let job = job.clone();
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut buf = [0u8; 8192];
                match pipe {
                    PipeReader::Out(mut r) => {
                        while let Ok(n) = r.read(&mut buf).await {
                            if n == 0 {
                                break;
                            }
                            job.append(&String::from_utf8_lossy(&buf[..n]));
                        }
                    }
                    PipeReader::Err(mut r) => {
                        while let Ok(n) = r.read(&mut buf).await {
                            if n == 0 {
                                break;
                            }
                            job.append(&String::from_utf8_lossy(&buf[..n]));
                        }
                    }
                }
            });
        }

        // 等待进程退出，更新状态
        {
            let job = job.clone();
            tokio::spawn(async move {
                let code = match child.wait().await {
                    Ok(st) => st.code().unwrap_or(-1),
                    Err(_) => -1,
                };
                *job.status.lock().unwrap() = JobStatus::Exited(code);
            });
        }

        Ok(id)
    }

    pub fn get(&self, id: &str) -> Option<Arc<BackgroundJob>> {
        self.jobs.lock().unwrap().get(id).cloned()
    }

    pub fn running_count(&self) -> usize {
        self.jobs
            .lock()
            .unwrap()
            .values()
            .filter(|j| j.status() == JobStatus::Running)
            .count()
    }

    /// 终止单个任务：对进程组 SIGTERM，2s 后仍存活则 SIGKILL
    pub async fn kill(&self, id: &str) -> anyhow::Result<bool> {
        let Some(job) = self.get(id) else {
            return Ok(false);
        };
        if job.status() != JobStatus::Running {
            return Ok(true); // 已退出，幂等成功
        }
        signal_group(job.pgid, "TERM");
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        if job.status() == JobStatus::Running {
            signal_group(job.pgid, "KILL");
        }
        Ok(true)
    }

    /// 退出清理：对所有仍在运行的任务进程组发 SIGKILL（防孤儿进程）
    pub fn kill_all(&self) {
        for job in self.jobs.lock().unwrap().values() {
            if job.status() == JobStatus::Running {
                signal_group(job.pgid, "KILL");
            }
        }
    }

    /// 列出全部任务（id, 命令, 状态）
    pub fn list(&self) -> Vec<(String, String, JobStatus)> {
        let mut v: Vec<_> = self
            .jobs
            .lock()
            .unwrap()
            .iter()
            .map(|(id, j)| (id.clone(), j.command.clone(), j.status()))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }
}

enum PipeReader {
    Out(tokio::process::ChildStdout),
    Err(tokio::process::ChildStderr),
}

/// 对进程组发信号（SIGTERM/SIGKILL）。
///
/// 之前用 `std::process::Command::new("kill")` fork 一个外部进程来发信号，
/// 在同一进程里混用 tokio::process（内部靠 SIGCHLD + waitpid(-1, WNOHANG)
/// 循环异步回收子进程）与 std::process 的同步阻塞 wait，属于 tokio 文档明确
/// 提示过的危险用法：两套回收机制可能对同一批子进程产生竞争，谁先收割到
/// 退出状态不确定。GitHub Actions 的 ubuntu-latest runner 上 `kill_terminates_
/// process_group` 测试稳定复现失败（目标进程状态一直不翻转），本地 macOS
/// 未必稳定命中同样的竞态窗口。直接调用 libc::kill 发系统调用，不再 fork
/// 额外进程，从源头消除这个竞态，同时也不再依赖 PATH 里一定有 `kill` 这个
/// 外部命令。
#[cfg(unix)]
fn signal_group(pgid: u32, sig: &str) {
    if pgid == 0 {
        return;
    }
    let signum = match sig {
        "TERM" => libc::SIGTERM,
        "KILL" => libc::SIGKILL,
        _ => return,
    };
    // SAFETY: 对自己 fork 出的子进程所在的进程组发信号，pgid 非 0（已在上面检查），
    // 传负值表示信号发给整个进程组而非单个进程，是 kill(2) 的标准用法。
    unsafe {
        libc::kill(-(pgid as i32), signum);
    }
}

#[cfg(not(unix))]
fn signal_group(pgid: u32, sig: &str) {
    if pgid == 0 {
        return;
    }
    let _ = std::process::Command::new("kill")
        .arg(format!("-{sig}"))
        .arg(format!("-{pgid}"))
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn spawn_read_and_exit() {
        let mgr = BashSessionManager::default();
        let id = mgr
            .spawn(
                "echo hello; sleep 0.2; echo done",
                std::path::Path::new("/tmp"),
            )
            .unwrap();
        let job = mgr.get(&id).unwrap();
        // 等进程结束
        for _ in 0..50 {
            if job.status() != JobStatus::Running {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(job.status(), JobStatus::Exited(0));
        let (out, _) = job.read_new();
        assert!(out.contains("hello"));
        assert!(out.contains("done"));
        // 增量读：再次读取应为空
        let (out2, _) = job.read_new();
        assert!(out2.is_empty());
    }

    #[tokio::test]
    async fn kill_terminates_process_group() {
        let mgr = BashSessionManager::default();
        let id = mgr.spawn("sleep 30", std::path::Path::new("/tmp")).unwrap();
        let job = mgr.get(&id).unwrap();
        assert_eq!(job.status(), JobStatus::Running);
        assert!(mgr.kill(&id).await.unwrap());
        // kill() 内部已等过 2s（SIGTERM 兜底 SIGKILL），这里再多等最多 5s 确认进程被
        // reap、状态翻转——繁忙/资源受限的 CI runner 上调度延迟可能明显长于本地开发机，
        // 之前只多等 1s 在 GitHub Actions ubuntu-latest 上实测会偶发超时导致误报失败。
        for _ in 0..100 {
            if job.status() != JobStatus::Running {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_ne!(job.status(), JobStatus::Running);
        // 幂等
        assert!(mgr.kill(&id).await.unwrap());
    }
}
