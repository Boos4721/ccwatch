//! 转移触发 hook:对应事件发生时 spawn 一条 shell 命令。
//!
//! 命令通过 `sh -c` 执行,注入环境变量 `CCWATCH_SESSION` / `CCWATCH_STATE` /
//! `CCWATCH_CONTEXT`。命令失败只 warn,不中断主循环。

use crate::config::Transitions;
use crate::state::Event;
use std::process::Command;

/// 针对一个事件,挑出对应的 hook 命令并执行(若已配置)。
pub fn run_for_event(tr: &Transitions, ev: &Event) {
    let cmd = match ev {
        Event::Done { .. } => &tr.on_done_cmd,
        Event::Waiting { .. } | Event::NewWaiting { .. } => &tr.on_waiting_cmd,
        Event::Working { .. } => &tr.on_working_cmd,
        Event::Stuck { .. } => &tr.on_stuck_cmd,
        // gone 暂无 hook。
        Event::Gone { .. } => &None,
    };
    if let Some(cmd) = cmd {
        spawn(cmd, ev);
    }
}

/// 给一组事件依次跑 hook。
pub fn run_for_events(tr: &Transitions, events: &[Event]) {
    for ev in events {
        run_for_event(tr, ev);
    }
}

fn spawn(cmd: &str, ev: &Event) {
    let session = ev.session();
    let state = ev.state_name();
    let context = ev.context().unwrap_or("");

    let result = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .env("CCWATCH_SESSION", session)
        .env("CCWATCH_STATE", state)
        .env("CCWATCH_CONTEXT", context)
        .spawn();

    match result {
        Ok(mut child) => {
            // 不阻塞主循环:等子进程结束只为回收(快命令);失败不致命。
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            tracing::info!("hook spawned for {} ({})", session, state);
        }
        Err(e) => tracing::warn!("hook 启动失败 ({} {}): {}", session, state, e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_correct_cmd_per_event() {
        let mut tr = Transitions::default();
        tr.on_done_cmd = Some("echo done".to_string());
        // 没配 waiting hook → 不应 panic、不应误触发(这里只验证选择逻辑不崩)。
        run_for_event(
            &tr,
            &Event::Waiting {
                session: "ccA".into(),
                context: None,
                kind: None,
            },
        );
        // 配了 done hook 的事件能跑(echo 必然成功)。
        run_for_event(
            &tr,
            &Event::Done {
                session: "ccA".into(),
                context: Some("ctx".into()),
            },
        );
    }
}
