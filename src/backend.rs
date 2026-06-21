//! 后端抽象:把"列会话 / 抓 pane / 发按键"从 tmux 解耦出来。
//!
//! 现支持 tmux(默认)和 GNU screen。加新后端 = 实现 `Backend` trait + 在
//! `make_backend` 里挂一个分支。

use crate::screen::ScreenBackend;
use crate::tmux::TmuxBackend;
use anyhow::Result;

/// 终端复用器后端:ccwatch 与会话交互的全部入口。
pub trait Backend: Send + Sync {
    /// 列出所有会话名(没有会话/后端没起视为空)。
    fn list_sessions(&self) -> Result<Vec<String>>;
    /// 抓某会话末尾 `lines` 行文本。
    fn capture_pane(&self, session: &str, lines: u32) -> Result<String>;
    /// 给某会话发送按键。
    fn send_keys(&self, session: &str, keys: &str) -> Result<()>;
}

/// 按配置里的 backend 名构造后端。未知名回退到 tmux。
pub fn make_backend(name: &str) -> Box<dyn Backend> {
    match name {
        "screen" => Box::new(ScreenBackend),
        // tmux 或未知:默认 tmux。
        _ => Box::new(TmuxBackend),
    }
}

/// 按前缀过滤会话名(空前缀列表 = 全部通过)。后端无关,放这复用。
pub fn filter_by_prefix(sessions: Vec<String>, prefixes: &[String]) -> Vec<String> {
    if prefixes.is_empty() {
        return sessions;
    }
    sessions
        .into_iter()
        .filter(|s| prefixes.iter().any(|p| s.starts_with(p.as_str())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_backend_falls_back_to_tmux() {
        // 不 panic、能造出来即可(实际类型不暴露,验证不崩)。
        let _ = make_backend("nope");
        let _ = make_backend("tmux");
        let _ = make_backend("screen");
    }

    #[test]
    fn prefix_filter_keeps_matching() {
        let s = vec![
            "ccA".to_string(),
            "codex1".to_string(),
            "other".to_string(),
        ];
        let got = filter_by_prefix(s, &["cc".to_string(), "codex".to_string()]);
        assert_eq!(got, vec!["ccA".to_string(), "codex1".to_string()]);
    }

    #[test]
    fn empty_prefixes_pass_all() {
        let s = vec!["a".to_string(), "b".to_string()];
        assert_eq!(filter_by_prefix(s.clone(), &[]), s);
    }
}
