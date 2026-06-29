//! macOS Keychain 集成:通过系统 `security` CLI 存取 AUTH_TOKEN。
//! 仅 darwin 可用;其它平台函数返回错误。

use anyhow::{anyhow, Result};
use std::process::Command;

use crate::config::{EnvMap, Profile};

const SERVICE: &str = "wyj-code";

/// 若 profile.keychain == true,从 keychain 读取 token 覆盖 ANTHROPIC_AUTH_TOKEN。
/// 读取失败时返回错误(避免静默用空 token 启动)。
pub fn maybe_overlay_token(profile: &Profile, env: &mut EnvMap) -> Result<()> {
    if !profile.keychain {
        return Ok(());
    }
    match get(&profile.name) {
        Ok(token) if !token.is_empty() => {
            env.insert("ANTHROPIC_AUTH_TOKEN".to_string(), token);
            Ok(())
        }
        Ok(_) => Err(anyhow!(
            "profile `{}` 标记了 keychain=true,但 keychain 中 token 为空。用 `wyj-code token {} set` 存入。",
            profile.name,
            profile.name
        )),
        Err(e) => Err(e),
    }
}

#[cfg(target_os = "macos")]
fn security() -> Command {
    Command::new("/usr/bin/security")
}

/// 把 token 存入 keychain(覆盖同名项)。
pub fn set(profile: &str, token: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        // 先删除可能存在的旧项(忽略"不存在"错误)
        let _ = delete(profile);
        let status = security()
            .args(["add-generic-password", "-a", profile, "-s", SERVICE, "-w", token, "-U"])
            .status()
            .map_err(|e| anyhow!("启动 security 失败: {}", e))?;
        if !status.success() {
            return Err(anyhow!("写入 keychain 失败(退出码 {:?})", status.code()));
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (profile, token);
        Err(anyhow!("Keychain 仅在 macOS 可用"))
    }
}

/// 从 keychain 读取 token。
pub fn get(profile: &str) -> Result<String> {
    #[cfg(target_os = "macos")]
    {
        let output = security()
            .args(["find-generic-password", "-a", profile, "-s", SERVICE, "-w"])
            .output()
            .map_err(|e| anyhow!("启动 security 失败: {}", e))?;
        if !output.status.success() {
            return Err(anyhow!(
                "未在 keychain 找到 profile `{}` 的 token(用 `wyj-code token {} set` 存入)",
                profile,
                profile
            ));
        }
        let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(s)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = profile;
        Err(anyhow!("Keychain 仅在 macOS 可用"))
    }
}

/// 删除 keychain 中的 token(不存在不报错)。
pub fn delete(profile: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let status = security()
            .args(["delete-generic-password", "-a", profile, "-s", SERVICE])
            .status()
            .map_err(|e| anyhow!("启动 security 失败: {}", e))?;
        // 退出码非 0 通常表示该项不存在,视为成功
        let _ = status;
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = profile;
        Err(anyhow!("Keychain 仅在 macOS 可用"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "macos")]
    #[test]
    fn keychain_roundtrip() {
        let p = "__wyj_test_profile__";
        let tok = "test-token-abc-123";
        // 可能首次弹 keychain 授权框;CI 环境会失败,本地应放行
        if set(p, tok).is_err() {
            eprintln!("跳过:keychain 不可用(可能需授权)");
            return;
        }
        assert_eq!(get(p).unwrap(), tok);
        let _ = delete(p);
        assert!(get(p).is_err(), "删除后应取不到");
    }
}
