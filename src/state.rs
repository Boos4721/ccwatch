//! 状态文件读写(JSON: session -> last_state)+ 转移检测。

use crate::classify::State;
use crate::config::Transitions;
use crate::stuck::StuckMeta;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// 持久化的状态文件结构:会话名 -> 上次状态字符串。
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct StateStore {
    /// 会话 -> 上次状态("working"/"waiting"/"idle"/"unknown")。
    #[serde(default)]
    pub sessions: BTreeMap<String, String>,
    /// 会话 -> 卡住检测元数据(新增,旧状态文件缺这段时默认空,向后兼容)。
    #[serde(default)]
    pub stuck: BTreeMap<String, StuckMeta>,
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
    },
    /// 开始干:* → working(默认不报)。
    Working { session: String },
    /// 会话消失。
    Gone { session: String },
    /// 新会话首次见到且已是 waiting。
    NewWaiting {
        session: String,
        context: Option<String>,
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
}

/// 判定单个会话从 prev 到 cur 的转移,按开关产出事件(可能为 None)。
/// `context` 是当前分类抽取的上下文。
pub fn detect_transition(
    session: &str,
    prev: Option<State>,
    cur: State,
    context: &Option<String>,
    tr: &Transitions,
) -> Option<Event> {
    match prev {
        // 新会话首次见到。
        None => {
            if cur == State::Waiting && tr.notify_new_waiting {
                return Some(Event::NewWaiting {
                    session: session.to_string(),
                    context: context.clone(),
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
}

impl TransitionTracker {
    /// 空白追踪器(首个观测都只建基线)。
    pub fn new(transitions: Transitions) -> TransitionTracker {
        TransitionTracker {
            transitions,
            last: BTreeMap::new(),
            stuck: BTreeMap::new(),
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
        }
    }

    /// 喂入某会话的当前观测状态,返回该走播报的转移事件(可能为 None)。
    /// 内部更新 per-session 上次状态。
    pub fn observe(
        &mut self,
        session: &str,
        cur: State,
        context: Option<String>,
    ) -> Option<Event> {
        let prev = self.last.get(session).copied();
        let ev = detect_transition(session, prev, cur, &context, &self.transitions);
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
        assert_eq!(t.observe("codex", State::Working, None), None);
    }

    /// working → idle 产出 Done(协议轨道:task_started 后 task_complete)。
    #[test]
    fn working_to_idle_emits_done() {
        let mut t = TransitionTracker::new(tr());
        t.observe("codex", State::Working, None); // 基线
        let ev = t.observe("codex", State::Idle, Some("Four".to_string()));
        assert_eq!(
            ev,
            Some(Event::Done {
                session: "codex".to_string(),
                context: Some("Four".to_string()),
            })
        );
    }

    /// working → waiting 产出 Waiting(协议轨道:task_started 后 approval 请求)。
    #[test]
    fn working_to_waiting_emits_waiting() {
        let mut t = TransitionTracker::new(tr());
        t.observe("codex", State::Working, None);
        let ev = t.observe("codex", State::Waiting, Some("rm -rf build".to_string()));
        assert_eq!(
            ev,
            Some(Event::Waiting {
                session: "codex".to_string(),
                context: Some("rm -rf build".to_string()),
            })
        );
    }

    /// 同状态重复观测不重复播报。
    #[test]
    fn repeated_state_no_event() {
        let mut t = TransitionTracker::new(tr());
        t.observe("codex", State::Working, None);
        assert_eq!(t.observe("codex", State::Working, None), None);
        assert_eq!(t.observe("codex", State::Working, None), None);
    }

    /// 从磁盘状态初始化:上次 working,这次直接 idle → 立刻 Done(跨进程接续)。
    #[test]
    fn from_store_carries_previous_state() {
        let mut store = StateStore::default();
        store
            .sessions
            .insert("codex".to_string(), "working".to_string());
        let mut t = TransitionTracker::from_store(tr(), &store);
        let ev = t.observe("codex", State::Idle, None);
        assert!(matches!(ev, Some(Event::Done { .. })));
    }

    /// to_store 往返:观测后能导出可持久化状态。
    #[test]
    fn to_store_roundtrip() {
        let mut t = TransitionTracker::new(tr());
        t.observe("codex", State::Waiting, None);
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
            let screen = detect_transition("s", Some(prev), cur, &ctx, &tr());
            // 协议路径:tracker 先用 prev 建基线,再 observe cur。
            let mut t = TransitionTracker::new(tr());
            t.observe("s", prev, None);
            let protocol = t.observe("s", cur, ctx.clone());
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
        tk.observe("codex", State::Working, None);
        assert_eq!(tk.observe("codex", State::Idle, None), None);
    }
}
