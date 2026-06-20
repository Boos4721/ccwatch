//! 核心循环:扫描 → 分类 → diff → 产出事件。once 和 daemon 共用。

use crate::classify::{Classification, Classifier, State};
use crate::config::Config;
use crate::state::{detect_transition, Event, StateStore};
use crate::tmux;
use anyhow::Result;

/// 一个会话的当前快照(给 check 调试用)。
pub struct SessionSnapshot {
    pub session: String,
    pub profile: String,
    pub state: State,
    pub context: Option<String>,
}

/// 扫描当前所有匹配会话,返回快照列表(不读写状态文件)。
pub fn scan_snapshots(cfg: &Config, classifier: &Classifier) -> Result<Vec<SessionSnapshot>> {
    let sessions = tmux::filter_by_prefix(tmux::list_sessions()?, &cfg.general.session_prefixes);
    let mut snaps = Vec::new();
    for s in sessions {
        let pane = match tmux::capture_pane(&s, cfg.general.capture_lines) {
            Ok(p) => p,
            Err(_) => continue, // 会话刚消失等,跳过。
        };
        if let Some(Classification {
            profile,
            state,
            context,
        }) = classifier.classify(&s, &pane)
        {
            snaps.push(SessionSnapshot {
                session: s,
                profile,
                state,
                context,
            });
        }
    }
    Ok(snaps)
}

/// 扫描一遍:对比上次状态,产出转移事件,并返回更新后的状态库。
/// 调用方负责把 `new_store` 写回磁盘(投递成功后)。
pub fn scan_once(
    cfg: &Config,
    classifier: &Classifier,
    prev: &StateStore,
) -> Result<(Vec<Event>, StateStore)> {
    // 当前活着的(已过前缀过滤的)会话全集,用于判定 gone。
    let live: Vec<String> =
        tmux::filter_by_prefix(tmux::list_sessions()?, &cfg.general.session_prefixes);

    let snaps = scan_snapshots(cfg, classifier)?;

    let mut events = Vec::new();
    let mut new_store = StateStore::default();

    // 1) 当前能分类的会话:检测转移 + 记录新状态。
    for snap in &snaps {
        let prev_state = prev.get(&snap.session);
        if let Some(ev) = detect_transition(
            &snap.session,
            prev_state,
            snap.state,
            &snap.context,
            &cfg.transitions,
        ) {
            events.push(ev);
        }
        new_store
            .sessions
            .insert(snap.session.clone(), snap.state.as_str().to_string());
    }

    // 2) 上次记过、这次没分类出来的会话:
    //    - 仍活在 tmux 里 → 暂时认不出,沿用旧状态(不播报、不丢)。
    //    - 已从 tmux 消失 → gone 事件。
    let classified: std::collections::HashSet<&str> =
        snaps.iter().map(|s| s.session.as_str()).collect();
    for (sess, last) in &prev.sessions {
        if classified.contains(sess.as_str()) {
            continue;
        }
        if live.iter().any(|l| l == sess) {
            // 还活着,只是这一帧没认出来:沿用旧状态。
            new_store.sessions.insert(sess.clone(), last.clone());
        } else if cfg.transitions.notify_gone {
            events.push(Event::Gone {
                session: sess.clone(),
            });
        }
        // notify_gone=false 时直接丢弃,不再跟踪。
    }

    Ok((events, new_store))
}
