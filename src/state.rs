//! 状态文件读写(JSON: session -> last_state)+ 转移检测。

use crate::classify::State;
use crate::config::Transitions;
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
}

impl Event {
    pub fn session(&self) -> &str {
        match self {
            Event::Done { session, .. }
            | Event::Waiting { session, .. }
            | Event::Working { session }
            | Event::Gone { session }
            | Event::NewWaiting { session, .. } => session,
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
