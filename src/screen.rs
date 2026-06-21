//! GNU screen 后端:用 `screen -ls` 列会话,`hardcopy` 抓屏,`stuff` 发按键。
//!
//! 注:screen 没有 tmux 那种 capture-pane,靠 hardcopy 把当前屏幕写到临时文件
//! 再读回。send_keys 用 `stuff`,把 "Enter" 映射成回车符 `\r`。

use crate::backend::Backend;
use anyhow::{Context, Result};
use std::process::Command;

/// GNU screen 后端实现。
pub struct ScreenBackend;

impl Backend for ScreenBackend {
    fn list_sessions(&self) -> Result<Vec<String>> {
        list_sessions()
    }
    fn capture_pane(&self, session: &str, lines: u32) -> Result<String> {
        capture_pane(session, lines)
    }
    fn send_keys(&self, session: &str, keys: &str) -> Result<()> {
        send_keys(session, keys)
    }
}

/// `screen -ls` 输出里每行形如 `\t12345.name\t(Detached)`,取 `.` 后的名字。
fn list_sessions() -> Result<Vec<String>> {
    let out = Command::new("screen")
        .arg("-ls")
        .output()
        .context("执行 screen -ls 失败(screen 没装?)")?;
    // screen -ls 在"有会话"时退出码是 1,"无会话"也可能非 0;统一解析 stdout。
    let text = String::from_utf8_lossy(&out.stdout);
    let mut names = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        // 形如 "12345.session-name   (Detached)"
        if let Some(first) = t.split_whitespace().next() {
            if let Some(dot) = first.find('.') {
                let name = &first[dot + 1..];
                if !name.is_empty() && first[..dot].chars().all(|c| c.is_ascii_digit()) {
                    names.push(name.to_string());
                }
            }
        }
    }
    Ok(names)
}

/// hardcopy 当前屏幕到临时文件再读回,取末尾 `lines` 行。
fn capture_pane(session: &str, lines: u32) -> Result<String> {
    let tmp = std::env::temp_dir().join(format!(
        "ccwatch-screen-{}-{}.txt",
        sanitize(session),
        std::process::id()
    ));
    let tmp_str = tmp.to_string_lossy().to_string();

    let status = Command::new("screen")
        .args(["-S", session, "-X", "hardcopy", &tmp_str])
        .status()
        .with_context(|| format!("执行 screen hardcopy -S {} 失败", session))?;
    if !status.success() {
        anyhow::bail!("screen hardcopy -S {} 失败", session);
    }

    let full = std::fs::read_to_string(&tmp).unwrap_or_default();
    std::fs::remove_file(&tmp).ok();

    // 取末尾 lines 行,并去掉 hardcopy 常见的尾部空白行。
    let all: Vec<&str> = full.lines().collect();
    let start = all.len().saturating_sub(lines as usize);
    Ok(all[start..].join("\n"))
}

/// `screen -X stuff` 发按键;"Enter" 映射成回车。
fn send_keys(session: &str, keys: &str) -> Result<()> {
    let payload = match keys {
        "Enter" => "\r".to_string(),
        other => other.to_string(),
    };
    let status = Command::new("screen")
        .args(["-S", session, "-X", "stuff", &payload])
        .status()
        .with_context(|| format!("执行 screen stuff -S {} 失败", session))?;
    if !status.success() {
        anyhow::bail!("screen stuff -S {} 失败", session);
    }
    Ok(())
}

/// 临时文件名里去掉路径分隔等危险字符。
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect()
}
