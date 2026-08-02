//! OS 级命令隔离。前台和后台 Bash 必须经同一个 `SandboxRunner` 构造进程。

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxMode {
    Enforce,
    Permissive,
    Disabled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkPolicy {
    Deny,
    AllowedDomains(Vec<String>),
    AllowAll,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxPolicy {
    pub mode: SandboxMode,
    pub read_roots: Vec<PathBuf>,
    pub write_roots: Vec<PathBuf>,
    pub deny_read_roots: Vec<PathBuf>,
    pub deny_write_roots: Vec<PathBuf>,
    pub network: NetworkPolicy,
}

impl SandboxPolicy {
    pub fn enforced_workspace(cwd: &Path) -> Self {
        let mut policy = Self {
            mode: SandboxMode::Enforce,
            read_roots: vec![PathBuf::from("/")],
            write_roots: vec![canonical_or_owned(cwd)],
            deny_read_roots: default_credential_paths(),
            deny_write_roots: Vec::new(),
            network: NetworkPolicy::Deny,
        };
        policy.add_write_root(sandbox_temp_dir());
        policy
    }

    pub fn disabled() -> Self {
        Self {
            mode: SandboxMode::Disabled,
            read_roots: Vec::new(),
            write_roots: Vec::new(),
            deny_read_roots: Vec::new(),
            deny_write_roots: Vec::new(),
            network: NetworkPolicy::Deny,
        }
    }

    pub fn add_write_root(&mut self, path: PathBuf) {
        let path = canonical_or_owned(&path);
        if !self.write_roots.contains(&path) {
            self.write_roots.push(path);
        }
    }

    pub fn add_read_root(&mut self, path: PathBuf) {
        let path = canonical_or_owned(&path);
        if !self.read_roots.contains(&path) {
            self.read_roots.push(path);
        }
    }

    pub fn add_deny_read_root(&mut self, path: PathBuf) {
        let path = canonical_or_owned(&path);
        if !self.deny_read_roots.contains(&path) {
            self.deny_read_roots.push(path);
        }
    }

    pub fn add_deny_write_root(&mut self, path: PathBuf) {
        let path = canonical_or_owned(&path);
        if !self.deny_write_roots.contains(&path) {
            self.deny_write_roots.push(path);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxBackend {
    MacOsSeatbelt { executable: PathBuf },
    LinuxBubblewrap { executable: PathBuf },
    Unavailable { reason: String },
}

impl SandboxBackend {
    pub fn name(&self) -> &'static str {
        match self {
            Self::MacOsSeatbelt { .. } => "macos-seatbelt",
            Self::LinuxBubblewrap { .. } => "linux-bubblewrap",
            Self::Unavailable { .. } => "unavailable",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxStatus {
    pub backend: String,
    pub available: bool,
    pub filesystem_isolation: bool,
    pub domain_network_isolation: bool,
    pub dependencies: Vec<String>,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct SandboxRunner {
    backend: SandboxBackend,
}

#[derive(Debug, Error)]
pub enum SandboxError {
    #[error("sandbox unavailable: {0}")]
    Unavailable(String),
    #[error("sandbox backend {backend} cannot enforce domain-scoped networking for {domains:?}: {reason}")]
    DomainNetworkUnsupported {
        backend: String,
        domains: Vec<String>,
        reason: String,
    },
    #[error("invalid allowed network rule: {0}")]
    InvalidNetworkRule(String),
    #[error("failed to start controlled network proxy: {0}")]
    Proxy(String),
    #[error("invalid sandbox write root: {0}")]
    InvalidWriteRoot(PathBuf),
}

impl SandboxRunner {
    pub fn detect() -> &'static Self {
        static RUNNER: OnceLock<SandboxRunner> = OnceLock::new();
        RUNNER.get_or_init(|| SandboxRunner {
            backend: detect_backend(),
        })
    }

    pub fn backend(&self) -> &SandboxBackend {
        &self.backend
    }

    pub fn is_available(&self) -> bool {
        !matches!(self.backend, SandboxBackend::Unavailable { .. })
    }

    pub fn status(&self) -> SandboxStatus {
        match &self.backend {
            SandboxBackend::MacOsSeatbelt { executable } => SandboxStatus {
                backend: self.backend.name().to_string(),
                available: true,
                filesystem_isolation: true,
                domain_network_isolation: true,
                dependencies: vec![executable.display().to_string()],
                detail: "Seatbelt filesystem isolation with an out-of-sandbox, host/port-validating loopback proxy".to_string(),
            },
            SandboxBackend::LinuxBubblewrap { executable } => SandboxStatus {
                backend: self.backend.name().to_string(),
                available: true,
                filesystem_isolation: true,
                domain_network_isolation: false,
                dependencies: vec![
                    executable.display().to_string(),
                    "domain proxy bridge: unavailable (requests fail closed)".to_string(),
                ],
                detail: "bubblewrap filesystem and network-namespace isolation; domain-scoped access remains disabled until a verifiable namespace proxy bridge is present".to_string(),
            },
            SandboxBackend::Unavailable { reason } => SandboxStatus {
                backend: self.backend.name().to_string(),
                available: false,
                filesystem_isolation: false,
                domain_network_isolation: false,
                dependencies: Vec::new(),
                detail: reason.clone(),
            },
        }
    }

    /// 构造已清理环境变量的 shell 进程。调用方可继续设置 stdio/process group，
    /// 但不得替换 program/args 绕过 runner。
    pub fn shell_command(
        &self,
        shell_command: &str,
        cwd: &Path,
        policy: &SandboxPolicy,
    ) -> Result<Command, SandboxError> {
        if policy.mode == SandboxMode::Disabled {
            return Ok(direct_shell(shell_command, cwd));
        }

        // `Permissive` 仅表示交互表面可以在调用方弹出一次性降级审批；runner
        // 自身绝不静默直连。headless/schedule/SubAgent 因此天然 fail-closed。
        match &self.backend {
            SandboxBackend::MacOsSeatbelt { executable } => {
                self.macos_command(executable, shell_command, cwd, policy)
            }
            SandboxBackend::LinuxBubblewrap { executable } => {
                self.linux_command(executable, shell_command, cwd, policy)
            }
            SandboxBackend::Unavailable { reason } => {
                Err(SandboxError::Unavailable(reason.clone()))
            }
        }
    }

    /// 仅供已经完成显式人类审批的交互调用方使用。不要在 headless、schedule
    /// 或 SubAgent 中调用；这些表面必须保留 fail-closed 语义。
    pub fn unsandboxed_shell_command(&self, shell_command: &str, cwd: &Path) -> Command {
        direct_shell(shell_command, cwd)
    }

    fn macos_command(
        &self,
        executable: &Path,
        shell_command: &str,
        cwd: &Path,
        policy: &SandboxPolicy,
    ) -> Result<Command, SandboxError> {
        validate_roots(policy)?;
        let proxy_port = match &policy.network {
            NetworkPolicy::AllowedDomains(domains) => Some(controlled_proxy(domains)?),
            _ => None,
        };
        let profile = seatbelt_profile(policy, proxy_port);
        let mut command = Command::new(executable);
        command
            .arg("-p")
            .arg(profile)
            .arg("/bin/bash")
            .arg("-c")
            .arg(shell_command)
            .current_dir(cwd);
        sanitize_environment(&mut command);
        if let Some(port) = proxy_port {
            configure_proxy_environment(&mut command, port);
        }
        Ok(command)
    }

    fn linux_command(
        &self,
        executable: &Path,
        shell_command: &str,
        cwd: &Path,
        policy: &SandboxPolicy,
    ) -> Result<Command, SandboxError> {
        if let NetworkPolicy::AllowedDomains(domains) = &policy.network {
            return Err(SandboxError::DomainNetworkUnsupported {
                backend: self.backend.name().to_string(),
                domains: domains.clone(),
                reason: "bubblewrap's isolated network namespace has no verified loopback bridge to the controlled proxy".to_string(),
            });
        }
        validate_roots(policy)?;
        let mut command = Command::new(executable);
        command
            .arg("--die-with-parent")
            .arg("--new-session")
            .arg("--unshare-all")
            .arg("--ro-bind")
            .arg("/")
            .arg("/")
            .arg("--proc")
            .arg("/proc")
            .arg("--dev")
            .arg("/dev");
        if matches!(policy.network, NetworkPolicy::AllowAll) {
            command.arg("--share-net");
        }
        for root in &policy.write_roots {
            command.arg("--bind").arg(root).arg(root);
        }
        for root in &policy.deny_write_roots {
            if root.exists() {
                command.arg("--ro-bind").arg(root).arg(root);
            }
        }
        for root in &policy.deny_read_roots {
            if root.is_dir() {
                command.arg("--tmpfs").arg(root);
            } else if root.is_file() {
                command.arg("--ro-bind").arg("/dev/null").arg(root);
            }
        }
        command
            .arg("--chdir")
            .arg(cwd)
            .arg("/bin/bash")
            .arg("-c")
            .arg(shell_command);
        sanitize_environment(&mut command);
        Ok(command)
    }
}

fn detect_backend() -> SandboxBackend {
    #[cfg(target_os = "macos")]
    {
        let executable = PathBuf::from("/usr/bin/sandbox-exec");
        if executable.is_file() {
            return SandboxBackend::MacOsSeatbelt { executable };
        }
        SandboxBackend::Unavailable {
            reason: "macOS sandbox-exec is not available".to_string(),
        }
    }

    #[cfg(target_os = "linux")]
    {
        if let Some(executable) = find_in_path("bwrap") {
            let probe = Command::new(&executable)
                .args([
                    "--die-with-parent",
                    "--unshare-all",
                    "--ro-bind",
                    "/",
                    "/",
                    "/bin/true",
                ])
                .status();
            if probe.is_ok_and(|status| status.success()) {
                return SandboxBackend::LinuxBubblewrap { executable };
            }
            return SandboxBackend::Unavailable {
                reason: "bubblewrap is installed but cannot create an unprivileged sandbox"
                    .to_string(),
            };
        }
        return SandboxBackend::Unavailable {
            reason: "bubblewrap (bwrap) is not installed".to_string(),
        };
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        SandboxBackend::Unavailable {
            reason:
                "native Windows and this platform do not provide equivalent isolation; use WSL2"
                    .to_string(),
        }
    }
}

fn direct_shell(shell_command: &str, cwd: &Path) -> Command {
    let mut command = Command::new(if cfg!(windows) { "bash" } else { "/bin/bash" });
    command.arg("-c").arg(shell_command).current_dir(cwd);
    sanitize_environment(&mut command);
    command
}

fn sanitize_environment(command: &mut Command) {
    const ALLOWED_ENV: &[&str] = &[
        "PATH",
        "HOME",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "TERM",
        "COLORTERM",
        "CARGO_HOME",
        "RUSTUP_HOME",
        "TMPDIR",
    ];
    let values: Vec<(OsString, OsString)> = ALLOWED_ENV
        .iter()
        .filter_map(|name| std::env::var_os(name).map(|value| (OsString::from(name), value)))
        .collect();
    command.env_clear();
    command.envs(values);
}

fn validate_roots(policy: &SandboxPolicy) -> Result<(), SandboxError> {
    for root in &policy.write_roots {
        if !root.is_dir() {
            return Err(SandboxError::InvalidWriteRoot(root.clone()));
        }
    }
    Ok(())
}

fn seatbelt_profile(policy: &SandboxPolicy, proxy_port: Option<u16>) -> String {
    let mut profile = String::from(
        "(version 1)\n(deny default)\n(allow process*)\n(allow sysctl-read)\n(allow mach-lookup)\n(allow file-read*)\n(allow file-write-data (literal \"/dev/null\"))\n",
    );
    for root in &policy.write_roots {
        profile.push_str(&format!(
            "(allow file-write* (subpath \"{}\"))\n",
            seatbelt_escape(root.as_os_str())
        ));
    }
    for root in &policy.deny_write_roots {
        profile.push_str(&format!(
            "(deny file-write* (subpath \"{}\"))\n",
            seatbelt_escape(root.as_os_str())
        ));
    }
    for root in &policy.deny_read_roots {
        profile.push_str(&format!(
            "(deny file-read* (subpath \"{}\"))\n",
            seatbelt_escape(root.as_os_str())
        ));
    }
    if matches!(policy.network, NetworkPolicy::AllowAll) {
        profile.push_str("(allow network*)\n");
    } else if let Some(port) = proxy_port {
        profile.push_str(&format!(
            "(allow network-outbound (remote ip \"localhost:{port}\"))\n"
        ));
    }
    profile
}

fn configure_proxy_environment(command: &mut Command, port: u16) {
    let proxy = format!("http://127.0.0.1:{port}");
    for name in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
        command.env(name, &proxy);
    }
    command.env("NO_PROXY", "").env("no_proxy", "");
}

fn sandbox_temp_dir() -> PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let path = std::env::temp_dir().join(format!("wyj-code-sandbox-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&path);
        canonical_or_owned(&path)
    })
    .clone()
}

fn default_credential_paths() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    [
        ".ssh",
        ".aws",
        ".azure",
        ".kube",
        ".gnupg",
        ".config/gcloud",
        ".docker/config.json",
        ".netrc",
        ".zsh_history",
        ".bash_history",
        ".wyj-code",
    ]
    .into_iter()
    .map(|relative| home.join(relative))
    .filter(|path| path.exists())
    .map(|path| canonical_or_owned(&path))
    .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct NetworkRule {
    host: String,
    port: Option<u16>,
}

impl NetworkRule {
    fn parse(raw: &str) -> Result<Self, SandboxError> {
        let mut value = raw.trim().to_ascii_lowercase();
        if let Some(rest) = value.strip_prefix("https://") {
            value = rest.to_string();
        } else if let Some(rest) = value.strip_prefix("http://") {
            value = rest.to_string();
        }
        value = value
            .trim_end_matches('/')
            .trim_start_matches('.')
            .to_string();
        if value.is_empty() || value.contains(['/', '?', '#', '@', '[', ']']) {
            return Err(SandboxError::InvalidNetworkRule(raw.to_string()));
        }
        let (host, port) = match value.rsplit_once(':') {
            Some((host, port)) if !host.contains(':') => {
                let port = port
                    .parse::<u16>()
                    .map_err(|_| SandboxError::InvalidNetworkRule(raw.to_string()))?;
                (host.to_string(), Some(port))
            }
            _ => (value, None),
        };
        if !valid_domain(&host) {
            return Err(SandboxError::InvalidNetworkRule(raw.to_string()));
        }
        Ok(Self { host, port })
    }

    fn allows(&self, host: &str, port: u16) -> bool {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        let host_allowed = host == self.host || host.ends_with(&format!(".{}", self.host));
        let port_allowed = self
            .port
            .map(|allowed| allowed == port)
            .unwrap_or(matches!(port, 80 | 443));
        host_allowed && port_allowed
    }
}

fn valid_domain(host: &str) -> bool {
    !host.is_empty()
        && host != "localhost"
        && host.parse::<IpAddr>().is_err()
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

fn controlled_proxy(domains: &[String]) -> Result<u16, SandboxError> {
    let mut rules = domains
        .iter()
        .map(|domain| NetworkRule::parse(domain))
        .collect::<Result<Vec<_>, _>>()?;
    rules.sort_by(|a, b| a.host.cmp(&b.host).then(a.port.cmp(&b.port)));
    rules.dedup();
    if rules.is_empty() {
        return Err(SandboxError::InvalidNetworkRule(
            "allowed domain list is empty".to_string(),
        ));
    }

    static PROXIES: OnceLock<Mutex<HashMap<Vec<NetworkRule>, u16>>> = OnceLock::new();
    let proxies = PROXIES.get_or_init(|| Mutex::new(HashMap::new()));
    let mut proxies = proxies
        .lock()
        .map_err(|_| SandboxError::Proxy("proxy registry lock poisoned".to_string()))?;
    if let Some(port) = proxies.get(&rules) {
        return Ok(*port);
    }

    let listener = TcpListener::bind(("127.0.0.1", 0))
        .map_err(|error| SandboxError::Proxy(error.to_string()))?;
    let port = listener
        .local_addr()
        .map_err(|error| SandboxError::Proxy(error.to_string()))?
        .port();
    let thread_rules = rules.clone();
    std::thread::Builder::new()
        .name(format!("wyj-domain-proxy-{port}"))
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let rules = thread_rules.clone();
                let _ = std::thread::Builder::new()
                    .name("wyj-domain-proxy-conn".to_string())
                    .spawn(move || {
                        let _ = handle_proxy_connection(stream, &rules);
                    });
            }
        })
        .map_err(|error| SandboxError::Proxy(error.to_string()))?;
    proxies.insert(rules, port);
    Ok(port)
}

fn handle_proxy_connection(mut client: TcpStream, rules: &[NetworkRule]) -> std::io::Result<()> {
    client.set_read_timeout(Some(Duration::from_secs(15)))?;
    client.set_write_timeout(Some(Duration::from_secs(15)))?;
    let mut request = Vec::with_capacity(4096);
    let header_end = loop {
        if request.len() > 64 * 1024 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "proxy request headers exceed 64 KiB",
            ));
        }
        let mut chunk = [0u8; 4096];
        let read = client.read(&mut chunk)?;
        if read == 0 {
            return Ok(());
        }
        request.extend_from_slice(&chunk[..read]);
        if let Some(index) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
    };

    let headers = std::str::from_utf8(&request[..header_end]).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "non-UTF8 proxy headers")
    })?;
    let mut lines = headers.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    let version = parts.next().unwrap_or("HTTP/1.1");

    if method.eq_ignore_ascii_case("CONNECT") {
        let (host, port) = parse_authority(target, 443)?;
        if !rules.iter().any(|rule| rule.allows(&host, port)) {
            client.write_all(b"HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n")?;
            return Ok(());
        }
        let upstream = connect_public(&host, port)?;
        client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;
        relay(client, upstream)
    } else {
        let parsed = url::Url::parse(target).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "HTTP proxy requests must use an absolute URL",
            )
        })?;
        if parsed.scheme() != "http" {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "only CONNECT or plain HTTP proxy requests are supported",
            ));
        }
        let host = parsed
            .host_str()
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "URL has no host")
            })?
            .to_ascii_lowercase();
        let port = parsed.port_or_known_default().unwrap_or(80);
        if !rules.iter().any(|rule| rule.allows(&host, port)) {
            client.write_all(b"HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n")?;
            return Ok(());
        }
        let mut upstream = connect_public(&host, port)?;
        let origin_target = match parsed.query() {
            Some(query) => format!("{}?{query}", parsed.path()),
            None => parsed.path().to_string(),
        };
        let mut forwarded = format!("{method} {origin_target} {version}\r\n").into_bytes();
        for line in lines.filter(|line| !line.is_empty()) {
            let name = line.split_once(':').map(|(name, _)| name.trim());
            if name.is_some_and(|name| {
                name.eq_ignore_ascii_case("proxy-authorization")
                    || name.eq_ignore_ascii_case("proxy-connection")
            }) {
                continue;
            }
            forwarded.extend_from_slice(line.as_bytes());
            forwarded.extend_from_slice(b"\r\n");
        }
        forwarded.extend_from_slice(b"\r\n");
        forwarded.extend_from_slice(&request[header_end..]);
        upstream.write_all(&forwarded)?;
        relay(client, upstream)
    }
}

