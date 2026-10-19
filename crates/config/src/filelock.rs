//! 跨进程文件锁：把「load → 修改 → save」这类 read-modify-write 收敛成临界区。
//!
//! # 为什么需要它
//!
//! 多个 `wyj-code` 进程跑在同一项目时会共享同一批状态文件（授权名单、记忆记录、
//! 会话文件、扩展安装清单）。这些写入全是 read-modify-write，而此前**全仓没有任何
//! 文件锁**——所有冲突都以「最后写者赢」的形式静默发生，非原子写还会截出半截 JSON
//! 让读侧把整个文件当成不存在。
//!
//! # 为什么住在 wyj-config
//!
//! `wyj-core` 依赖 `wyj-config`，方向不能反。锁要能被 `Config::save` 自己用——那
//! 是最典型的 load→改→save——就必须住在最底层 crate 上。
//!
//! # 为什么不用第三方 crate
//!
//! unix 走 `flock(2)`：内核把锁绑定在文件描述符上，进程崩溃或被 `SIGKILL` 时锁
//! **自动释放**，因此不存在 stale lock，调用方只需在 Drop 里解锁。非 unix 平台没有
//! `flock`，退化为 `create_new(true)` 的锁文件 + 持有者 pid + 陈旧超时兜底。
//!
//! 用法：
//!
//! ```no_run
//! # fn save(list: &[String]) -> anyhow::Result<()> {
//! let path = std::path::Path::new("/tmp/x.json");
//! let _guard = wyj_config::filelock::acquire(path, "authorized tool list")?;
//! let mut current = std::fs::read_to_string(path).unwrap_or_default();
//! // ... 在临界区内重新读盘、合并、落盘 ...
//! # let _ = current; let _ = list;
//! # Ok(())
//! # }
//! ```

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

/// 等锁最长时长。超时后返回错误而不是无限阻塞——宁可让调用方明确失败并向用户
/// 解释，也不要在一个看不见的死锁里静默挂起。
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(30);
/// 重试间隔。配合 `LOCK_NB` 做轮询，避免阻塞在不可中断的内核等待里。
const RETRY_INTERVAL: Duration = Duration::from_millis(20);
/// 非 unix 平台：锁文件持有者超过这个时长没释放就判定为陈旧（进程被强杀且没跑 Drop）。
#[cfg(not(unix))]
const STALE_AFTER: Duration = Duration::from_secs(120);

/// 跨进程排他锁的守卫。持有期间同名锁文件不可被其他进程获取；`drop` 时释放。
#[derive(Debug)]
pub struct FileLock {
    #[allow(dead_code)]
    path: PathBuf,
    #[cfg(not(unix))]
    acquired: bool,
    #[cfg(unix)]
    file: fs::File,
}

impl FileLock {
    /// 锁的持有者 pid。用于向用户解释「谁在占着这个状态文件」。
    pub fn holder_pid(&self) -> Option<u32> {
        read_holder_pid(&self.path)
    }
}

/// 获取 `path` 的跨进程排他锁。`what` 是人类可读的锁用途，只出现在错误信息里。
pub fn acquire(path: &Path, what: &str) -> Result<FileLock> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create lock directory {}", parent.display()))?;
    }
    imp::acquire(path, what)
}

/// 非阻塞获取：锁已被别人持有就立刻返回 `Ok(None)`，不等待。
///
/// 与 [`acquire`] 的区别只有语义，不是另一套实现——定时任务的防重入需要的是
/// 「有人在跑就放弃这次触发」，而不是排队 30 秒后报错。排队会让 cron 把本该
/// 跳过的触发变成一串等待中的进程。
pub fn try_acquire(path: &Path, what: &str) -> Result<Option<FileLock>> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create lock directory {}", parent.display()))?;
    }
    imp::try_acquire(path, what)
}

/// `path` 若是当前进程自己持有的锁，释放它；否则什么都不做。
///
/// 用于「上次崩溃留下的孤儿锁」自愈：调用方在 `acquire` 失败后调用本函数再重试一次。
pub fn release_self(path: &Path) -> Result<()> {
    imp::release_self(path)
}

