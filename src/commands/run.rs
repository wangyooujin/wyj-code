//! run 子命令。

use crate::launcher;
use crate::merge;
use crate::store;
use anyhow::Result;

/// 解析 run 的 trailing tokens,分离 profile 名与 claude 透传参数。
///
/// 规则:
/// - 首个 token 若为 `--`:消费它,profile=None,其余全为 claude 参数。
/// - 首个 token 若以 `-` 开头:profile=None,全部(含该 token)为 claude 参数。
/// - 否则:首个 token 为 profile 名;其后若紧跟 `--` 则消费,其余为 claude 参数。
fn split_profile_and_args(args: Vec<String>) -> (Option<String>, Vec<String>) {
    let mut it = args.into_iter();
    let first = match it.next() {
        None => return (None, vec![]),
        Some(f) => f,
    };
    if first == "--" {
        return (None, it.collect());
    }
    if first.starts_with('-') {
        // 无 profile,把 first 放回
        let mut rest: Vec<String> = vec![first];
        rest.extend(it);
        return (None, rest);
    }
    // first 是 profile 名
    let profile = Some(first);
    // 消费紧随的 `--`(若存在)
    let mut collected: Vec<String> = it.collect();
    if collected.first().map(|s| s == "--").unwrap_or(false) {
        collected.remove(0);
    }
    (profile, collected)
}

pub fn run(args: Vec<String>) -> Result<()> {
    let (profile, claude_args) = split_profile_and_args(args);
    let config = store::load()?;
    let name = store::resolve_profile_name(&config, profile.as_deref())?;
    let profile = config
        .get_profile(&name)
        .ok_or_else(|| anyhow::anyhow!("内部错误:profile `{}` 未找到", name))?;
    let mut env = merge::merge_env(&config, profile);
    crate::keychain::maybe_overlay_token(&profile, &mut env)?;
    let claude = launcher::resolve_claude(&config)?;
    launcher::exec_claude(&claude, &env, &claude_args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_default_with_separator() {
        let (p, a) = split_profile_and_args(vec!["--".into(), "--version".into()]);
        assert_eq!(p, None);
        assert_eq!(a, vec!["--version"]);
    }

    #[test]
    fn split_default_with_flag_first() {
        let (p, a) = split_profile_and_args(vec!["--version".into()]);
        assert_eq!(p, None);
        assert_eq!(a, vec!["--version"]);
    }

    #[test]
    fn split_profile_then_args() {
        let (p, a) = split_profile_and_args(vec!["huoshan".into(), "--dangerously-skip-permissions".into()]);
        assert_eq!(p.as_deref(), Some("huoshan"));
        assert_eq!(a, vec!["--dangerously-skip-permissions"]);
    }

    #[test]
    fn split_profile_with_separator() {
        let (p, a) = split_profile_and_args(vec!["huoshan".into(), "--".into(), "--version".into()]);
        assert_eq!(p.as_deref(), Some("huoshan"));
        assert_eq!(a, vec!["--version"]);
    }

    #[test]
    fn split_empty() {
        let (p, a) = split_profile_and_args(vec![]);
        assert_eq!(p, None);
        assert!(a.is_empty());
    }
}