fn parse_authority(authority: &str, default_port: u16) -> std::io::Result<(String, u16)> {
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => (
            host,
            port.parse::<u16>().map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid proxy port")
            })?,
        ),
        _ => (authority, default_port),
    };
    if !valid_domain(host) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "IP literals, localhost and invalid domains are not allowed",
        ));
    }
    Ok((host.to_ascii_lowercase(), port))
}

fn connect_public(host: &str, port: u16) -> std::io::Result<TcpStream> {
    let addresses = (host, port).to_socket_addrs()?;
    let mut last_error = None;
    for address in addresses.filter(|address| public_ip(address.ip())) {
        match TcpStream::connect_timeout(&address, Duration::from_secs(10)) {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "domain resolved only to private, local or reserved addresses",
        )
    }))
}

fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let octets = ip.octets();
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_broadcast()
                || ip.is_documentation()
                || ip.is_unspecified()
                || ip.is_multicast()
                || octets[0] >= 240
                || (octets[0] == 100 && (64..=127).contains(&octets[1]))
                || (octets[0] == 198 && matches!(octets[1], 18 | 19)))
        }
        IpAddr::V6(ip) => {
            let segments = ip.segments();
            !(ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                || (segments[0] & 0xfe00) == 0xfc00
                || (segments[0] & 0xffc0) == 0xfe80)
        }
    }
}