/// 读取锁文件里记录的持有者 pid（非 unix 实现才有意义）。
pub fn read_holder_pid(path: &Path) -> Option<u32> {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| text.trim().parse().ok())
}

/// 锁文件里记录的 pid，且该进程**当前仍存活**。
///
/// 「文件里有 pid」不等于「有人正持锁」：进程被 `kill -9` 后 unix 的 flock 由内核
/// 释放，但锁文件里的 pid 字样还在。不做这一步，调用方会把每一个崩过的进程都当成
/// 活跃占用者。
pub fn active_holder(path: &Path) -> Option<u32> {
    let pid = read_holder_pid(path)?;
    (pid != std::process::id() && is_process_alive(pid)).then_some(pid)
}

/// `pid` 对应的进程是否还活着。
///
/// unix 用 `kill(pid, 0)`：返回 0 或 `EPERM` 都表示进程存在（`EPERM` 只是别人家
/// 的进程），`ESRCH` 才是真的没了。其他平台退化为「假定存活」——宁可多报一次占用，
/// 也不要漏报导致用户两边同时写同一份状态。
#[cfg(unix)]
pub fn is_process_alive(pid: u32) -> bool {
    // SAFETY: `kill(pid, 0)` 不投递信号，只做存在性/权限探测。pid 来自本进程写下的
    // 锁文件，转成 `pid_t` 不会溢出出另一个有效 pid。
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
pub fn is_process_alive(_pid: u32) -> bool {
    true
}

// ── unix: flock ──────────────────────────────────────────────────────────────

#[cfg(unix)]
mod imp {
    use super::*;
    use std::os::unix::io::AsRawFd;

    pub fn acquire(path: &Path, what: &str) -> Result<FileLock> {
        use std::io::Write;
        // `truncate(false)` 是关键：锁文件本身承载 pid，截断会把别的进程的持有者信息抹掉。
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("open lock file {}", path.display()))?;
        let fd = file.as_raw_fd();
        let deadline = Instant::now() + ACQUIRE_TIMEOUT;
        loop {
            let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
            if rc == 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            // EWOULDBLOCK/EAGAIN 只是「有人正持有」，其余是真错误（EBADF 等）。
            let contended = matches!(
                error.raw_os_error(),
                Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN
            );
            if !contended {
                return Err(error).with_context(|| format!("lock {what} at {}", path.display()));
            }
            if Instant::now() >= deadline {
                let holder = read_holder_pid(path)
                    .map(|pid| format!(" (held by pid {pid})"))
                    .unwrap_or_default();
                bail!("timed out after {ACQUIRE_TIMEOUT:?} waiting for the lock on {what}{holder}")
            }
            std::thread::sleep(RETRY_INTERVAL);
        }
        // 只在拿到锁之后写 pid：读者看到 pid 即意味着这个 pid 确实持锁。
        let _ = file.set_len(0);
        let _ = writeln!(file, "{}", std::process::id());
        Ok(FileLock {
            path: path.to_path_buf(),
            file,
        })
    }

    pub fn release_self(path: &Path) -> Result<()> {
        // flock 的持有者是 fd，别的进程无法代为解锁；进程退出时内核自动释放。
        // 这里唯一能自愈的是「pid 文件存在但锁其实已释放」的情况，删掉让它重新记录。
        let _ = fs::remove_file(path);
        Ok(())
    }

    pub fn try_acquire(path: &Path, what: &str) -> Result<Option<FileLock>> {
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("open lock file {}", path.display()))?;
        let fd = file.as_raw_fd();
        let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let error = std::io::Error::last_os_error();
            let contended = matches!(
                error.raw_os_error(),
                Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN
            );
            if contended {
                return Ok(None);
            }
            return Err(error).with_context(|| format!("lock {what} at {}", path.display()));
        }
        let _ = file.set_len(0);
        let _ = writeln!(file, "{}", std::process::id());
        Ok(Some(FileLock {
            path: path.to_path_buf(),
            file,
        }))
    }
}

// ── 非 unix: 锁文件 + 陈旧兜底 ───────────────────────────────────────────────

#[cfg(not(unix))]
mod imp {
    use super::*;

