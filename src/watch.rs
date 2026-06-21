//! 核心循环:扫描 → 分类 → diff → 产出事件。once 和 daemon 共用。

use crate::backend::{filter_by_prefix, Backend};
use crate::classify::{Classification, Classifier, State};
use crate::config::Config;
use crate::state::{detect_transition, Event, StateStore};
use crate::stuck;
use anyhow::Result;

/// 一个会话的当前快照(给 check 调试用)。
pub struct SessionSnapshot {
    pub session: String,
    pub profile: String,
    pub state: State,
    pub context: Option<String>,
    /// waiting 子类型(非 waiting 为 None)。
    pub wait_kind: Option<crate::classify::WaitKind>,
    /// 规整后内容签名(卡住检测用;剥数字抗 spinner/计时器噪音)。
    pub content_sig: u64,
    /// pane 原文(自动应答 / 录制用;check/status 可忽略)。
    pub pane: String,
}

/// 扫描当前所有匹配会话,返回快照列表(不读写状态文件)。
pub fn scan_snapshots(
    cfg: &Config,
    classifier: &Classifier,
    backend: &dyn Backend,
) -> Result<Vec<SessionSnapshot>> {
    let sessions = filter_by_prefix(backend.list_sessions()?, &cfg.general.session_prefixes);
    let mut snaps = Vec::new();
    for s in sessions {
        let pane = match backend.capture_pane(&s, cfg.general.capture_lines) {
            Ok(p) => p,
            Err(_) => continue, // 会话刚消失等,跳过。
        };
        if let Some(Classification {
            profile,
            state,
            context,
            wait_kind,
        }) = classifier.classify(&s, &pane)
        {
            snaps.push(SessionSnapshot {
                session: s,
                profile,
                state,
                context,
                wait_kind,
                content_sig: stuck::content_signature(&pane),
                pane,
            });
        }
    }
    Ok(snaps)
}

/// 当前 unix 秒(墙钟)。
fn now_unix() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 扫描一遍:对比上次状态,产出转移事件,并返回更新后的状态库。
/// 调用方负责把 `new_store` 写回磁盘(投递成功后)。
///
/// `answerer` 非空且有启用规则时,对当前 pane 做自动应答(send-keys)。
pub fn scan_once(
    cfg: &Config,
    classifier: &Classifier,
    prev: &StateStore,
    answerer: Option<&crate::auto_answer::AutoAnswerer>,
    backend: &dyn Backend,
) -> Result<(Vec<Event>, StateStore)> {
    // 当前活着的(已过前缀过滤的)会话全集,用于判定 gone。
    let live: Vec<String> =
        filter_by_prefix(backend.list_sessions()?, &cfg.general.session_prefixes);

    let snaps = scan_snapshots(cfg, classifier, backend)?;
    let now = now_unix();
    let threshold = cfg.general.stuck_threshold_secs;

    // 自动应答:在状态推进前,对命中规则的会话发按键(默认无规则则跳过)。
    if let Some(a) = answerer {
        if !a.is_empty() {
            for snap in &snaps {
                a.apply(backend, &snap.session, &snap.profile, &snap.pane);
            }
        }
    }

    let mut events = Vec::new();
    let mut new_store = StateStore::default();

    // 1) 当前能分类的会话:检测转移 + 卡住 + 记录新状态。
    for snap in &snaps {
        let prev_state = prev.get(&snap.session);
        if let Some(ev) = detect_transition(
            &snap.session,
            prev_state,
            snap.state,
            &snap.context,
            snap.wait_kind,
            &cfg.transitions,
        ) {
            events.push(ev);
        }

        // 卡住检测:复用 stuck 模块的纯逻辑(注入墙钟 + 内容签名)。
        let (meta, stuck_secs) = stuck::evaluate(
            snap.state,
            snap.content_sig,
            now,
            threshold,
            prev.stuck.get(&snap.session),
        );
        if let Some(secs) = stuck_secs {
            if cfg.transitions.notify_stuck {
                events.push(Event::Stuck {
                    session: snap.session.clone(),
                    secs,
                });
            }
        }
        new_store
            .sessions
            .insert(snap.session.clone(), snap.state.as_str().to_string());
        new_store.stuck.insert(snap.session.clone(), meta);

        // 转移时刻:状态真变(或首见)记 now,否则沿用旧值,供 status 视图显示。
        let changed = if prev_state != Some(snap.state) {
            now
        } else {
            prev.changed_at.get(&snap.session).copied().unwrap_or(now)
        };
        new_store.changed_at.insert(snap.session.clone(), changed);

        // report 时长累计:把上次扫描到现在的时长记到上一个状态头上(跨日清零)。
        let (durations, waited, day) = crate::state::accumulate(
            prev.durations.get(&snap.session),
            prev.waited_count.get(&snap.session).copied().unwrap_or(0),
            prev.day.get(&snap.session).copied(),
            prev.seen_at.get(&snap.session).copied(),
            prev_state,
            snap.state,
            now,
        );
        new_store.durations.insert(snap.session.clone(), durations);
        new_store.waited_count.insert(snap.session.clone(), waited);
        new_store.day.insert(snap.session.clone(), day);
        new_store.seen_at.insert(snap.session.clone(), now);
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
            // 还活着,只是这一帧没认出来:沿用旧状态 + 卡住元数据 + 转移时刻。
            new_store.sessions.insert(sess.clone(), last.clone());
            if let Some(m) = prev.stuck.get(sess) {
                new_store.stuck.insert(sess.clone(), m.clone());
            }
            if let Some(t) = prev.changed_at.get(sess) {
                new_store.changed_at.insert(sess.clone(), *t);
            }
            // report 统计原样带出(不累计这一帧,因为状态未知)。
            if let Some(d) = prev.durations.get(sess) {
                new_store.durations.insert(sess.clone(), d.clone());
            }
            if let Some(w) = prev.waited_count.get(sess) {
                new_store.waited_count.insert(sess.clone(), *w);
            }
            if let Some(d) = prev.day.get(sess) {
                new_store.day.insert(sess.clone(), *d);
            }
            if let Some(t) = prev.seen_at.get(sess) {
                new_store.seen_at.insert(sess.clone(), *t);
            }
        } else if cfg.transitions.notify_gone {
            events.push(Event::Gone {
                session: sess.clone(),
            });
        }
        // notify_gone=false 时直接丢弃,不再跟踪。
    }

    Ok((events, new_store))
}
