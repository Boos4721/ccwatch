//! 状态文件读写(JSON: session -> last_state)+ 转移检测。

use crate::classify::{State, WaitKind};
use crate::config::Transitions;
use crate::stuck::StuckMeta;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// 当前 unix 秒(墙钟)。
fn now_unix() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 持久化的状态文件结构:会话名 -> 上次状态字符串。
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct StateStore {
    /// 会话 -> 上次状态("working"/"waiting"/"idle"/"unknown")。
    #[serde(default)]
    pub sessions: BTreeMap<String, String>,
    /// 会话 -> 卡住检测元数据(新增,旧状态文件缺这段时默认空,向后兼容)。
    #[serde(default)]
    pub stuck: BTreeMap<String, StuckMeta>,
    /// 会话 -> 上次状态转移的 unix 秒(status 视图显示"多久前转的";向后兼容默认空)。
    #[serde(default)]
    pub changed_at: BTreeMap<String, u64>,
    /// 会话 -> 上次扫描时刻(report 时长累计用;向后兼容默认空)。
    #[serde(default)]
    pub seen_at: BTreeMap<String, u64>,
    /// 会话 -> 今日各状态累计秒数(state 名 -> 秒;report 用)。
    #[serde(default)]
    pub durations: BTreeMap<String, BTreeMap<String, u64>>,
    /// 会话 -> 今日进入 waiting 的次数(report 用)。
    #[serde(default)]
    pub waited_count: BTreeMap<String, u64>,
    /// 会话 -> durations/waited_count 归属的天编号(跨日自动清零)。
    #[serde(default)]
    pub day: BTreeMap<String, u64>,
}

impl StateStore {
    /// 从文件加载;文件不存在视为空(首次运行)。
    pub fn load(path: &Path) -> Result<StateStore> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let store: StateStore = serde_json::from_str(&text)
                    .with_context(|| format!("解析状态文件失败: {}", path.display()))?;
                Ok(store)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(StateStore::default()),
            Err(e) => Err(e).with_context(|| format!("读取状态文件失败: {}", path.display())),
        }
    }

    /// 原子写回(先写临时文件再 rename)。
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建状态目录失败: {}", parent.display()))?;
        }
        let text = serde_json::to_string_pretty(self).context("序列化状态失败")?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text).with_context(|| format!("写临时状态文件失败: {}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("替换状态文件失败: {}", path.display()))?;
        Ok(())
    }

    pub fn get(&self, session: &str) -> Option<State> {
        self.sessions.get(session).map(|s| State::from_str(s))
    }
}

/// 一条转移事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// 干完了:* → idle。
    Done {
        session: String,
        context: Option<String>,
    },
    /// 卡住了:* → waiting。
    Waiting {
        session: String,
        context: Option<String>,
        kind: Option<WaitKind>,
    },
    /// 开始干:* → working(默认不报)。
    Working { session: String },
    /// 会话消失。
    Gone { session: String },
    /// 新会话首次见到且已是 waiting。
    NewWaiting {
        session: String,
        context: Option<String>,
        kind: Option<WaitKind>,
    },
    /// 疑似卡住:working 但内容持续无变化超过阈值。带已卡秒数。
    Stuck { session: String, secs: u64 },
}

impl Event {
    pub fn session(&self) -> &str {
        match self {
            Event::Done { session, .. }
            | Event::Waiting { session, .. }
            | Event::Working { session }
            | Event::Gone { session }
            | Event::NewWaiting { session, .. }
            | Event::Stuck { session, .. } => session,
        }
    }

    /// 给 hook 注入用的状态名。
    pub fn state_name(&self) -> &'static str {
        match self {
            Event::Done { .. } => "idle",
            Event::Waiting { .. } | Event::NewWaiting { .. } => "waiting",
            Event::Working { .. } => "working",
            Event::Gone { .. } => "gone",
            Event::Stuck { .. } => "stuck",
        }
    }

    /// 给 hook 注入用的 context(无 context 的事件返回 None)。
    pub fn context(&self) -> Option<&str> {
        match self {
            Event::Done { context, .. }
            | Event::Waiting { context, .. }
            | Event::NewWaiting { context, .. } => context.as_deref(),
            Event::Working { .. } | Event::Gone { .. } | Event::Stuck { .. } => None,
        }
    }
}

