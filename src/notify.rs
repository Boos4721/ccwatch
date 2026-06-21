//! 投递:stdout 格式化 + Telegram Bot API。

use crate::config::Delivery;
use crate::state::Event;
use anyhow::{Context, Result};

/// 把一条事件格式化成播报文本。
pub fn format_event(ev: &Event) -> String {
    match ev {
        Event::Done { session, context } => {
            with_ctx(format!("✅ {} 干完了,空闲待命", session), context)
        }
        Event::Waiting {
            session,
            context,
            kind,
        } => with_ctx(format!("⏸ {} 卡住了,{}", session, wait_phrase(kind)), context),
        Event::NewWaiting {
            session,
            context,
            kind,
        } => with_ctx(
            format!("⏸ {} 一上来就{}", session, wait_phrase(kind)),
            context,
        ),
        Event::Working { session } => format!("▶ {} 开始干了", session),
        Event::Gone { session } => format!("⚫ {} 会话已结束", session),
        Event::Stuck { session, secs } => {
            format!("⚠ {} 疑似卡住了(已 {} 无变化)", session, fmt_duration(*secs))
        }
    }
}

/// waiting 子类型 → 播报短语。未知子类型回退到通用「在等你拍板」。
fn wait_phrase(kind: &Option<crate::classify::WaitKind>) -> &'static str {
    use crate::classify::WaitKind;
    match kind {
        Some(WaitKind::Approval) => "在等你审批(y/n)",
        Some(WaitKind::Input) => "在等你输入",
        Some(WaitKind::Menu) => "在等你选择",
        None => "在等你拍板",
    }
}

/// 把秒数格式化成人类可读时长(如 12m / 1h5m)。
fn fmt_duration(secs: u64) -> String {
    let m = secs / 60;
    let h = m / 60;
    if h > 0 {
        format!("{}h{}m", h, m % 60)
    } else if m > 0 {
        format!("{}m", m)
    } else {
        format!("{}s", secs)
    }
}

/// 把多条事件拼成一段播报文本(每行一条)。
pub fn format_events(events: &[Event]) -> String {
    events
        .iter()
        .map(format_event)
        .collect::<Vec<_>>()
        .join("\n")
}

fn with_ctx(head: String, context: &Option<String>) -> String {
    match context {
        Some(c) if !c.is_empty() => format!("{}\n   {}", head, c),
        _ => head,
    }
}

/// 投递器。
pub enum Notifier {
    /// 只打印 stdout。
    Stdout,
    /// 直连 Telegram Bot API。
    Telegram {
        client: reqwest::Client,
        bot_token: String,
        chat_id: String,
    },
}

impl Notifier {
    /// 按配置构造投递器。
    pub fn from_delivery(d: &Delivery) -> Result<Notifier> {
        match d.mode.as_str() {
            "telegram" => {
                let bot_token = d
                    .bot_token
                    .clone()
                    .context("delivery.mode=telegram 但缺 bot_token")?;
                let chat_id = d
                    .chat_id
                    .clone()
                    .context("delivery.mode=telegram 但缺 chat_id")?;
                let mut builder = reqwest::Client::builder();
                if let Some(proxy) = &d.proxy {
                    builder = builder
                        .proxy(reqwest::Proxy::all(proxy).context("代理地址无效")?);
                }
                let client = builder.build().context("构造 reqwest 客户端失败")?;
                Ok(Notifier::Telegram {
                    client,
                    bot_token,
                    chat_id,
                })
            }
            // none 或未知都退化成 stdout。
            _ => Ok(Notifier::Stdout),
        }
    }

    /// 投递一组事件。空事件不投递。
    pub async fn deliver(&self, events: &[Event]) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        let text = format_events(events);
        match self {
            Notifier::Stdout => {
                println!("{}", text);
                Ok(())
            }
            Notifier::Telegram {
                client,
                bot_token,
                chat_id,
            } => {
                let url = format!("https://api.telegram.org/bot{}/sendMessage", bot_token);
                let resp = client
                    .post(&url)
                    .json(&serde_json::json!({
                        "chat_id": chat_id,
                        "text": text,
                        "disable_web_page_preview": true,
                    }))
                    .send()
                    .await
                    .context("Telegram sendMessage 请求失败")?;
                if !resp.status().is_success() {
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    anyhow::bail!("Telegram API 返回 {}: {}", status, body);
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Event;

    #[test]
    fn stuck_event_formats_with_duration() {
        let ev = Event::Stuck {
            session: "ccA".to_string(),
            secs: 720,
        };
        let s = format_event(&ev);
        assert!(s.contains("ccA"));
        assert!(s.contains("疑似卡住"));
        assert!(s.contains("12m"), "720s 应渲染成 12m,实际: {}", s);
    }

    #[test]
    fn duration_formatting() {
        assert_eq!(fmt_duration(45), "45s");
        assert_eq!(fmt_duration(600), "10m");
        assert_eq!(fmt_duration(3900), "1h5m");
    }
}
