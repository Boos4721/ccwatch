//! 双轨集成测试:协议轨道完整链路(真实报文 → 解析 → 共享转移 → 播报)。
//!
//! 喂入 2026-06-21 本机实测抓取的 Codex `codex/event` 序列,经
//! `acp::classify_codex_frame` → `state::TransitionTracker`(复用 detect_transition)
//! → `notify::format_event`,验证产出的播报和抓屏轨道同源同形。

use ccwatch::acp::classify_codex_frame;
use ccwatch::config::Transitions;
use ccwatch::notify::format_event;
use ccwatch::state::{Event, TransitionTracker};
use serde_json::json;

/// 把一串 Codex 帧喂给 tracker,收集产出的播报事件。
fn run_frames(label: &str, frames: &[serde_json::Value]) -> Vec<Event> {
    let mut tracker = TransitionTracker::new(Transitions::default());
    let mut events = Vec::new();
    for f in frames {
        if let Some(sig) = classify_codex_frame(f) {
            if let Some(ev) = tracker.observe(label, sig.state, sig.context) {
                events.push(ev);
            }
        }
    }
    events
}

/// 真实 turn:session_configured → task_started → 中间噪音 → task_complete。
/// 协议轨道应只播报一条 Done(working→idle),中间事件不产噪。
#[test]
fn full_codex_turn_emits_single_done() {
    let frames = [
        json!({"method":"codex/event","params":{"msg":{"type":"session_configured","model":"gpt-5.5"}}}),
        json!({"method":"codex/event","params":{"msg":{"type":"task_started","turn_id":"3"}}}),
        json!({"method":"codex/event","params":{"msg":{"type":"item_started","item":{"type":"Reasoning"}}}}),
        json!({"method":"codex/event","params":{"msg":{"type":"agent_message_content_delta","delta":"4"}}}),
        json!({"method":"codex/event","params":{"msg":{"type":"token_count"}}}),
        json!({"method":"codex/event","params":{"msg":{"type":"task_complete","turn_id":"3","last_agent_message":"4"}}}),
    ];
    let events = run_frames("codex", &frames);
    assert_eq!(events.len(), 1, "整个 turn 只该播报一条转移,实际 {:?}", events);
    assert_eq!(
        events[0],
        Event::Done {
            session: "codex".to_string(),
            context: Some("4".to_string()),
        }
    );
    // 播报文本走和抓屏完全相同的 format_event。
    let text = format_event(&events[0]);
    assert!(text.contains("干完了"), "播报文本: {}", text);
    assert!(text.contains('4'));
}

/// 含 approval 的 turn:task_started → exec_approval_request → task_complete。
/// 应播报 Waiting(working→waiting)再 Done(waiting→idle)两条。
#[test]
fn turn_with_approval_emits_waiting_then_done() {
    let frames = [
        json!({"method":"codex/event","params":{"msg":{"type":"task_started","turn_id":"1"}}}),
        json!({"method":"codex/event","params":{"msg":{"type":"exec_approval_request","command":["rm","-rf","build"]}}}),
        json!({"method":"codex/event","params":{"msg":{"type":"task_complete","turn_id":"1","last_agent_message":"done"}}}),
    ];
    let events = run_frames("codex", &frames);
    assert_eq!(events.len(), 2, "实际 {:?}", events);
    assert!(matches!(events[0], Event::Waiting { .. }));
    assert!(matches!(events[1], Event::Done { .. }));
    if let Event::Waiting { context, .. } = &events[0] {
        assert_eq!(context.as_deref(), Some("rm -rf build"));
    }
}

/// tools/call 最终响应作为兜底 idle:即使没有 task_complete 也能收尾。
#[test]
fn tools_call_result_closes_turn() {
    let frames = [
        json!({"method":"codex/event","params":{"msg":{"type":"task_started","turn_id":"1"}}}),
        json!({"jsonrpc":"2.0","id":2,"result":{"structuredContent":{"threadId":"x","content":"42"}}}),
    ];
    let events = run_frames("codex", &frames);
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], Event::Done { context, .. } if context.as_deref()==Some("42")));
}
