//! `ccwatch status`:一屏列出所有被监控会话的当前状态 + 上次转移多久前。
//!
//! 渲染逻辑做成纯函数(给定行集合 + now + 是否着色 → 文本),便于单测;
//! 数据采集(读 state_file + 一次性 capture)在 main 里组装。

use crate::classify::{State, WaitKind};

/// 状态着色用的 ANSI 码(仅 tty 下用)。
mod color {
    pub const RESET: &str = "\x1b[0m";
    pub const GREEN: &str = "\x1b[32m"; // idle
    pub const CYAN: &str = "\x1b[36m"; // working
    pub const YELLOW: &str = "\x1b[33m"; // waiting
    pub const RED: &str = "\x1b[31m"; // stuck
    pub const DIM: &str = "\x1b[2m"; // unknown / 次要
}

/// 一行待渲染的会话状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusRow {
    pub session: String,
    pub state: State,
    /// waiting 子类型(仅 waiting 有意义)。
    pub wait_kind: Option<WaitKind>,
    /// 是否被判为卡住(working 但长时间无变化)。
    pub stuck: bool,
    /// 上次转移的 unix 秒(None = 未知)。
    pub changed_at: Option<u64>,
}

/// 状态显示标签:stuck 优先于 working;waiting 带子类型。
pub fn state_label(row: &StatusRow) -> String {
    if row.stuck {
        return "stuck".to_string();
    }
    match row.state {
        State::Working => "working".to_string(),
        State::Idle => "idle".to_string(),
        State::Unknown => "unknown".to_string(),
        State::Waiting => match row.wait_kind {
            Some(k) => format!("waiting({})", k.as_str()),
            None => "waiting".to_string(),
        },
    }
}

/// 给标签上色(仅着色状态主词)。
fn colorize(row: &StatusRow, label: &str) -> String {
    let c = if row.stuck {
        color::RED
    } else {
        match row.state {
            State::Working => color::CYAN,
            State::Idle => color::GREEN,
            State::Waiting => color::YELLOW,
            State::Unknown => color::DIM,
        }
    };
    format!("{}{}{}", c, label, color::RESET)
}

/// 把 unix 秒差渲染成"多久前"(如 `3m ago` / `1h5m ago` / `just now`)。
pub fn ago(changed_at: Option<u64>, now: u64) -> String {
    match changed_at {
        None => "-".to_string(),
        Some(t) => {
            let secs = now.saturating_sub(t);
            if secs < 5 {
                "just now".to_string()
            } else if secs < 60 {
                format!("{}s ago", secs)
            } else {
                let m = secs / 60;
                let h = m / 60;
                if h > 0 {
                    format!("{}h{}m ago", h, m % 60)
                } else {
                    format!("{}m ago", m)
                }
            }
        }
    }
}

/// 渲染整张表。`use_color` 由调用方按 tty 判定传入。
pub fn render(rows: &[StatusRow], now: u64, use_color: bool) -> String {
    if rows.is_empty() {
        return "(没有被监控的会话)".to_string();
    }
    let mut out = String::new();
    out.push_str(&format!("{:<18} {:<20} {}\n", "SESSION", "STATE", "SINCE"));
    for r in rows {
        let label = state_label(r);
        let shown = if use_color {
            // 着色后宽度对齐会被 ANSI 码干扰,这里先按原文 padding 再上色。
            let padded = format!("{:<20}", label);
            colorize(r, padded.trim_end()) + &" ".repeat(20usize.saturating_sub(label.chars().count()))
        } else {
            format!("{:<20}", label)
        };
        out.push_str(&format!(
            "{:<18} {} {}\n",
            r.session,
            shown,
            ago(r.changed_at, now)
        ));
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(session: &str, state: State, wait_kind: Option<WaitKind>, stuck: bool, ca: Option<u64>) -> StatusRow {
        StatusRow {
            session: session.to_string(),
            state,
            wait_kind,
            stuck,
            changed_at: ca,
        }
    }

    #[test]
    fn labels_cover_all_states() {
        assert_eq!(state_label(&row("a", State::Working, None, false, None)), "working");
        assert_eq!(state_label(&row("a", State::Idle, None, false, None)), "idle");
        assert_eq!(
            state_label(&row("a", State::Waiting, Some(WaitKind::Approval), false, None)),
            "waiting(approval)"
        );
        assert_eq!(
            state_label(&row("a", State::Waiting, Some(WaitKind::Menu), false, None)),
            "waiting(menu)"
        );
        assert_eq!(state_label(&row("a", State::Waiting, None, false, None)), "waiting");
    }

    #[test]
    fn stuck_overrides_working_label() {
        // working + stuck 旗标 → 显示 stuck。
        assert_eq!(state_label(&row("a", State::Working, None, true, None)), "stuck");
    }

    #[test]
    fn ago_formats() {
        assert_eq!(ago(None, 1000), "-");
        assert_eq!(ago(Some(998), 1000), "just now");
        assert_eq!(ago(Some(955), 1000), "45s ago");
        assert_eq!(ago(Some(700), 1000), "5m ago");
        assert_eq!(ago(Some(1100), 5000), "1h5m ago");
    }

    #[test]
    fn render_plain_lists_all_rows_no_ansi() {
        let rows = vec![
            row("ccA", State::Idle, None, false, Some(900)),
            row("ccD", State::Working, None, false, Some(990)),
            row("codex1", State::Waiting, Some(WaitKind::Approval), false, Some(880)),
            row("ccX", State::Working, None, true, Some(100)),
        ];
        let out = render(&rows, 1000, false);
        assert!(out.contains("ccA"));
        assert!(out.contains("idle"));
        assert!(out.contains("waiting(approval)"));
        assert!(out.contains("stuck"));
        assert!(out.contains("ago"));
        // 非 tty:不应含 ANSI 转义。
        assert!(!out.contains('\x1b'), "plain 输出不该有颜色码");
    }

    #[test]
    fn render_color_has_ansi() {
        let rows = vec![row("ccA", State::Idle, None, false, Some(900))];
        let out = render(&rows, 1000, true);
        assert!(out.contains('\x1b'), "tty 输出应含颜色码");
        assert!(out.contains("idle"));
    }

    #[test]
    fn render_empty() {
        assert!(render(&[], 0, false).contains("没有被监控"));
    }
}