/// 判定单个会话从 prev 到 cur 的转移,按开关产出事件(可能为 None)。
/// `context` 是当前分类抽取的上下文,`wait_kind` 是 waiting 子类型(非 waiting 传 None)。
pub fn detect_transition(
    session: &str,
    prev: Option<State>,
    cur: State,
    context: &Option<String>,
    wait_kind: Option<WaitKind>,
    tr: &Transitions,
) -> Option<Event> {
    match prev {
        // 新会话首次见到。
        None => {
            if cur == State::Waiting && tr.notify_new_waiting {
                return Some(Event::NewWaiting {
                    session: session.to_string(),
                    context: context.clone(),
                    kind: wait_kind,
                });
            }
            // 首次见到的其他状态只建基线,不播报。
            None
        }
        Some(p) => {
            if p == cur {
                return None; // 无转移。
            }
            match cur {
                State::Idle => {
                    // working/waiting/unknown → idle
                    if tr.notify_done {
                        Some(Event::Done {
                            session: session.to_string(),
                            context: context.clone(),
                        })
                    } else {
                        None
                    }
                }
                State::Waiting => {
                    if tr.notify_waiting {
                        Some(Event::Waiting {
                            session: session.to_string(),
                            context: context.clone(),
                            kind: wait_kind,
                        })
                    } else {
                        None
                    }
                }
                State::Working => {
                    if tr.notify_working {
                        Some(Event::Working {
                            session: session.to_string(),
                        })
                    } else {
                        None
                    }
                }
                // → unknown 不播报(避免噪音)。
                State::Unknown => None,
            }
        }
    }
}

/// Unix 秒 → 天编号(UTC 自然日),用于今日统计跨日重置。
pub fn day_of(ts: u64) -> u64 {
    ts / 86_400
}

/// report 时长累计:把"上次扫描到现在"的时长记到上一个状态头上,跨日清零。
///
/// 返回 (新的今日各状态时长 map, 新的 waited 次数, 新的天编号)。纯函数便于单测。
/// - `prev_state`:上次记录的状态(None = 首见,不累计,只建基线)。
/// - `cur`:当前状态(用于 waiting 计数)。
pub fn accumulate(
    prev_durations: Option<&BTreeMap<String, u64>>,
    prev_waited: u64,
    prev_day: Option<u64>,
    prev_seen: Option<u64>,
    prev_state: Option<State>,
    cur: State,
    now: u64,
) -> (BTreeMap<String, u64>, u64, u64) {
    let today = day_of(now);
    // 跨日(或首见)清零今日统计。
    let (mut durations, mut waited) = match prev_day {
        Some(d) if d == today => (
            prev_durations.cloned().unwrap_or_default(),
            prev_waited,
        ),
        _ => (BTreeMap::new(), 0),
    };

    // 把上次扫描到现在的时长记到"上一个状态"头上。
    if let (Some(seen), Some(ps)) = (prev_seen, prev_state) {
        let delta = now.saturating_sub(seen).min(86_400);
        if delta > 0 {
            *durations.entry(ps.as_str().to_string()).or_insert(0) += delta;
        }
    }

    // 进入 waiting(状态真变成 waiting)计一次。
    if cur == State::Waiting && prev_state != Some(State::Waiting) {
        waited += 1;
    }

    (durations, waited, today)
}

/// 把一条会话的今日统计格式化成 report 行尾,
/// 如 `waited 3x / 18m · worked 42m · idle 1h`。
pub fn format_report_line(durations: &BTreeMap<String, u64>, waited: u64) -> String {
    let g = |k: &str| durations.get(k).copied().unwrap_or(0);
    format!(
        "waited {}x / {} · worked {} · idle {}",
        waited,
        crate::util::fmt_dur(g("waiting")),
        crate::util::fmt_dur(g("working")),
        crate::util::fmt_dur(g("idle"))
    )
}

#[cfg(test)]
mod report_tests {
    use super::*;

    #[test]
    fn accumulates_time_into_prev_state() {
        let (d, _w, _day) = accumulate(None, 0, Some(day_of(1000)), Some(1000), Some(State::Working), State::Idle, 1100);
        assert_eq!(d.get("working").copied(), Some(100));
    }

    #[test]
    fn waiting_increments_count_on_entry_only() {
        // idle → waiting:+1
        let (_d, w, _) = accumulate(None, 2, Some(day_of(1000)), Some(1000), Some(State::Idle), State::Waiting, 1010);
        assert_eq!(w, 3);
        // waiting → waiting:不再+1
        let (_d2, w2, _) = accumulate(None, 3, Some(day_of(1000)), Some(1000), Some(State::Waiting), State::Waiting, 1010);
        assert_eq!(w2, 3);
    }

