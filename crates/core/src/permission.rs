//! 与 TUI/CLI 解耦的工具权限策略。

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionMode {
    /// 交互表面逐次询问；无交互表面 fail-closed。
    Prompt,
    /// 显式 bypass。跳过询问，但不绕过受保护路径和 sandbox 要求。
    AutoApprove,
    /// Headless/schedule 的显式工具白名单。
    Allowlist(HashSet<String>),
    /// Plan 模式：工具名白名单之外，还强制只读 Bash 和文档写路径策略。
    Plan(HashSet<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionSurface {
    TuiInteractive,
    /// ACP client with a real `session/request_permission` round trip.
    AcpClient,
    SinglePrompt,
    HeadlessRepl,
    Schedule,
    SubAgent,
    Hook,
}

impl ExecutionSurface {
    pub fn is_interactive(self) -> bool {
        matches!(self, Self::TuiInteractive | Self::AcpClient)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRequest {
    pub mode: PermissionMode,
    pub surface: ExecutionSurface,
    pub tool_name: String,
    pub input: Value,
    pub cwd: PathBuf,
    pub sandbox_available: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionVerdict {
    Allow,
    Ask(PermissionPrompt),
    Deny(DenyReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionPrompt {
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DenyReason {
    pub code: &'static str,
    pub message: String,
}

impl DenyReason {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct PermissionPolicy {
    pub allowed_write_roots: Vec<PathBuf>,
    pub allowed_domains: HashSet<String>,
    pub plan_document_grants: HashSet<PathBuf>,
    pub require_sandbox: bool,
}

impl PermissionPolicy {
    pub fn evaluate(&self, request: &PermissionRequest) -> PermissionVerdict {
        if self.require_sandbox && is_process_tool(&request.tool_name) && !request.sandbox_available
        {
            return PermissionVerdict::Deny(DenyReason::new(
                "sandbox_required",
                "该工具要求 sandbox，但当前平台没有可用的隔离后端",
            ));
        }

        let write_target = if is_file_write_tool(&request.tool_name) {
            match request.input.get("file_path").and_then(Value::as_str) {
                Some(raw) => match safe_resolve_write_target(&request.cwd, raw) {
                    Ok(path) => Some(path),
                    Err(reason) => return PermissionVerdict::Deny(reason),
                },
                None => {
                    return PermissionVerdict::Deny(DenyReason::new(
                        "missing_write_target",
                        "写工具缺少有效的 file_path",
                    ));
                }
            }
        } else {
            None
        };

        match &request.mode {
            PermissionMode::Plan(allowed) => {
                if !allowed.contains(&request.tool_name) {
                    return PermissionVerdict::Deny(DenyReason::new(
                        "tool_not_allowed_in_plan",
                        format!("工具 `{}` 不在 Plan 模式白名单内", request.tool_name),
                    ));
                }
                if let Some(target) = write_target.as_deref() {
                    if self.plan_write_allowed(&request.cwd, target) {
                        return PermissionVerdict::Allow;
                    }
                    if request.surface.is_interactive() && is_plan_document_extension(target) {
                        return PermissionVerdict::Ask(PermissionPrompt {
                            reason: format!("本轮仅允许修改此规划文档：{}", target.display()),
                        });
                    }
                    return PermissionVerdict::Deny(DenyReason::new(
                            "plan_write_path_denied",
                            format!(
                                "Plan 模式只能写入 doc/plan、docs/plan、.wyj-code/plans，或本轮明确授权的单个文档：{}",
                                target.display()
                            ),
                        ));
                }
                if request.tool_name == "Bash" {
                    let command = request
                        .input
                        .get("command")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    return match validate_plan_read_only_command(command) {
                        Ok(()) => PermissionVerdict::Allow,
                        Err(message) => PermissionVerdict::Deny(DenyReason::new(
                            "plan_bash_not_read_only",
                            message,
                        )),
                    };
                }
                PermissionVerdict::Allow
            }
            PermissionMode::Allowlist(allowed) => {
                if !allowed.contains(&request.tool_name) {
                    return PermissionVerdict::Deny(DenyReason::new(
                        "tool_not_allowlisted",
                        format!("工具 `{}` 未被本次进程显式授权", request.tool_name),
                    ));
                }
                if let Some(target) = write_target.as_deref() {
                    if !self
                        .allowed_write_roots
                        .iter()
                        .any(|root| path_is_within(target, root))
                    {
                        return PermissionVerdict::Deny(DenyReason::new(
                            "write_root_not_allowlisted",
                            format!("写入目标不在 --allow-write 授权范围：{}", target.display()),
                        ));
                    }
                }
                if let Some(domain) = network_domain(&request.tool_name, &request.input) {
                    if !self
                        .allowed_domains
                        .iter()
                        .any(|allowed| domain_matches(&domain, allowed))
                    {
                        return PermissionVerdict::Deny(DenyReason::new(
                            "network_domain_not_allowlisted",
                            format!("网络目标未被 --allow-network 授权：{domain}"),
                        ));
                    }
                }
                PermissionVerdict::Allow
            }
            PermissionMode::AutoApprove => PermissionVerdict::Allow,
            PermissionMode::Prompt => {
                if is_side_effect_tool(&request.tool_name, &request.input) {
                    if request.surface.is_interactive() {
                        PermissionVerdict::Ask(PermissionPrompt {
                            reason: "该工具可能产生外部副作用，需要用户确认".to_string(),
                        })
                    } else {
                        PermissionVerdict::Deny(DenyReason::new(
                            "interactive_approval_unavailable",
                            format!(
                                "工具 `{}` 需要确认，但当前运行表面没有交互审批通道",
                                request.tool_name
                            ),
                        ))
                    }
                } else {
                    PermissionVerdict::Allow
                }
            }
        }
    }

    pub fn add_allowed_write_root(
        &mut self,
        cwd: &Path,
        raw: &Path,
    ) -> Result<PathBuf, DenyReason> {
        let raw = raw.to_string_lossy();
        let resolved = safe_resolve_write_target(cwd, &raw)?;
        self.allowed_write_roots.push(resolved.clone());
        Ok(resolved)
    }

    pub fn add_plan_document_grant(
        &mut self,
        cwd: &Path,
        raw: &Path,
    ) -> Result<PathBuf, DenyReason> {
        let raw = raw.to_string_lossy();
        let resolved = safe_resolve_write_target(cwd, &raw)?;
        if !is_plan_document_extension(&resolved) {
            return Err(DenyReason::new(
                "plan_write_extension_denied",
                "Plan 文档授权只接受 .md、.mdx、.txt、.rst",
            ));
        }
        self.plan_document_grants.insert(resolved.clone());
        Ok(resolved)
    }

    fn plan_write_allowed(&self, cwd: &Path, target: &Path) -> bool {
        if !is_plan_document_extension(target) {
            return false;
        }
        if self.plan_document_grants.contains(target) {
            return true;
        }
        let project = crate::project::project_root(cwd);
        [
            project.join("doc/plan"),
            project.join("docs/plan"),
            project.join(".wyj-code/plans"),
        ]
        .iter()
        .any(|root| path_is_within(target, root))
    }
}

pub fn safe_resolve_write_target(cwd: &Path, raw: &str) -> Result<PathBuf, DenyReason> {
    let raw = raw.trim();
    if raw.is_empty() || raw.contains('\0') {
        return Err(DenyReason::new(
            "invalid_write_target",
            "写入路径为空或包含 NUL",
        ));
    }
    let path = Path::new(raw);
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(DenyReason::new(
            "parent_path_denied",
            "写入路径不得包含 `..`",
        ));
    }

    let base = crate::project::project_root(cwd);
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    let resolved = canonicalize_existing_or_parent(&candidate).map_err(|error| {
        DenyReason::new(
            "write_target_resolution_failed",
            format!("无法安全解析写入路径：{error}"),
        )
    })?;

    if is_protected_write_target(&resolved) {
        return Err(DenyReason::new(
            "protected_write_target",
            format!("拒绝写入受保护路径：{}", resolved.display()),
        ));
    }

    if let Ok(metadata) = std::fs::symlink_metadata(&resolved) {
        let file_type = metadata.file_type();
        if file_type.is_dir() || !file_type.is_file() {
            return Err(DenyReason::new(
                "special_write_target",
                "写入目标必须是普通文件，不能是目录、设备、FIFO 或 socket",
            ));
        }
    }
    Ok(resolved)
}

fn canonicalize_existing_or_parent(path: &Path) -> std::io::Result<PathBuf> {
    if path.exists() {
        return path.canonicalize();
    }
    let mut cursor = path;
    let mut missing = Vec::new();
    while !cursor.exists() {
        let name = cursor.file_name().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "路径没有可解析的父目录")
        })?;
        missing.push(name.to_os_string());
        cursor = cursor.parent().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "路径没有可解析的父目录")
        })?;
    }
    let mut resolved = cursor.canonicalize()?;
    for component in missing.iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

fn is_protected_write_target(path: &Path) -> bool {
    let components: Vec<String> = path
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .map(|component| component.to_ascii_lowercase())
        .collect();
    if components.iter().any(|component| component == ".git") {
        return true;
    }
    if let Some(index) = components
        .iter()
        .position(|component| component == ".wyj-code")
    {
        if components.get(index + 1).map(String::as_str) != Some("plans") {
            return true;
        }
    }
    let file_name = components.last().map(String::as_str).unwrap_or_default();
    file_name == ".env"
        || file_name.starts_with(".env.")
        || matches!(
            file_name,
            "credentials" | ".netrc" | "id_rsa" | "id_ed25519"
        )
        || matches!(
            Path::new(file_name)
                .extension()
                .and_then(|ext| ext.to_str()),
            Some("pem" | "key" | "p12" | "pfx")
        )
}

fn path_is_within(path: &Path, root: &Path) -> bool {
    if let Ok(root) = canonicalize_existing_or_parent(root) {
        path == root || path.starts_with(root)
    } else {
        false
    }
}

fn is_plan_document_extension(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("md" | "mdx" | "txt" | "rst")
    )
}

fn is_file_write_tool(name: &str) -> bool {
    matches!(name, "Write" | "Edit")
}

fn is_process_tool(name: &str) -> bool {
    matches!(name, "Bash" | "BashOutput" | "KillShell" | "Agent")
}

fn is_side_effect_tool(name: &str, input: &Value) -> bool {
    if is_file_write_tool(name) {
        return true;
    }
    match name {
        "Bash" | "KillShell" | "Agent" | "ExitPlanMode" => true,
        // PermissionPolicy runs before Tool::needs_permission. Keep the same
        // read-only boundary here so headless surfaces can observe/inspect the
        // desktop while mutations still fail closed without an approval UI.
        "computer" => !matches!(
            input.get("action").and_then(Value::as_str),
            Some("screenshot" | "zoom" | "cursor_position" | "wait")
        ),
        "app_computer" => !matches!(
            input.get("action").and_then(Value::as_str),
            Some("list_windows" | "screenshot" | "inspect_element")
        ),
        _ => false,
    }
}

fn network_domain(tool_name: &str, input: &Value) -> Option<String> {
    match tool_name {
        "WebFetch" => input
            .get("url")
            .and_then(Value::as_str)
            .and_then(|raw| url::Url::parse(raw).ok())
            .and_then(|url| url.host_str().map(str::to_ascii_lowercase)),
        "WebSearch" => Some("api.tavily.com".to_string()),
        _ => None,
    }
}

fn domain_matches(domain: &str, allowed: &str) -> bool {
    let allowed = allowed
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_start_matches('.')
        .trim_end_matches('/')
        .to_ascii_lowercase();
    !allowed.is_empty() && (domain == allowed || domain.ends_with(&format!(".{allowed}")))
}

pub fn validate_plan_read_only_command(command: &str) -> Result<(), String> {
    let command = command.trim();
    if command.is_empty() {
        return Err("Plan Bash 命令为空".to_string());
    }
    for forbidden in ["\n", ">", "<", ";", "&&", "||", "`", "$(", "${"] {
        if command.contains(forbidden) {
            return Err(format!("Plan Bash 拒绝 shell 控制符或重定向：{forbidden}"));
        }
    }

    for segment in command.split('|') {
        let tokens = shell_words(segment)?;
        let Some(program) = tokens.first().map(String::as_str) else {
            return Err("Plan Bash 管道包含空命令".to_string());
        };
        if program.contains('/') || program.contains('=') {
            return Err(format!("Plan Bash 只允许受控只读命令，拒绝：{program}"));
        }
        match program {
            "pwd" | "ls" | "cat" | "head" | "tail" | "wc" | "stat" | "file" | "rg" | "grep" => {}
            "find" => {
                if tokens.iter().any(|token| {
                    matches!(
                        token.as_str(),
                        "-delete" | "-exec" | "-execdir" | "-ok" | "-okdir" | "-fprint"
                    )
                }) {
                    return Err("Plan Bash 拒绝 find 的写入/执行参数".to_string());
                }
            }
            "git" => validate_read_only_git(&tokens[1..])?,
            _ => return Err(format!("Plan Bash 不允许命令：{program}")),
        }
    }
    Ok(())
}

fn validate_read_only_git(args: &[String]) -> Result<(), String> {
    let Some(subcommand) = args.first().map(String::as_str) else {
        return Err("Plan Bash 的 git 命令缺少只读子命令".to_string());
    };
    if !matches!(subcommand, "status" | "diff" | "log" | "show" | "grep") {
        return Err(format!("Plan Bash 不允许 git {subcommand}"));
    }
    if args.iter().any(|arg| {
        arg == "--output"
            || arg.starts_with("--output=")
            || arg == "--ext-diff"
            || arg.starts_with("--config-env")
    }) {
        return Err("Plan Bash 拒绝可能写文件或执行外部程序的 git 参数".to_string());
    }
    Ok(())
}

/// 足够解析受控只读命令的引号/转义；控制符在调用前已被拒绝。
fn shell_words(input: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    for ch in input.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        if matches!(ch, '\'' | '"') {
            if quote == Some(ch) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(ch);
            } else {
                current.push(ch);
            }
            continue;
        }
        if ch.is_whitespace() && quote.is_none() {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    if escaped || quote.is_some() {
        return Err("Plan Bash 命令包含未闭合的引号或转义".to_string());
    }
    if !current.is_empty() {
        words.push(current);
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(mode: PermissionMode, cwd: &Path, tool: &str, input: Value) -> PermissionRequest {
        PermissionRequest {
            mode,
            surface: ExecutionSurface::HeadlessRepl,
            tool_name: tool.to_string(),
            input,
            cwd: cwd.to_path_buf(),
            sandbox_available: true,
        }
    }

    #[test]
    fn prompt_without_ui_denies_side_effects_but_allows_reads() {
        let policy = PermissionPolicy::default();
        let cwd = std::env::temp_dir();
        assert!(matches!(
            policy.evaluate(&request(PermissionMode::Prompt, &cwd, "Read", json!({}))),
            PermissionVerdict::Allow
        ));
        assert!(matches!(
            policy.evaluate(&request(
                PermissionMode::Prompt,
                &cwd,
                "Bash",
                json!({"command": "ls"})
            )),
            PermissionVerdict::Deny(_)
        ));
        for (tool, input) in [
            ("computer", json!({"action": "screenshot"})),
            ("computer", json!({"action": "zoom"})),
            ("computer", json!({"action": "cursor_position"})),
            ("computer", json!({"action": "wait"})),
            ("app_computer", json!({"action": "list_windows"})),
            ("app_computer", json!({"action": "screenshot"})),
            ("app_computer", json!({"action": "inspect_element"})),
        ] {
            assert!(
                matches!(
                    policy.evaluate(&request(PermissionMode::Prompt, &cwd, tool, input)),
                    PermissionVerdict::Allow
                ),
                "只读 computer-use 动作 {tool} 不应要求 headless 交互审批"
            );
        }
        for (tool, input) in [
            ("computer", json!({"action": "left_click"})),
            ("computer", json!({"action": "unknown"})),
            ("computer", json!({})),
            ("app_computer", json!({"action": "click"})),
            ("app_computer", json!({"action": "set_text"})),
            ("app_computer", json!({"action": "unknown"})),
            ("app_computer", json!({})),
        ] {
            assert!(
                matches!(
                    policy.evaluate(&request(PermissionMode::Prompt, &cwd, tool, input)),
                    PermissionVerdict::Deny(_)
                ),
                "有副作用的 computer-use 动作 {tool} 在 headless 下必须 fail closed"
            );
        }
    }

    #[test]
    fn interactive_prompt_only_asks_for_mutating_computer_actions() {
        let policy = PermissionPolicy::default();
        let cwd = std::env::temp_dir();
        let mut read = request(
            PermissionMode::Prompt,
            &cwd,
            "app_computer",
            json!({"action": "inspect_element"}),
        );
        read.surface = ExecutionSurface::TuiInteractive;
        assert!(matches!(policy.evaluate(&read), PermissionVerdict::Allow));

        let mut mutation = request(
            PermissionMode::Prompt,
            &cwd,
            "app_computer",
            json!({"action": "click"}),
        );
        mutation.surface = ExecutionSurface::TuiInteractive;
        assert!(matches!(
            policy.evaluate(&mutation),
            PermissionVerdict::Ask(_)
        ));
    }

    #[test]
    fn plan_allows_only_document_roots_and_safe_extensions() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".git")).unwrap();
        let allowed: HashSet<String> = ["Write"].into_iter().map(str::to_string).collect();
        let policy = PermissionPolicy::default();
        let mode = PermissionMode::Plan(allowed.clone());
        assert!(matches!(
            policy.evaluate(&request(
                mode.clone(),
                repo.path(),
                "Write",
                json!({"file_path": "doc/plan/v1.md"})
            )),
            PermissionVerdict::Allow
        ));
        for path in [
            "src/lib.rs",
            "README.md",
            "doc/plan/code.rs",
            ".wyj-code/config.toml",
            "doc/plan/../README.md",
        ] {
            assert!(matches!(
                policy.evaluate(&request(
                    mode.clone(),
                    repo.path(),
                    "Write",
                    json!({"file_path": path})
                )),
                PermissionVerdict::Deny(_)
            ));
        }
    }

    #[test]
    fn explicit_plan_grant_is_exact_not_directory_wide() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".git")).unwrap();
        let mut policy = PermissionPolicy::default();
        policy
            .add_plan_document_grant(repo.path(), Path::new("README.md"))
            .unwrap();
        let allowed: HashSet<String> = ["Write"].into_iter().map(str::to_string).collect();
        assert!(matches!(
            policy.evaluate(&request(
                PermissionMode::Plan(allowed.clone()),
                repo.path(),
                "Write",
                json!({"file_path": "README.md"})
            )),
            PermissionVerdict::Allow
        ));
        assert!(matches!(
            policy.evaluate(&request(
                PermissionMode::Plan(allowed),
                repo.path(),
                "Write",
                json!({"file_path": "CHANGELOG.md"})
            )),
            PermissionVerdict::Deny(_)
        ));
    }

    #[test]
    fn plan_bash_is_conservative_and_cannot_write_via_shell() {
        for command in [
            "git status --short",
            "git diff -- src/lib.rs",
            "rg -n TODO crates | head -20",
            "find crates -name '*.rs'",
        ] {
            assert!(
                validate_plan_read_only_command(command).is_ok(),
                "{command}"
            );
        }
        for command in [
            "echo x > README.md",
            "cat a | tee b",
            "sed -i s/a/b/ file",
            "python -c 'open(\"x\",\"w\")'",
            "git reset --hard",
            "find . -delete",
            "bash -c 'touch x'",
        ] {
            assert!(
                validate_plan_read_only_command(command).is_err(),
                "{command}"
            );
        }
    }

    #[test]
    fn headless_network_requires_domain_scope_in_addition_to_tool_name() {
        let cwd = std::env::temp_dir();
        let allowed: HashSet<String> = ["WebFetch"].into_iter().map(str::to_string).collect();
        let mut policy = PermissionPolicy::default();
        let req = request(
            PermissionMode::Allowlist(allowed.clone()),
            &cwd,
            "WebFetch",
            json!({"url": "https://docs.example.com/page"}),
        );
        assert!(matches!(policy.evaluate(&req), PermissionVerdict::Deny(_)));
        policy.allowed_domains.insert("example.com".to_string());
        assert!(matches!(policy.evaluate(&req), PermissionVerdict::Allow));
    }

    #[cfg(unix)]
    #[test]
    fn plan_write_rejects_symlink_escape_for_existing_and_new_files() {
        use std::os::unix::fs::symlink;

        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".git")).unwrap();
        std::fs::create_dir_all(repo.path().join("doc/plan")).unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("existing.md"), "secret").unwrap();
        symlink(outside.path(), repo.path().join("doc/plan/link")).unwrap();

        let allowed: HashSet<String> = ["Write"].into_iter().map(str::to_string).collect();
        let policy = PermissionPolicy::default();
        for target in ["doc/plan/link/existing.md", "doc/plan/link/new.md"] {
            assert!(matches!(
                policy.evaluate(&request(
                    PermissionMode::Plan(allowed.clone()),
                    repo.path(),
                    "Write",
                    json!({"file_path": target})
                )),
                PermissionVerdict::Deny(_)
            ));
        }
    }
}