    pub fn acquire(path: &Path, what: &str) -> Result<FileLock> {
        let deadline = Instant::now() + ACQUIRE_TIMEOUT;
        loop {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
            {
                Ok(mut file) => {
                    use std::io::Write;
                    let _ = writeln!(file, "{}", std::process::id());
                    return Ok(FileLock {
                        path: path.to_path_buf(),
                        acquired: true,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("lock {what} at {}", path.display()));
                }
            }
            if is_stale(path) {
                let _ = fs::remove_file(path);
                continue;
            }
            if Instant::now() >= deadline {
                let holder = read_holder_pid(path)
                    .map(|pid| format!(" (held by pid {pid})"))
                    .unwrap_or_default();
                bail!("timed out after {ACQUIRE_TIMEOUT:?} waiting for the lock on {what}{holder}")
            }
            std::thread::sleep(RETRY_INTERVAL);
        }
    }

    pub fn release_self(path: &Path) -> Result<()> {
        fs::remove_file(path)
            .or_else(|error| match error.kind() {
                std::io::ErrorKind::NotFound => Ok(()),
                _ => Err(error),
            })
            .with_context(|| format!("remove stale lock {}", path.display()))
    }

    pub fn try_acquire(path: &Path, what: &str) -> Result<Option<FileLock>> {
        use std::io::Write;
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(mut file) => {
                let _ = writeln!(file, "{}", std::process::id());
                Ok(Some(FileLock {
                    path: path.to_path_buf(),
                    acquired: true,
                }))
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if is_stale(path) {
                    let _ = fs::remove_file(path);
                    return Ok(None);
                }
                Ok(None)
            }
            Err(error) => Err(error).with_context(|| format!("lock {what} at {}", path.display())),
        }
    }

    fn is_stale(path: &Path) -> bool {
        fs::metadata(path)
            .and_then(|meta| meta.modified())
            .map(|modified| modified.elapsed().unwrap_or_default() > STALE_AFTER)
            .unwrap_or(false)
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
        }
        #[cfg(not(unix))]
        if self.acquired {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_is_reentrant_across_sequential_acquisitions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.lock");
        {
            let _first = acquire(&path, "test state").unwrap();
            assert_eq!(
                read_holder_pid(&path),
                Some(std::process::id()),
                "holder pid should be recorded once the lock is held"
            );
        }
        // 前一个守卫已 drop，必须能立刻重新获取（否则说明释放逻辑坏了）。
        let _second = acquire(&path, "test state").unwrap();
    }

    #[test]
    fn creates_missing_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("deeper").join("state.lock");
        let _guard = acquire(&path, "test state").unwrap();
        assert!(path.exists());
    }

    #[test]
    fn separate_paths_do_not_contend() {
        let dir = tempfile::tempdir().unwrap();
        let _a = acquire(&dir.path().join("a.lock"), "a").unwrap();
        let _b = acquire(&dir.path().join("b.lock"), "b").unwrap();
    }

    /// 同一个锁在**另一个进程**上必须真的互斥 —— 进程内的 Mutex 在这里帮不上忙，
    /// 而这正是全部共享状态写入要依赖的性质。
    #[test]
    fn a_second_process_cannot_take_the_same_lock() {
        use std::process::Command;
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("contended.lock");
        let _guard = acquire(&lock_path, "test state").unwrap();

        // 复用本测试二进制自身跑一次非阻塞探测（见 `probe_lock_is_held`）。
        // 用 `flock` 自己的语义验证 `flock`，不需要额外依赖或外部工具。
        let probe = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "filelock::tests::probe_lock_is_held",
                "--ignored",
                "--nocapture",
            ])
            .env("WYJ_TEST_LOCK_PROBE", &lock_path)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&probe.stderr);
        assert!(
            stderr.contains("lock is locked"),
            "another process must observe the lock as held; stderr was: {stderr}"
        );
    }

    /// 子进程侧：非阻塞尝试一次并把结论打到 stderr，随后立刻退出（不去等 30 秒超时）。
    #[test]
    #[ignore]
    fn probe_lock_is_held() {
        use std::os::unix::io::AsRawFd;
        let Ok(path) = std::env::var("WYJ_TEST_LOCK_PROBE") else {
            return;
        };
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .unwrap();
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        eprintln!("lock is {}", if rc == 0 { "free" } else { "locked" });
    }
}
