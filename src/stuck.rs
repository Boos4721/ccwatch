//! 「卡住」检测:working 但内容持续 N 秒无变化 → 疑似卡死(等不存在的输入 / 死循环 /
//! network hang)。纯逻辑 + 注入时钟,便于确定性单测。
//!
//! 抓屏轨道:signature = 规整后 pane 文本的哈希(剥掉数字,让 spinner/计时器
//! "(3s • esc to interrupt)" 这种每秒变的噪音不算"有变化")。
//! 协议轨道:signature 可取最近事件的状态+上下文哈希(配合常驻循环的时钟)。

use crate::classify::State;
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// 单会话的卡住追踪元数据(随 StateStore 持久化)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StuckMeta {
    /// 上次见到的内容签名。
    pub content_sig: u64,
    /// 当前签名(在 working 下)首次出现的 unix 秒——卡住计时起点。
    pub since_unix: u64,
    /// 本次停滞是否已播报过(去重/冷却,避免刷屏)。
    pub reported: bool,
}

/// 把内容规整后哈希成签名:剥掉 ASCII 数字、折叠空白。
/// 这样 agent 的计时器/spinner(只有数字在变)不会被误当成"有进展"。
pub fn content_signature(content: &str) -> u64 {
    let normalized: String = content
        .chars()
        .filter(|c| !c.is_ascii_digit())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut h = DefaultHasher::new();
    normalized.hash(&mut h);
    h.finish()
}

/// 评估一次观测,返回(更新后的 meta, 若应播报卡住则 Some(已卡秒数))。
///
/// 规则:
/// - 非 working:不可能"假装干活卡住",重置计时,不报。
/// - working 且签名未变:
///     - 已达阈值且未报过 → 报一次(置 reported)。
///     - 否则沿用计时,不报。
/// - working 且签名变了(或首次):重置计时起点,不报(= 有进展/恢复)。
pub fn evaluate(
    state: State,
    sig: u64,
    now_unix: u64,
    threshold_secs: u64,
    prev: Option<&StuckMeta>,
) -> (StuckMeta, Option<u64>) {
    if state != State::Working {
        return (
            StuckMeta {
                content_sig: sig,
                since_unix: now_unix,
                reported: false,
            },
            None,
        );
    }

    match prev {
        Some(p) if p.content_sig == sig => {
            // 内容没变:累积卡住时长。
            let elapsed = now_unix.saturating_sub(p.since_unix);
            if elapsed >= threshold_secs && !p.reported {
                (
                    StuckMeta {
                        content_sig: sig,
                        since_unix: p.since_unix,
                        reported: true,
                    },
                    Some(elapsed),
                )
            } else {
                (
                    StuckMeta {
                        content_sig: sig,
                        since_unix: p.since_unix,
                        reported: p.reported,
                    },
                    None,
                )
            }
        }
        // 内容变了或首次见到:重置计时(有进展 / 从卡住恢复)。
        _ => (
            StuckMeta {
                content_sig: sig,
                since_unix: now_unix,
                reported: false,
            },
            None,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TH: u64 = 600;

    /// 数字变化(计时器)不改变签名;真实文字变化才改。
    #[test]
    fn signature_ignores_digits_not_words() {
        let a = content_signature("• Working (3s • esc to interrupt)");
        let b = content_signature("• Working (47s • esc to interrupt)");
        assert_eq!(a, b, "只有秒数在变,应视为同一签名");
        let c = content_signature("• Reading file foo.rs");
        assert_ne!(a, c, "文字变了,签名应不同");
    }

    /// 持续无变化跨过阈值 → 报一次卡住。
    #[test]
    fn unchanged_past_threshold_reports_once() {
        let sig = 42;
        // t=0 首见 working。
        let (m0, e0) = evaluate(State::Working, sig, 0, TH, None);
        assert_eq!(e0, None);
        assert_eq!(m0.since_unix, 0);
        // t=300 还没到阈值。
        let (m1, e1) = evaluate(State::Working, sig, 300, TH, Some(&m0));
        assert_eq!(e1, None);
        // t=600 到阈值 → 报,带已卡 600s。
        let (m2, e2) = evaluate(State::Working, sig, 600, TH, Some(&m1));
        assert_eq!(e2, Some(600));
        assert!(m2.reported);
        // t=900 仍卡但已报过 → 不再报(去重)。
        let (_m3, e3) = evaluate(State::Working, sig, 900, TH, Some(&m2));
        assert_eq!(e3, None);
    }

    /// 恢复活动(签名变)→ 静默并重置计时。
    #[test]
    fn recovery_resets_and_is_silent() {
        let (m0, _) = evaluate(State::Working, 42, 0, TH, None);
        let (m1, e1) = evaluate(State::Working, 42, 700, TH, Some(&m0)); // 已报
        assert_eq!(e1, Some(700));
        assert!(m1.reported);
        // 内容变了 = 有进展。
        let (m2, e2) = evaluate(State::Working, 99, 720, TH, Some(&m1));
        assert_eq!(e2, None);
        assert_eq!(m2.since_unix, 720);
        assert!(!m2.reported);
    }

    /// 恢复后再次卡住 → 能再报(冷却已随恢复清零)。
    #[test]
    fn restuck_after_recovery_reports_again() {
        let (m0, _) = evaluate(State::Working, 42, 0, TH, None);
        let (m1, _) = evaluate(State::Working, 42, 600, TH, Some(&m0)); // 报过
        let (m2, _) = evaluate(State::Working, 7, 610, TH, Some(&m1)); // 恢复
        // 新停滞从 610 起算。
        let (m3, e3) = evaluate(State::Working, 7, 900, TH, Some(&m2));
        assert_eq!(e3, None, "才过 290s,没到阈值");
        let (_m4, e4) = evaluate(State::Working, 7, 1210, TH, Some(&m3));
        assert_eq!(e4, Some(600), "新停滞跨过阈值,应再报");
    }

    /// 切到非 working(idle/waiting)重置计时、不报。
    #[test]
    fn non_working_resets() {
        let (m0, _) = evaluate(State::Working, 42, 0, TH, None);
        let (m1, e1) = evaluate(State::Idle, 42, 700, TH, Some(&m0));
        assert_eq!(e1, None);
        assert!(!m1.reported);
        assert_eq!(m1.since_unix, 700);
    }
}