    #[test]
    fn cross_day_resets() {
        let mut prev = BTreeMap::new();
        prev.insert("working".to_string(), 999u64);
        // prev_day = 0,now 落在 day 1 → 清零,再记新增量。
        let (d, w, day) = accumulate(Some(&prev), 5, Some(0), Some(86_000), Some(State::Idle), State::Idle, 90_000);
        assert_eq!(day, 1);
        assert_eq!(w, 0);
        // 旧的 working=999 被清掉,只剩本次 idle 增量。
        assert_eq!(d.get("working").copied(), None);
    }

    #[test]
    fn format_report_line_renders() {
        let mut d = BTreeMap::new();
        d.insert("waiting".to_string(), 1080);
        d.insert("working".to_string(), 2520);
        d.insert("idle".to_string(), 3600);
        assert_eq!(format_report_line(&d, 3), "waited 3x / 18m · worked 42m · idle 1h");
    }
}

/// 流式转移追踪器:协议轨道用。
///
/// 抓屏轨道是"扫描一遍 → diff"(批式,`scan_once`);协议轨道是事件流(推式)。
/// 两者**共用同一套转移规则**——本追踪器内部只调 [`detect_transition`],不另写一份
/// 判定逻辑。每来一个会话的当前 `State` 观测,就吐出 0/1 条 [`Event`],交给和抓屏
/// 完全相同的 [`crate::notify::Notifier`] 投递路径。
///
/// 持有可变的 per-session 上次状态,可由磁盘 [`StateStore`] 初始化(跨进程续上转移)。
pub struct TransitionTracker {
    transitions: Transitions,
    last: BTreeMap<String, State>,
    /// 卡住元数据(从磁盘带入、原样带出;协议轨道卡住检测后续接入由此承载)。
    stuck: BTreeMap<String, StuckMeta>,
    /// 上次转移时刻(unix 秒);status 视图用。
    changed_at: BTreeMap<String, u64>,
    /// report 统计原样带入带出(协议轨道不累计时长,只保持不丢)。
    seen_at: BTreeMap<String, u64>,
    durations: BTreeMap<String, BTreeMap<String, u64>>,
    waited_count: BTreeMap<String, u64>,
    day: BTreeMap<String, u64>,
}

impl TransitionTracker {
    /// 空白追踪器(首个观测都只建基线)。
    pub fn new(transitions: Transitions) -> TransitionTracker {
        TransitionTracker {
            transitions,
            last: BTreeMap::new(),
            stuck: BTreeMap::new(),
            changed_at: BTreeMap::new(),
            seen_at: BTreeMap::new(),
            durations: BTreeMap::new(),
            waited_count: BTreeMap::new(),
            day: BTreeMap::new(),
        }
    }

    /// 从持久化状态库初始化(让协议轨道也能跨进程接上上次状态)。
    pub fn from_store(transitions: Transitions, store: &StateStore) -> TransitionTracker {
        let last = store
            .sessions
            .iter()
            .map(|(k, v)| (k.clone(), State::from_str(v)))
            .collect();
        TransitionTracker {
            transitions,
            last,
            stuck: store.stuck.clone(),
            changed_at: store.changed_at.clone(),
            seen_at: store.seen_at.clone(),
            durations: store.durations.clone(),
            waited_count: store.waited_count.clone(),
            day: store.day.clone(),
        }
    }

    /// 喂入某会话的当前观测状态,返回该走播报的转移事件(可能为 None)。
    /// 内部更新 per-session 上次状态。`wait_kind` 是 waiting 子类型(非 waiting 传 None)。
    pub fn observe(
        &mut self,
        session: &str,
        cur: State,
        context: Option<String>,
        wait_kind: Option<WaitKind>,
    ) -> Option<Event> {
        let prev = self.last.get(session).copied();
        let ev = detect_transition(session, prev, cur, &context, wait_kind, &self.transitions);
        // 状态真正变化(或首见)时记录转移时刻,供 status 视图显示。
        if prev != Some(cur) {
            self.changed_at.insert(session.to_string(), now_unix());
        }
        self.last.insert(session.to_string(), cur);
        ev
    }

    /// 导出当前状态为可持久化的 [`StateStore`](写回磁盘用)。
    /// 保留传入的卡住元数据(协议轨道的卡住检测后续接入时由此承载)。
    pub fn to_store(&self) -> StateStore {
        StateStore {
            sessions: self
                .last
                .iter()
                .map(|(k, v)| (k.clone(), v.as_str().to_string()))
                .collect(),
            stuck: self.stuck.clone(),
            changed_at: self.changed_at.clone(),
            seen_at: self.seen_at.clone(),
            durations: self.durations.clone(),
            waited_count: self.waited_count.clone(),
            day: self.day.clone(),
        }
    }
}

