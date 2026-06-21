//! 自动应答:命中安全弹窗等待时,按规则用 send-keys 自动回应。
//!
//! 规则来自 config `[[auto_answer]]`,默认全部禁用,需显式 `enabled = true`。
//! 只在 daemon/once 扫描时对当前 pane 匹配;匹配成功则发按键并记 info 日志。

use crate::backend::Backend;
use crate::config::AutoAnswer;
use regex::Regex;

/// 编译好的单条自动应答规则。
struct CompiledRule {
    profile: Option<String>,
    re: Regex,
    send: String,
}

/// 自动应答器:持有已编译、且 enabled 的规则。
pub struct AutoAnswerer {
    rules: Vec<CompiledRule>,
}

/// 一次自动应答动作(给测试/日志用)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnswerAction {
    pub session: String,
    pub send: String,
}

impl AutoAnswerer {
    /// 从配置编译(跳过 disabled 规则)。
    pub fn from_rules(rules: &[AutoAnswer]) -> anyhow::Result<AutoAnswerer> {
        let mut compiled = Vec::new();
        for r in rules {
            if !r.enabled {
                continue;
            }
            let re = Regex::new(&r.r#match).map_err(|e| {
                anyhow::anyhow!("auto_answer 正则 `{}` 编译失败: {}", r.r#match, e)
            })?;
            compiled.push(CompiledRule {
                profile: r.profile.clone(),
                re,
                send: r.send.clone(),
            });
        }
        Ok(AutoAnswerer { rules: compiled })
    }

    /// 有没有启用的规则(没有就整段跳过,省得抓 pane)。
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// 对一个会话的 pane 求要发的按键(第一条命中的规则)。
    /// `profile` 为该会话识别到的 profile 名。
    pub fn decide(&self, profile: &str, pane: &str) -> Option<String> {
        for r in &self.rules {
            if let Some(p) = &r.profile {
                if p != profile {
                    continue;
                }
            }
            if r.re.is_match(pane) {
                return Some(r.send.clone());
            }
        }
        None
    }

    /// 真正发送按键(daemon/once 用)。返回执行的动作列表。
    pub fn apply(
        &self,
        backend: &dyn Backend,
        session: &str,
        profile: &str,
        pane: &str,
    ) -> Option<AnswerAction> {
        let keys = self.decide(profile, pane)?;
        match backend.send_keys(session, &keys) {
            Ok(()) => {
                tracing::info!("auto-answer: 向 {} 发送按键 `{}`", session, keys);
                Some(AnswerAction {
                    session: session.to_string(),
                    send: keys,
                })
            }
            Err(e) => {
                tracing::warn!("auto-answer 发送失败 ({}): {}", session, e);
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(profile: Option<&str>, m: &str, send: &str, enabled: bool) -> AutoAnswer {
        AutoAnswer {
            profile: profile.map(|s| s.to_string()),
            r#match: m.to_string(),
            send: send.to_string(),
            enabled,
        }
    }

    #[test]
    fn disabled_rules_are_skipped() {
        let a = AutoAnswerer::from_rules(&[rule(None, "Trust", "Enter", false)]).unwrap();
        assert!(a.is_empty());
        assert_eq!(a.decide("codex", "Trust this folder?"), None);
    }

    #[test]
    fn enabled_rule_matches_and_returns_keys() {
        let a = AutoAnswerer::from_rules(&[rule(None, "Trust all", "Enter", true)]).unwrap();
        assert!(!a.is_empty());
        assert_eq!(
            a.decide("codex", "2. Trust all and continue"),
            Some("Enter".to_string())
        );
    }

    #[test]
    fn profile_scoped_rule_only_matches_its_profile() {
        let a =
            AutoAnswerer::from_rules(&[rule(Some("codex"), "Trust", "Enter", true)]).unwrap();
        assert_eq!(a.decide("claude", "Trust?"), None);
        assert_eq!(a.decide("codex", "Trust?"), Some("Enter".to_string()));
    }

    #[test]
    fn first_matching_rule_wins() {
        let a = AutoAnswerer::from_rules(&[
            rule(None, "yes/no", "y", true),
            rule(None, ".*", "Enter", true),
        ])
        .unwrap();
        assert_eq!(a.decide("x", "answer yes/no please"), Some("y".to_string()));
    }
}