fn relay(client: TcpStream, upstream: TcpStream) -> std::io::Result<()> {
    let mut client_reader = client.try_clone()?;
    let mut upstream_writer = upstream.try_clone()?;
    let forward =
        std::thread::spawn(move || std::io::copy(&mut client_reader, &mut upstream_writer));
    let mut upstream_reader = upstream;
    let mut client_writer = client;
    let reverse = std::io::copy(&mut upstream_reader, &mut client_writer);
    let _ = forward.join();
    reverse.map(|_| ())
}

fn seatbelt_escape(value: &OsStr) -> String {
    value
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

fn canonical_or_owned(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(target_os = "linux")]
fn find_in_path(program: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|path| path.join(program))
            .find(|path| path.is_file())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_mode_builds_direct_shell_and_scrubs_provider_keys() {
        let dir = tempfile::tempdir().unwrap();
        let command = SandboxRunner::detect()
            .shell_command("pwd", dir.path(), &SandboxPolicy::disabled())
            .unwrap();
        assert!(command.get_args().any(|arg| arg == "-c"));
        assert!(!command
            .get_envs()
            .any(|(name, _)| name == "WYJ_CODE_API_KEY"));
    }

    #[test]
    fn enforced_workspace_always_includes_cwd_as_write_root() {
        let dir = tempfile::tempdir().unwrap();
        let policy = SandboxPolicy::enforced_workspace(dir.path());
        assert_eq!(policy.mode, SandboxMode::Enforce);
        assert!(policy
            .write_roots
            .iter()
            .any(|root| root == &dir.path().canonicalize().unwrap()));
    }

    #[test]
    fn network_rules_are_domain_and_port_scoped() {
        let default = NetworkRule::parse("example.com").unwrap();
        assert!(default.allows("example.com", 443));
        assert!(default.allows("api.example.com", 80));
        assert!(!default.allows("example.com.evil.invalid", 443));
        assert!(!default.allows("example.com", 22));

        let exact = NetworkRule::parse("api.example.com:8443").unwrap();
        assert!(exact.allows("api.example.com", 8443));
        assert!(!exact.allows("api.example.com", 443));
        assert!(NetworkRule::parse("127.0.0.1:8080").is_err());
        assert!(NetworkRule::parse("localhost").is_err());
    }

    #[test]
    fn permissive_mode_never_silently_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let runner = SandboxRunner {
            backend: SandboxBackend::Unavailable {
                reason: "test".to_string(),
            },
        };
        let mut policy = SandboxPolicy::enforced_workspace(dir.path());
        policy.mode = SandboxMode::Permissive;
        assert!(runner.shell_command("pwd", dir.path(), &policy).is_err());
    }

    #[test]
    fn available_backend_allows_workspace_write_and_denies_sibling_write() {
        let runner = SandboxRunner::detect();
        if !runner.is_available() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let outside = root.path().join("outside.txt");
        let inside = workspace.join("inside.txt");
        let command = format!(
            "printf inside > '{}'; printf outside > '{}'",
            inside.display(),
            outside.display()
        );
        let policy = SandboxPolicy::enforced_workspace(&workspace);
        let status = runner
            .shell_command(&command, &workspace, &policy)
            .unwrap()
            .status()
            .unwrap();
        assert!(!status.success(), "outside write must make the shell fail");
        assert_eq!(std::fs::read_to_string(&inside).unwrap(), "inside");
        assert!(!outside.exists());
    }

    #[test]
    fn available_backend_honors_explicit_deny_read_root() {
        let runner = SandboxRunner::detect();
        if !runner.is_available() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let secret = root.path().join("credential.txt");
        std::fs::write(&secret, "do-not-read").unwrap();
        let mut policy = SandboxPolicy::enforced_workspace(&workspace);
        policy.add_deny_read_root(secret.clone());
        let status = runner
            .shell_command(
                &format!("cat '{}' >/dev/null", secret.display()),
                &workspace,
                &policy,
            )
            .unwrap()
            .status()
            .unwrap();
        assert!(!status.success());
    }

    #[test]
    #[ignore = "requires public network access; run explicitly during the release gate"]
    fn macos_domain_proxy_allows_only_the_approved_domain() {
        let runner = SandboxRunner::detect();
        if !matches!(runner.backend(), SandboxBackend::MacOsSeatbelt { .. }) {
            return;
        }
        let workspace = tempfile::tempdir().unwrap();
        let mut policy = SandboxPolicy::enforced_workspace(workspace.path());
        policy.network = NetworkPolicy::AllowedDomains(vec!["example.com:443".to_string()]);

        let allowed = runner
            .shell_command(
                "curl -fsSI --max-time 15 https://example.com >/dev/null",
                workspace.path(),
                &policy,
            )
            .unwrap()
            .status()
            .unwrap();
        assert!(allowed.success());

        let denied = runner
            .shell_command(
                "curl -fsSI --max-time 10 https://example.org >/dev/null",
                workspace.path(),
                &policy,
            )
            .unwrap()
            .status()
            .unwrap();
        assert!(!denied.success());
    }
}