#[cfg(test)]
mod tracker_tests {
    use super::*;

    fn tr() -> Transitions {
        Transitions::default()
    }

    /// 首个观测只建基线,不播报(和抓屏首见同语义)。
    #[test]
    fn first_observation_is_baseline_only() {
        let mut t = TransitionTracker::new(tr());
        assert_eq!(t.observe("codex", State::Working, None, None), None);
    }

    /// working → idle 产出 Done(协议轨道:task_started 后 task_complete)。
    #[test]
    fn working_to_idle_emits_done() {
        let mut t = TransitionTracker::new(tr());
        t.observe("codex", State::Working, None, None); // 基线
        let ev = t.observe("codex", State::Idle, Some("Four".to_string()), None);
        assert_eq!(
            ev,
            Some(Event::Done {
                session: "codex".to_string(),
                context: Some("Four".to_string()),
            })
        );
    }

    /// working → waiting 产出 Waiting,且带上子类型(协议轨道:approval 请求)。
    #[test]
    fn working_to_waiting_emits_waiting() {
        let mut t = TransitionTracker::new(tr());
        t.observe("codex", State::Working, None, None);
        let ev = t.observe(
            "codex",
            State::Waiting,
            Some("rm -rf build".to_string()),
            Some(WaitKind::Approval),
        );
        assert_eq!(
            ev,
            Some(Event::Waiting {
                session: "codex".to_string(),
                context: Some("rm -rf build".to_string()),
                kind: Some(WaitKind::Approval),
            })
        );
    }

    /// 同状态重复观测不重复播报。
    #[test]
    fn repeated_state_no_event() {
        let mut t = TransitionTracker::new(tr());
        t.observe("codex", State::Working, None, None);
        assert_eq!(t.observe("codex", State::Working, None, None), None);
        assert_eq!(t.observe("codex", State::Working, None, None), None);
    }

    /// 从磁盘状态初始化:上次 working,这次直接 idle → 立刻 Done(跨进程接续)。
    #[test]
    fn from_store_carries_previous_state() {
        let mut store = StateStore::default();
        store
            .sessions
            .insert("codex".to_string(), "working".to_string());
        let mut t = TransitionTracker::from_store(tr(), &store);
        let ev = t.observe("codex", State::Idle, None, None);
        assert!(matches!(ev, Some(Event::Done { .. })));
    }

    /// to_store 往返:观测后能导出可持久化状态。
    #[test]
    fn to_store_roundtrip() {
        let mut t = TransitionTracker::new(tr());
        t.observe("codex", State::Waiting, None, None);
        let store = t.to_store();
        assert_eq!(store.sessions.get("codex").map(String::as_str), Some("waiting"));
    }

    /// 关键:协议轨道(tracker)与抓屏轨道(detect_transition)对**同样的**
    /// (prev, cur, context) 必须产出**完全一致**的 Event —— 证明两轨共用一套规则、
    /// 没有第二份判定逻辑。
    #[test]
    fn tracker_matches_screen_path_exactly() {
        let cases = [
            (State::Working, State::Idle, Some("done".to_string())),
            (State::Working, State::Waiting, Some("approve?".to_string())),
            (State::Idle, State::Working, None),
            (State::Waiting, State::Idle, None),
            (State::Idle, State::Idle, None),
        ];
        for (prev, cur, ctx) in cases {
            // 抓屏路径:直接调 detect_transition。
            let screen = detect_transition("s", Some(prev), cur, &ctx, None, &tr());
            // 协议路径:tracker 先用 prev 建基线,再 observe cur。
            let mut t = TransitionTracker::new(tr());
            t.observe("s", prev, None, None);
            let protocol = t.observe("s", cur, ctx.clone(), None);
            assert_eq!(
                screen, protocol,
                "两轨对 {:?}->{:?} 必须产出一致事件",
                prev, cur
            );
        }
    }

    /// notify_done=false 时,working→idle 不播报(开关对两轨一致生效)。
    #[test]
    fn respects_transition_switches() {
        let mut t = Transitions::default();
        t.notify_done = false;
        let mut tk = TransitionTracker::new(t);
        tk.observe("codex", State::Working, None, None);
        assert_eq!(tk.observe("codex", State::Idle, None, None), None);
    }
}
