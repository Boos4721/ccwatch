//! tmux 交互:list-sessions / capture-pane。
//!
//! 用同步 `std::process`——调用很快,daemon 的 async 循环里直接调也无妨。

use anyhow::{Context, Result};
use std::process::Command;

/// 列出所有 tmux 会话名。tmux 没起/无会话时返回空 vec。
pub fn list_sessions() -> Result<Vec<String>> {
    let out = Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}"])
        .output()
        .context("执行 tmux list-sessions 失败(tmux 没装?)")?;

    if !out.status.success() {
        // 没有 server / 没有会话时 tmux 返回非 0,视为空。
        return Ok(Vec::new());
    }

    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

/// 抓某会话 pane 末尾 `lines` 行文本。
pub fn capture_pane(session: &str, lines: u32) -> Result<String> {
    let start = format!("-{}", lines);
    let out = Command::new("tmux")
        .args([
            "capture-pane",
            "-t",
            session,
            "-p",
            "-S",
            &start,
        ])
        .output()
        .with_context(|| format!("执行 tmux capture-pane -t {} 失败", session))?;

    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("capture-pane -t {} 失败: {}", session, err.trim());
    }

    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// 按前缀过滤会话名(空前缀列表 = 全部通过)。
pub fn filter_by_prefix(sessions: Vec<String>, prefixes: &[String]) -> Vec<String> {
    if prefixes.is_empty() {
        return sessions;
    }
    sessions
        .into_iter()
        .filter(|s| prefixes.iter().any(|p| s.starts_with(p.as_str())))
        .collect()
}
