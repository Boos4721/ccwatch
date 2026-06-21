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

/// 构造往会话发文本的两步 tmux send-keys 参数(可靠提交的关键)。
///
/// 经验:`send-keys -l`(literal)只把文本塞进输入行,**不会提交**;要单独再发一次
/// 不带 `-l` 的 `Enter` 键名才会触发提交。所以拆成两步:
///   1. `send-keys -t <session> -l -- <message>`  (字面文本,`--` 防止以 `-` 开头的消息被当 flag)
///   2. `send-keys -t <session> Enter`             (提交)
///
/// 返回两组参数向量,便于单测与复用。
pub fn send_text_args(session: &str, message: &str) -> (Vec<String>, Vec<String>) {
    let type_args = vec![
        "send-keys".to_string(),
        "-t".to_string(),
        session.to_string(),
        "-l".to_string(),
        "--".to_string(),
        message.to_string(),
    ];
    let enter_args = vec![
        "send-keys".to_string(),
        "-t".to_string(),
        session.to_string(),
        "Enter".to_string(),
    ];
    (type_args, enter_args)
}

/// 往会话发一段文本并提交(两步 send-keys)。
pub fn send_text(session: &str, message: &str) -> Result<()> {
    let (type_args, enter_args) = send_text_args(session, message);

    let out = Command::new("tmux")
        .args(&type_args)
        .output()
        .with_context(|| format!("执行 tmux send-keys(文本)-t {} 失败", session))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("send-keys 文本到 {} 失败: {}", session, err.trim());
    }

    let out2 = Command::new("tmux")
        .args(&enter_args)
        .output()
        .with_context(|| format!("执行 tmux send-keys(Enter)-t {} 失败", session))?;
    if !out2.status.success() {
        let err = String::from_utf8_lossy(&out2.stderr);
        anyhow::bail!("send-keys Enter 到 {} 失败: {}", session, err.trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_text_args_two_step_literal_then_enter() {
        let (type_args, enter_args) = send_text_args("ccA", "hello world");
        // 第一步:字面文本,带 -l 和 -- 守卫。
        assert_eq!(
            type_args,
            vec!["send-keys", "-t", "ccA", "-l", "--", "hello world"]
        );
        // 第二步:单独的 Enter 键名(不带 -l,才会被当成回车提交)。
        assert_eq!(enter_args, vec!["send-keys", "-t", "ccA", "Enter"]);
    }

    #[test]
    fn send_text_args_message_starting_with_dash_is_guarded() {
        let (type_args, _) = send_text_args("s", "--help me");
        // `--` 之后即使消息以 - 开头也不会被 tmux 当 flag。
        let dash_pos = type_args.iter().position(|a| a == "--").unwrap();
        assert_eq!(type_args[dash_pos + 1], "--help me");
    }

    #[test]
    fn send_text_args_preserves_unicode() {
        let (type_args, _) = send_text_args("s", "继续推进 B-0");
        assert_eq!(type_args.last().unwrap(), "继续推进 B-0");
    }
}
