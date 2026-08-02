use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{bail, Result};
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Subcommand, Debug)]
pub enum ReviewCommand {
    /// Scan added diff lines and emit auditable JSON evidence.
    Run {
        #[arg(long, default_value = "HEAD^")]
        base: String,
        #[arg(long, default_value = "HEAD")]
        head: String,
        #[arg(long)]
        output: Option<PathBuf>,
        #[arg(long)]
        json: bool,
        #[arg(long, value_parser = ["low", "medium", "high", "critical"])]
        fail_on: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewSeverity {
    Low,
    Medium,
    High,
    Critical,
}

impl ReviewSeverity {
    fn rank(self) -> u8 {
        match self {
            Self::Low => 1,
            Self::Medium => 2,
            Self::High => 3,
            Self::Critical => 4,
        }
    }

    fn parse(value: &str) -> Self {
        match value {
            "critical" => Self::Critical,
            "high" => Self::High,
            "medium" => Self::Medium,
            _ => Self::Low,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewFinding {
    pub rule_id: String,
    pub severity: ReviewSeverity,
    pub category: String,
    pub path: PathBuf,
    pub line: u32,
    pub message: String,
    /// Evidence is always redacted for secret rules and bounded for every other rule.
    pub evidence: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReviewDiffSummary {
    pub changed_files: usize,
    pub insertions: u64,
    pub deletions: u64,
    pub paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewReport {
    pub schema_version: u32,
    pub generated_at: String,
    pub repository_root: PathBuf,
    pub base: String,
    pub head: String,
    pub resolved_base: String,
    pub resolved_head: String,
    pub patch_sha256: String,
    pub diff: ReviewDiffSummary,
    pub findings: Vec<ReviewFinding>,
}

pub fn run(command: ReviewCommand, cwd: &Path) -> Result<()> {
    match command {
        ReviewCommand::Run {
            base,
            head,
            output,
            json,
            fail_on,
        } => {
            let report = generate(cwd, &base, &head)?;
            let serialized = serde_json::to_vec_pretty(&report)?;
            if let Some(path) = output {
                if let Some(parent) = path
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(path, &serialized)?;
            }
            if json {
                println!("{}", String::from_utf8_lossy(&serialized));
            } else {
                println!(
                    "review {}...{}: {} files, +{} -{}, {} finding(s)",
                    report.base,
                    report.head,
                    report.diff.changed_files,
                    report.diff.insertions,
                    report.diff.deletions,
                    report.findings.len()
                );
                for finding in &report.findings {
                    println!(
                        "  {:?}\t{}:{}\t{}\t{}",
                        finding.severity,
                        finding.path.display(),
                        finding.line,
                        finding.rule_id,
                        finding.message
                    );
                }
            }
            if let Some(threshold) = fail_on {
                let threshold = ReviewSeverity::parse(&threshold).rank();
                if report
                    .findings
                    .iter()
                    .any(|finding| finding.severity.rank() >= threshold)
                {
                    bail!("review findings reached the configured failure threshold")
                }
            }
        }
    }
    Ok(())
}

pub fn generate(cwd: &Path, base: &str, head: &str) -> Result<ReviewReport> {
    let repository_root = std::fs::canonicalize(wyj_core::project_root(cwd))?;
    let resolved_base = git_text(&repository_root, &["rev-parse", "--verify", base])?;
    let resolved_head = git_text(&repository_root, &["rev-parse", "--verify", head])?;
    let range = format!("{}...{}", resolved_base.trim(), resolved_head.trim());
    let patch = git_text(
        &repository_root,
        &[
            "-c",
            "core.quotePath=false",
            "diff",
            "--no-ext-diff",
            "--no-prefix",
            "--unified=0",
            &range,
            "--",
        ],
    )?;
    let numstat = git_text(
        &repository_root,
        &["diff", "--no-ext-diff", "--numstat", &range, "--"],
    )?;
    let changed_paths = git_output(
        &repository_root,
        &[
            "-c",
            "core.quotePath=false",
            "diff",
            "--no-ext-diff",
            "--name-only",
            "-z",
            &range,
            "--",
        ],
    )?;
    let mut findings = scan_patch(&patch);
    findings.sort_by(|left, right| {
        right
            .severity
            .rank()
            .cmp(&left.severity.rank())
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.line.cmp(&right.line))
    });
    Ok(ReviewReport {
        schema_version: 1,
        generated_at: wyj_core::now_iso(),
        repository_root,
        base: base.to_string(),
        head: head.to_string(),
        resolved_base: resolved_base.trim().to_string(),
        resolved_head: resolved_head.trim().to_string(),
        patch_sha256: sha256_hex(patch.as_bytes()),
        diff: parse_diff_summary(&numstat, &changed_paths.stdout),
        findings,
    })
}

fn scan_patch(patch: &str) -> Vec<ReviewFinding> {
    let mut path = PathBuf::new();
    let mut new_line = 0_u32;
    let mut findings = Vec::new();
    for line in patch.lines() {
        if let Some(value) = parse_patch_path(line) {
            path = value;
            continue;
        }
        if line.starts_with("@@") {
            new_line = parse_new_hunk_line(line).unwrap_or(0);
            continue;
        }
        if line.starts_with('+') && !line.starts_with("+++") {
            let added = &line[1..];
            findings.extend(scan_added_line(&path, new_line, added));
            new_line = new_line.saturating_add(1);
        } else if !line.starts_with('-') && !line.starts_with("diff --git") {
            new_line = new_line.saturating_add(1);
        }
    }
    findings
}

fn scan_added_line(path: &Path, line: u32, added: &str) -> Vec<ReviewFinding> {
    let mut findings = Vec::new();
    let lower = added.to_ascii_lowercase();
    let rule_definition = path.file_name().and_then(|name| name.to_str()) == Some("review_cmd.rs")
        && added.contains("review-rule-definition");

    let private_key_markers = ["-----BEGIN ", "PRIVATE KEY-----"]; // review-rule-definition
    let secret_rule = if private_key_markers
        .iter()
        .all(|marker| added.contains(marker))
    {
        Some((
            "secret.private_key",
            ReviewSeverity::Critical,
            "private key material",
        ))
    } else if contains_aws_access_key(added) {
        Some((
            "secret.aws_access_key",
            ReviewSeverity::Critical,
            "AWS access key-like token",
        ))
    } else if contains_sk_token(added) {
        Some((
            "secret.api_token",
            ReviewSeverity::Critical,
            "API token-like value",
        ))
    } else if assignment_has_long_secret(&lower, added) {
        Some((
            "secret.assignment",
            ReviewSeverity::High,
            "credential assignment with a long literal",
        ))
    } else {
        None
    };
    let redact_evidence = secret_rule.is_some();
    if let Some((rule_id, severity, message)) = secret_rule.filter(|_| !rule_definition) {
        findings.push(ReviewFinding {
            rule_id: rule_id.to_string(),
            severity,
            category: "secret".to_string(),
            path: path.to_path_buf(),
            line,
            message: message.to_string(),
            evidence: format!("redacted added line ({} bytes)", added.len()),
        });
    }

    let dangerous = [
        (
            "shell.rm_rf",
            "rm -rf", // review-rule-definition
            ReviewSeverity::Critical,
            "recursive forced deletion",
        ),
        (
            "shell.no_preserve_root",
            "--no-preserve-root", // review-rule-definition
            ReviewSeverity::Critical,
            "root deletion guard disabled",
        ),
        (
            "shell.curl_pipe",
            "curl",
            ReviewSeverity::High,
            "download piped into a shell",
        ),
        (
            "shell.wget_pipe",
            "wget",
            ReviewSeverity::High,
            "download piped into a shell",
        ),
        (
            "shell.chmod_777",
            "chmod 777", // review-rule-definition
            ReviewSeverity::High,
            "world-writable permissions",
        ),
    ];
    for (rule_id, needle, severity, message) in dangerous {
        let matched = if matches!(rule_id, "shell.curl_pipe" | "shell.wget_pipe") {
            lower.contains(needle)
                && (lower.contains("| sh") || lower.contains("| bash") || lower.contains("| zsh"))
        } else {
            lower.contains(needle)
        };
        if matched && !rule_definition {
            findings.push(finding(
                rule_id,
                severity,
                "dangerous_shell",
                path,
                line,
                message,
                evidence_line(added, redact_evidence),
            ));
        }
    }

    for (rule_id, needle, message) in [
        (
            "permission.write_all",
            "permissions: write-all", // review-rule-definition
            "GitHub token receives write-all permissions",
        ),
        (
            "permission.pull_request_target", // review-rule-definition
            "pull_request_target",            // review-rule-definition
            "workflow executes in privileged pull_request_target context", // review-rule-definition
        ),
        (
            "permission.persist_credentials",
            "persist-credentials: true", // review-rule-definition
            "checkout persists repository credentials",
        ),
        (
            "permission.sandbox_disabled",
            "require_sandbox = false", // review-rule-definition
            "sandbox requirement is explicitly disabled",
        ),
        (
            "permission.unsandboxed_default",
            "allow_unsandboxed_fallback = true", // review-rule-definition
            "unsandboxed fallback is enabled by default",
        ),
        (
            "permission.bypass_default",
            "bypass_permissions = true", // review-rule-definition
            "permission bypass is enabled by default",
        ),
    ] {
        if lower.contains(needle) && !rule_definition {
            findings.push(finding(
                rule_id,
                ReviewSeverity::High,
                "permission_relaxation",
                path,
                line,
                message,
                evidence_line(added, redact_evidence),
            ));
        }
    }

    if contains_nonlocal_http_url(&lower) && !rule_definition {
        findings.push(finding(
            "network.insecure_http",
            ReviewSeverity::Medium,
            "network",
            path,
            line,
            "non-local plaintext HTTP endpoint",
            evidence_line(added, redact_evidence),
        ));
    }
    findings
}

fn evidence_line(value: &str, redact: bool) -> &str {
    if redact {
        "[redacted: line contains potential secret]"
    } else {
        value
    }
}

fn finding(
    rule_id: &str,
    severity: ReviewSeverity,
    category: &str,
    path: &Path,
    line: u32,
    message: &str,
    evidence: &str,
) -> ReviewFinding {
    ReviewFinding {
        rule_id: rule_id.to_string(),
        severity,
        category: category.to_string(),
        path: path.to_path_buf(),
        line,
        message: message.to_string(),
        evidence: evidence.chars().take(240).collect(),
    }
}

fn contains_aws_access_key(value: &str) -> bool {
    value.as_bytes().windows(20).any(|window| {
        window.starts_with(b"AKIA")
            && window
                .iter()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
    })
}

fn contains_sk_token(value: &str) -> bool {
    value
        .split(|ch: char| ch.is_whitespace() || matches!(ch, '"' | '\'' | '=' | ':' | ','))
        .any(|token| {
            token.starts_with("sk-")
                && token.len() >= 24
                && token
                    .bytes()
                    .skip(3)
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        })
}

fn assignment_has_long_secret(lower: &str, original: &str) -> bool {
    const KEY_LIKE: [&str; 5] = ["api_key", "apikey", "secret", "access_token", "auth_token"];
    original.char_indices().any(|(index, separator)| {
        if !matches!(separator, '=' | ':') {
            return false;
        }
        let left = &lower[..index];
        let key_like = if separator == '=' {
            KEY_LIKE.iter().any(|needle| left.contains(needle))
        } else {
            let key = left
                .trim()
                .trim_matches(|ch: char| matches!(ch, '"' | '\'' | '{' | '['));
            !key.chars().any(char::is_whitespace)
                && KEY_LIKE.iter().any(|needle| key.contains(needle))
        };
        if !key_like {
            return false;
        }
        let value = &original[index + separator.len_utf8()..];
        let value = value.trim_matches(|ch: char| ch.is_whitespace() || ch == '"' || ch == '\'');
        value.len() >= 24
            && !value.starts_with('$')
            && !value.contains("secrets.")
            && !value.contains("<redacted>")
            && !value.contains("placeholder")
    })
}

fn contains_nonlocal_http_url(lower: &str) -> bool {
    let mut remaining = lower;
    while let Some(index) = remaining.find("http://") {
        let after = &remaining[index + "http://".len()..];
        if let Some(stripped) = after.strip_prefix("[::1]") {
            remaining = stripped;
            continue;
        }
        let host_port = after
            .split(|ch: char| {
                ch.is_whitespace() || matches!(ch, '/' | '"' | '\'' | ')' | '>' | '`' | ',' | ';')
            })
            .next()
            .unwrap_or_default();
        let host = host_port.split(':').next().unwrap_or_default();
        if host.is_empty() {
            remaining = after;
            continue;
        }
        let local = host == "localhost"
            || host.ends_with(".localhost")
            || host == "0.0.0.0"
            || host.starts_with("127.");
        let reserved = matches!(host, "example.com" | "example.net" | "example.org")
            || host.ends_with(".example.com")
            || host.ends_with(".example.net")
            || host.ends_with(".example.org")
            || host.ends_with(".example")
            || host.ends_with(".invalid")
            || host.ends_with(".test");
        if !local && !reserved {
            return true;
        }
        remaining = &after[host_port.len()..];
    }
    false
}

fn parse_patch_path(line: &str) -> Option<PathBuf> {
    let raw = line.strip_prefix("+++ ")?.split('\t').next()?;
    if raw == "/dev/null" {
        return None;
    }
    let decoded = if raw.starts_with('"') {
        serde_json::from_str::<String>(raw).ok()?
    } else {
        raw.to_string()
    };
    Some(PathBuf::from(
        decoded.strip_prefix("b/").unwrap_or(&decoded),
    ))
}

fn parse_new_hunk_line(line: &str) -> Option<u32> {
    let plus = line.split_whitespace().find(|part| part.starts_with('+'))?;
    plus.trim_start_matches('+').split(',').next()?.parse().ok()
}

fn parse_diff_summary(value: &str, path_bytes: &[u8]) -> ReviewDiffSummary {
    let mut summary = ReviewDiffSummary::default();
    for line in value.lines() {
        let mut fields = line.splitn(3, '\t');
        let insertions = fields.next().unwrap_or("-");
        let deletions = fields.next().unwrap_or("-");
        if fields.next().is_none() {
            continue;
        }
        summary.insertions = summary
            .insertions
            .saturating_add(insertions.parse::<u64>().unwrap_or(0));
        summary.deletions = summary
            .deletions
            .saturating_add(deletions.parse::<u64>().unwrap_or(0));
    }
    summary.paths = path_bytes
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| PathBuf::from(String::from_utf8_lossy(path).into_owned()))
        .collect();
    summary.changed_files = summary.paths.len();
    summary
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(64);
    for byte in digest {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn git_text(cwd: &Path, args: &[&str]) -> Result<String> {
    git_output(cwd, args).map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
}

fn git_output(cwd: &Path, args: &[&str]) -> Result<Output> {
    ensure_success(
        Command::new("git").arg("-C").arg(cwd).args(args).output()?,
        args,
    )
}

fn ensure_success(output: Output, args: &[&str]) -> Result<Output> {
    if output.status.success() {
        return Ok(output);
    }
    bail!(
        "git {} failed ({}): {}",
        args.join(" "),
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scanner_redacts_secrets_and_flags_dangerous_shell() {
        let token = ["sk-", "example_abcdefghijklmnopqrstuvwxyz"].concat();
        let command = ["curl https://example.invalid/install", " | sh"].concat(); // review-rule-definition
        let patch = format!(
            "diff --git a/x.sh b/x.sh\n+++ b/x.sh\n@@ -0,0 +1,2 @@\n+TOKEN={token}\n+{command}\n"
        );
        let findings = scan_patch(&patch);
        assert!(findings
            .iter()
            .any(|finding| finding.rule_id == "secret.api_token"));
        assert!(findings
            .iter()
            .any(|finding| finding.rule_id == "shell.curl_pipe"));
        assert!(findings
            .iter()
            .all(|finding| !finding.evidence.contains("sk-example")));

        let combined = format!(
            "diff --git a/x.sh b/x.sh\n+++ b/x.sh\n@@ -0,0 +1 @@\n+TOKEN={token} curl https://example.invalid/install | sh\n" // review-rule-definition
        );
        let findings = scan_patch(&combined);
        assert!(findings.len() >= 2);
        assert!(findings
            .iter()
            .all(|finding| !finding.evidence.contains("sk-example")));
    }

    #[test]
    fn scanner_detects_ci_permission_relaxation() {
        let patch = format!(
            "diff --git a/.github/workflows/x.yml b/.github/workflows/x.yml\n+++ b/.github/workflows/x.yml\n@@ -0,0 +1,2 @@\n+permissions: {}\n+  persist-credentials: {}\n",
            "write-all", "true"
        );
        let findings = scan_patch(&patch);
        assert_eq!(findings.len(), 2);
        assert!(findings
            .iter()
            .all(|finding| finding.category == "permission_relaxation"));
    }

    #[test]
    fn secret_environment_references_are_not_treated_as_literal_credentials() {
        for line in [
            "api_key = ${WYJ_CODE_API_KEY}",
            "secret: ${{ secrets.RELEASE_TOKEN }}",
            "auth_token = <redacted>",
        ] {
            assert!(!assignment_has_long_secret(
                &line.to_ascii_lowercase(),
                line
            ));
        }
    }

    #[test]
    fn assignment_detection_uses_the_left_hand_key_only() {
        assert!(assignment_has_long_secret(
            "api_key = \"abcdefghijklmnopqrstuvwxyz\"", // review-rule-definition
            "api_key = \"abcdefghijklmnopqrstuvwxyz\"", // review-rule-definition
        ));
        assert!(assignment_has_long_secret(
            "\"access_token\": \"abcdefghijklmnopqrstuvwxyz\"", // review-rule-definition
            "\"access_token\": \"abcdefghijklmnopqrstuvwxyz\"", // review-rule-definition
        ));
        assert!(!assignment_has_long_secret(
            "'feature.review.desc': 'secret evidence is redacted in this long description'",
            "'feature.review.desc': 'secret evidence is redacted in this long description'"
        ));
    }

    #[test]
    fn insecure_http_detection_ignores_parser_literals_and_reserved_test_hosts() {
        assert!(!contains_nonlocal_http_url(
            "target.strip_prefix(\"http://\")", // review-rule-definition
        ));
        assert!(!contains_nonlocal_http_url("http://www.example.com/docs"));
        assert!(!contains_nonlocal_http_url("http://localhost:8080/health"));
        assert!(contains_nonlocal_http_url(
            "http://downloads.vendor.cn/tool", // review-rule-definition
        ));
    }

    fn git(cwd: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn generate_handles_renames_binary_numstat_and_writes_evidence_before_failure() {
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "-q"]);
        git(repo.path(), &["config", "user.name", "wyj-test"]);
        git(
            repo.path(),
            &["config", "user.email", "wyj-test@example.invalid"],
        );
        std::fs::write(repo.path().join("old.sh"), "echo safe\n").unwrap();
        git(repo.path(), &["add", "old.sh"]);
        git(repo.path(), &["commit", "-qm", "base"]);

        std::fs::rename(
            repo.path().join("old.sh"),
            repo.path().join("renamed file.sh"),
        )
        .unwrap();
        let token = ["sk-", "generated_abcdefghijklmnopqrstuvwxyz"].concat();
        std::fs::write(
            repo.path().join("renamed file.sh"),
            format!("TOKEN={token}\n"),
        )
        .unwrap();
        std::fs::write(repo.path().join("asset.bin"), [0, 159, 146, 150]).unwrap();
        git(repo.path(), &["add", "-A"]);
        git(repo.path(), &["commit", "-qm", "review target"]);

        let report = generate(repo.path(), "HEAD^", "HEAD").unwrap();
        assert!((2..=3).contains(&report.diff.changed_files));
        assert!(report.diff.paths.contains(&PathBuf::from("asset.bin")));
        assert!(report
            .diff
            .paths
            .contains(&PathBuf::from("renamed file.sh")));
        assert!(
            report
                .findings
                .iter()
                .any(|finding| finding.rule_id == "secret.api_token"
                    && finding.path == Path::new("renamed file.sh")),
            "findings={:?}",
            report.findings
        );
        assert!(report.patch_sha256.len() == 64);
        assert!(report
            .findings
            .iter()
            .all(|finding| !finding.evidence.contains("sk-generated")));

        let output = repo.path().join("review-evidence.json");
        let error = run(
            ReviewCommand::Run {
                base: "HEAD^".to_string(),
                head: "HEAD".to_string(),
                output: Some(output.clone()),
                json: false,
                fail_on: Some("critical".to_string()),
            },
            repo.path(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("failure threshold"));
        let persisted: ReviewReport =
            serde_json::from_slice(&std::fs::read(output).unwrap()).unwrap();
        assert_eq!(persisted.patch_sha256, report.patch_sha256);
    }
}
