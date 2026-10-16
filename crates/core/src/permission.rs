//! 与 TUI/CLI 解耦的工具权限策略。

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionMode {
    /// Auto 模式（唯一需要判定的日常模式）：「要不要问人」交给
    /// `crate::auto_mode::AutoGate` 判定——确定性规则先行，拿不准的送模型
    /// 分类器，闸门自己也拿不准时交回逐次弹窗。
    ///
    /// 这里刻意返回 `Allow` 而不是新增一个 verdict 变体：真正的判定需要
    /// `.await` 模型，而 `evaluate` 是同步函数。Auto 闸门挂在 `Agent` 侧
    /// （`Agent::ensure_tool_permission`），本函数只负责**任何模式都不放行**
    /// 的那部分（受保护路径写入、Plan 白名单）。
    ///
    /// 历史上的 `Prompt`（逐次弹窗）变体已并入本变体：它与 Auto 的差别只是
    /// 「谁来回答要不要问人」，而 Auto 的闸门在判 `AskUser` 时本来就会落到
    /// 同一条弹窗路径（`ToolCtx::confirm_tool`）——无交互面时那里 fail-closed，
    /// 覆盖了原 `Prompt` 分支在 headless 下的职责。
    Auto,
    /// 显式 bypass。跳过询问，但不绕过受保护路径。
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
    pub plan_document_grants: HashSet<PathBuf>,
}

impl PermissionPolicy {
    pub fn evaluate(&self, request: &PermissionRequest) -> PermissionVerdict {
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
                PermissionVerdict::Allow
            }
            PermissionMode::AutoApprove => PermissionVerdict::Allow,
            // Auto 不在这里下结论：受保护路径已在上面的 `safe_resolve_write_target`
            // 里 deny 掉，剩下的「要不要问人」需要 await 分类器，由 Agent 侧的
            // AutoGate 负责。这里放行不代表动作会真的执行；闸门判 AskUser 时
            // 会落到 `ToolCtx::confirm_tool`，无交互面时它 fail-closed。
            PermissionMode::Auto => PermissionVerdict::Allow,
        }
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
        }
    }

    /// Auto 在 `evaluate` 层**只拦一件事**：受保护路径写入。
    ///
    /// 这是「Auto 会放行一切」的印象能成立的前提——没有这条，任何模式的
    /// `Allow` 都会变成绕过 `.env` / `.ssh/` / `.git` 的口子。工具自身要不要
    /// 审批（`Tool::needs_permission`，含 computer-use 的只读/副作用区分）不在
    /// 这一层，见 `crates/tools/src/computer.rs` 与 `app_computer.rs` 的测试。
    #[test]
    fn auto_blocks_protected_write_targets_but_defers_everything_else() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".git")).unwrap();
        std::fs::write(repo.path().join("secret.pem"), "x").unwrap();
        let policy = PermissionPolicy::default();
        let cwd = repo.path();

        // 受保护路径：`.git` 内写入 + 凭据后缀文件，任何模式都不放行。
        for path in [".git/config", "secret.pem", ".env", ".ssh/id_ed25519"] {
            assert!(
                matches!(
                    policy.evaluate(&request(
                        PermissionMode::Auto,
                        cwd,
                        "Write",
                        json!({ "file_path": path })
                    )),
                    PermissionVerdict::Deny(_)
                ),
                "Auto 下写 {path} 必须 deny"
            );
        }

        // 普通文件写与 Bash：这里放行，真正的判定交给 AutoGate / confirm_tool。
        assert!(matches!(
            policy.evaluate(&request(
                PermissionMode::Auto,
                cwd,
                "Write",
                json!({ "file_path": "src/lib.rs" })
            )),
            PermissionVerdict::Allow
        ));
        assert!(matches!(
            policy.evaluate(&request(
                PermissionMode::Auto,
                cwd,
                "Bash",
                json!({ "command": "git reset --hard" })
            )),
            PermissionVerdict::Allow
        ));
    }

    /// Auto 的 `Allow` 不能被误读成「无交互面也放行」：headless 下 `evaluate`
    /// 放行之后，`Agent::ensure_tool_permission` 仍会问闸门，闸门判 AskUser 时
    /// `ToolCtx::confirm_tool` 在没有 UI 通道时 fail-closed。这条链路端到端钉在
    /// `crates/tools/src/ctx.rs` 的 `sub_agent_bash_is_blocked_...`，这里只固定
    /// 它依赖的前置事实：Auto 不会在 policy 层自己把有副作用的动作拒掉。
    #[test]
    fn auto_leaves_side_effect_decision_to_the_gate_not_to_the_policy() {
        let policy = PermissionPolicy::default();
        let cwd = std::env::temp_dir();
        for (tool, input) in [
            ("Bash", json!({ "command": "rm -rf build" })),
            ("app_computer", json!({ "action": "click" })),
            ("computer", json!({ "action": "left_click" })),
        ] {
            assert!(
                matches!(
                    policy.evaluate(&request(PermissionMode::Auto, &cwd, tool, input)),
                    PermissionVerdict::Allow
                ),
                "{tool} 的副作用判定属于 AutoGate，policy 层不应代劳"
            );
        }
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
