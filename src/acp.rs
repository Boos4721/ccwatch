//! ACP/MCP 协议模式:作为 client 拉起 agent 子进程,从协议事件流读**权威**状态。
//!
//! 这是抓屏模式(classify.rs)之外的第二条轨道。两条轨道复用同一个
//! [`crate::classify::State`] 抽象 —— 抓屏靠正则猜,协议靠结构化事件读,
//! 产出的 State 一致,后续 state.rs 的转移播报逻辑两边通用。
//!
//! 本模块先打通 **Codex**(`codex mcp-server`,MCP over stdio,纯 Rust 直连,
//! 无 Node 依赖)。报文形态见 `docs/ACP_RESEARCH.md`(均为实测抓取)。
//!
//! 状态映射(实测 `codex/event` 的 `msg.type`):
//!   - `task_started`                    → Working(turn 开始)
//!   - `exec_approval_request` /
//!     `apply_patch_approval_request`     → Waiting(等用户批准工具)
//!   - `task_complete`                   → Idle(turn 完成)
//!   - 其余(session_configured / item_* / *_delta / token_count / mcp_startup_*)→ 无状态变化

use crate::classify::State;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};

/// 协议事件解析出的一次状态信号。
///
/// `context` 给播报用(如等批准的命令、完成时最后一句),可能为空。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateSignal {
    pub state: State,
    pub context: Option<String>,
}

impl StateSignal {
    fn new(state: State, context: Option<String>) -> StateSignal {
        StateSignal { state, context }
    }
}

/// 把一条 Codex MCP 报文(已解析为 JSON)映射成可能的状态信号。
///
/// 纯函数,无副作用 —— 单测直接喂实测报文 fixture 验证。
/// 返回 `None` 表示这条报文不引起状态变化(噪音/中间事件)。
///
/// 识别两类输入:
///   1. `codex/event` 通知:看 `params.msg.type`(主路径,实测报文都走这条)。
///   2. `tools/call` 的最终响应(带 `result.structuredContent`):turn 收尾 → Idle。
pub fn classify_codex_frame(frame: &Value) -> Option<StateSignal> {
    let method = frame.get("method").and_then(Value::as_str);

    // tools/call 的最终响应:turn 结束(兜底的 Idle 信号,task_complete 之后到达)。
    if method.is_none() && frame.get("result").is_some() {
        if let Some(sc) = frame.get("result").and_then(|r| r.get("structuredContent")) {
            let ctx = sc
                .get("content")
                .and_then(Value::as_str)
                .map(|s| truncate_chars(s, 120));
            return Some(StateSignal::new(State::Idle, ctx));
        }
        return None;
    }

    if method != Some("codex/event") {
        return None;
    }
    let msg = frame.get("params").and_then(|p| p.get("msg"))?;
    classify_codex_msg(msg)
}

/// 映射 `codex/event` 的 `msg` 体。拆出来便于单测直接喂 msg。
pub fn classify_codex_msg(msg: &Value) -> Option<StateSignal> {
    let typ = msg.get("type").and_then(Value::as_str)?;
    match typ {
        // turn 开始 = 正在干活。
        "task_started" => Some(StateSignal::new(State::Working, None)),

        // 等用户批准执行命令 / 应用补丁 = 卡住等拍板。
        // 注:本机环境(approval_policy=never)未触发,type 名取自 codex MCP 协议;
        // 映射逻辑用合成 fixture 单测覆盖。详见 docs/ACP_RESEARCH.md。
        "exec_approval_request" | "apply_patch_approval_request" => {
            Some(StateSignal::new(State::Waiting, approval_context(msg)))
        }

        // turn 完成 = 空闲待命。带最后一句 agent 消息当上下文。
        "task_complete" => {
            let ctx = msg
                .get("last_agent_message")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(|s| truncate_chars(s, 120));
            Some(StateSignal::new(State::Idle, ctx))
        }

        // 其余都是 turn 内中间事件或启动噪音,不改变 working/waiting/idle 大态。
        _ => None,
    }
}

/// 从 approval 请求里尽量抽出"在等什么"的上下文(命令行 / 补丁路径)。
/// 字段形态本机未实测,做防御式提取,取不到就返回 None。
fn approval_context(msg: &Value) -> Option<String> {
    // 常见可能:msg.command(数组或字符串)、msg.call.command、msg.cwd。
    if let Some(cmd) = msg.get("command") {
        if let Some(s) = cmd.as_str() {
            return Some(truncate_chars(s, 120));
        }
        if let Some(arr) = cmd.as_array() {
            let joined = arr
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" ");
            if !joined.is_empty() {
                return Some(truncate_chars(&joined, 120));
            }
        }
    }
    None
}

/// 按字符截断(保留中文),与 classify.rs 的口径一致。
fn truncate_chars(s: &str, max: usize) -> String {
    let t = s.trim();
    if t.chars().count() <= max {
        return t.to_string();
    }
    let cut: String = t.chars().take(max).collect();
    format!("{}…", cut)
}

// ============================================================================
// Codex MCP client 驱动:拉起 `codex mcp-server`,走 NDJSON JSON-RPC。
// ============================================================================

/// MCP 协议版本(实测 codex 0.139.0 应答此版本)。
const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

/// 一次解析出的状态信号 + 原始 type(probe 打印用)。
#[derive(Debug, Clone)]
pub struct ProbeEvent {
    pub signal: Option<StateSignal>,
    /// 原始事件标签(codex/event 的 msg.type,或 "tools/call result")。
    pub raw_kind: String,
}

/// Codex MCP client:持有子进程 + stdin,负责发请求。
pub struct CodexClient {
    child: Child,
    stdin: ChildStdin,
    next_id: i64,
}

impl CodexClient {
    /// 拉起 `codex mcp-server`。可选 `cwd` 作为 codex 工作目录。
    pub fn spawn(cwd: Option<&str>) -> Result<CodexClient> {
        let mut cmd = Command::new("codex");
        cmd.arg("mcp-server")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        let mut child = cmd.spawn().context("拉起 `codex mcp-server` 失败(codex 没装?)")?;
        let stdin = child.stdin.take().context("拿不到 codex stdin")?;
        Ok(CodexClient {
            child,
            stdin,
            next_id: 0,
        })
    }

    fn new_id(&mut self) -> i64 {
        self.next_id += 1;
        self.next_id
    }

    /// 发一行 NDJSON JSON-RPC。
    async fn send(&mut self, obj: &Value) -> Result<()> {
        let mut line = serde_json::to_string(obj).context("序列化 JSON-RPC 失败")?;
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .await
            .context("写 codex stdin 失败")?;
        self.stdin.flush().await.context("flush codex stdin 失败")?;
        Ok(())
    }

    async fn request(&mut self, id: i64, method: &str, params: Value) -> Result<()> {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .await
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.send(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
        .await
    }

    async fn respond(&mut self, id: &Value, result: Value) -> Result<()> {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        }))
        .await
    }

    /// 跑一个完整 turn:initialize → notifications/initialized → tools/call codex,
    /// 流式读 `codex/event`,对每个引起状态变化的事件回调 `on_state`。
    ///
    /// agent 反向请求(elicitation 等)自动应答以免卡死;approval 默认拒绝
    /// (probe 语义:观察到 Waiting 即可,不真的放行工具)。
    /// turn 的 `tools/call` 响应到达即结束。`timeout` 是整体超时。
    pub async fn run_turn<F>(
        &mut self,
        prompt: &str,
        approval_policy: &str,
        sandbox: &str,
        timeout: Duration,
        mut on_state: F,
    ) -> Result<()>
    where
        F: FnMut(ProbeEvent),
    {
        let stdout = self.child.stdout.take().context("拿不到 codex stdout")?;
        let mut reader = BufReader::new(stdout).lines();
        let deadline = Instant::now() + timeout;

        // 1) initialize
        let init_id = self.new_id();
        self.request(
            init_id,
            "initialize",
            json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "ccwatch", "version": env!("CARGO_PKG_VERSION")},
            }),
        )
        .await?;

        let mut call_id: Option<i64> = None;

        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                anyhow::bail!("codex turn 超时({}s 内没等到 task_complete)", timeout.as_secs());
            }
            let line = match tokio::time::timeout(remaining, reader.next_line()).await {
                Err(_) => anyhow::bail!("codex turn 超时"),
                Ok(r) => r.context("读 codex stdout 失败")?,
            };
            let Some(line) = line else {
                anyhow::bail!("codex 提前关闭 stdout(鉴权失败?)");
            };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let frame: Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(_) => continue, // 非 JSON 行(诊断输出)跳过。
            };

            // agent 反向请求(有 method 且有 id):自动应答。
            if frame.get("method").is_some() && frame.get("id").is_some() {
                self.handle_server_request(&frame).await?;
                // 反向请求也可能是状态信号(approval),分类一下。
                if let Some(sig) = classify_codex_frame(&frame) {
                    on_state(ProbeEvent {
                        signal: Some(sig),
                        raw_kind: server_request_kind(&frame),
                    });
                }
                continue;
            }

            // initialize 响应:发 initialized + tools/call。
            if frame.get("id").and_then(Value::as_i64) == Some(init_id)
                && frame.get("result").is_some()
            {
                self.notify("notifications/initialized", json!({})).await?;
                let cid = self.new_id();
                call_id = Some(cid);
                self.request(
                    cid,
                    "tools/call",
                    json!({
                        "name": "codex",
                        "arguments": {
                            "prompt": prompt,
                            "approval-policy": approval_policy,
                            "sandbox": sandbox,
                        },
                    }),
                )
                .await?;
                continue;
            }

            // tools/call 响应:turn 收尾。
            if call_id.is_some() && frame.get("id").and_then(Value::as_i64) == call_id {
                if let Some(err) = frame.get("error") {
                    anyhow::bail!("codex tools/call 报错: {}", err);
                }
                if let Some(sig) = classify_codex_frame(&frame) {
                    on_state(ProbeEvent {
                        signal: Some(sig),
                        raw_kind: "tools/call result".to_string(),
                    });
                }
                return Ok(());
            }

            // codex/event 通知:主状态流。
            if frame.get("method").and_then(Value::as_str) == Some("codex/event") {
                let kind = frame
                    .get("params")
                    .and_then(|p| p.get("msg"))
                    .and_then(|m| m.get("type"))
                    .and_then(Value::as_str)
                    .unwrap_or("?")
                    .to_string();
                let sig = classify_codex_frame(&frame);
                on_state(ProbeEvent {
                    signal: sig,
                    raw_kind: kind,
                });
            }
        }
    }

    /// 自动应答 agent 的反向请求,避免它卡住。
    /// approval 类一律拒绝(probe 不真的放行);其余回空 result。
    async fn handle_server_request(&mut self, frame: &Value) -> Result<()> {
        let id = frame.get("id").cloned().unwrap_or(Value::Null);
        let method = frame.get("method").and_then(Value::as_str).unwrap_or("");
        if method.contains("approval") || method.contains("elicit") {
            // 拒绝/取消语义:不同 codex 版本字段可能不同,给一个通用拒绝。
            self.respond(&id, json!({"decision": "denied"})).await
        } else {
            self.respond(&id, json!({})).await
        }
    }

    /// 等子进程退出(probe 结束时调用)。
    pub async fn shutdown(mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
}

/// 反向请求的状态标签。
fn server_request_kind(frame: &Value) -> String {
    frame
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("server-request")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- 实测报文 fixture(2026-06-21 本机 `codex mcp-server` 抓取)----
    // 见 docs/ACP_RESEARCH.md。下面的 JSON 是真实 codex/event 帧的精简。

    /// task_started → Working。
    #[test]
    fn task_started_is_working() {
        let frame = json!({
            "jsonrpc": "2.0",
            "method": "codex/event",
            "params": {
                "_meta": {"requestId": 3},
                "msg": {
                    "type": "task_started",
                    "turn_id": "3",
                    "started_at": 1781973039,
                    "model_context_window": 258400,
                    "collaboration_mode_kind": "default"
                },
                "id": "3"
            }
        });
        let sig = classify_codex_frame(&frame).expect("task_started 应产出信号");
        assert_eq!(sig.state, State::Working);
        assert_eq!(sig.context, None);
    }

    /// task_complete → Idle,带 last_agent_message 当上下文。
    #[test]
    fn task_complete_is_idle_with_context() {
        let frame = json!({
            "jsonrpc": "2.0",
            "method": "codex/event",
            "params": {
                "_meta": {"requestId": 3},
                "msg": {
                    "type": "task_complete",
                    "turn_id": "3",
                    "last_agent_message": "The current user is `root`.",
                    "completed_at": 1781973043,
                    "duration_ms": 4047
                },
                "id": "3"
            }
        });
        let sig = classify_codex_frame(&frame).expect("task_complete 应产出信号");
        assert_eq!(sig.state, State::Idle);
        assert_eq!(sig.context.as_deref(), Some("The current user is `root`."));
    }

    /// task_complete 没有 last_agent_message 时,context 为 None。
    #[test]
    fn task_complete_without_message_has_no_context() {
        let msg = json!({"type": "task_complete", "turn_id": "1", "duration_ms": 10});
        let sig = classify_codex_msg(&msg).expect("应产出 idle");
        assert_eq!(sig.state, State::Idle);
        assert_eq!(sig.context, None);
    }

    /// 中间事件(session_configured / item_started / *_delta / token_count /
    /// mcp_startup_*)都不引起状态变化 —— 全是实测见过的 type。
    #[test]
    fn intermediate_events_yield_no_signal() {
        for typ in [
            "session_configured",
            "mcp_startup_update",
            "mcp_startup_complete",
            "item_started",
            "item_completed",
            "raw_response_item",
            "user_message",
            "agent_message_content_delta",
            "agent_message",
            "token_count",
        ] {
            let msg = json!({"type": typ});
            assert!(
                classify_codex_msg(&msg).is_none(),
                "{} 不该产出状态信号",
                typ
            );
        }
    }

    /// agent_message_content_delta(实测)是中间事件,不改大态。
    #[test]
    fn content_delta_is_noise() {
        let frame = json!({
            "method": "codex/event",
            "params": {"msg": {
                "type": "agent_message_content_delta",
                "delta": "4",
                "turn_id": "3"
            }}
        });
        assert!(classify_codex_frame(&frame).is_none());
    }

    /// exec_approval_request → Waiting(合成 fixture:本机 approval_policy=never
    /// 未触发真实报文,type 名取自 codex MCP 协议,映射逻辑在此覆盖)。
    #[test]
    fn exec_approval_is_waiting() {
        let msg = json!({
            "type": "exec_approval_request",
            "command": ["bash", "-lc", "rm -rf build"],
            "cwd": "/repo"
        });
        let sig = classify_codex_msg(&msg).expect("approval 应产出信号");
        assert_eq!(sig.state, State::Waiting);
        assert_eq!(sig.context.as_deref(), Some("bash -lc rm -rf build"));
    }

    /// apply_patch_approval_request 同样 → Waiting。
    #[test]
    fn apply_patch_approval_is_waiting() {
        let msg = json!({"type": "apply_patch_approval_request"});
        let sig = classify_codex_msg(&msg).expect("approval 应产出信号");
        assert_eq!(sig.state, State::Waiting);
        assert_eq!(sig.context, None);
    }

    /// approval 的 command 是字符串形态时也能抽出上下文。
    #[test]
    fn approval_context_from_string_command() {
        let msg = json!({"type": "exec_approval_request", "command": "git push --force"});
        let sig = classify_codex_msg(&msg).unwrap();
        assert_eq!(sig.context.as_deref(), Some("git push --force"));
    }

    /// tools/call 的最终响应(实测结构)→ Idle,带 structuredContent.content。
    #[test]
    fn tools_call_result_is_idle() {
        let frame = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "result": {
                "content": [{"type": "text", "text": "4"}],
                "structuredContent": {
                    "threadId": "019ee5de-b818-7800-82df-4b6db477682c",
                    "content": "4"
                }
            }
        });
        let sig = classify_codex_frame(&frame).expect("tools/call 响应应产出 idle");
        assert_eq!(sig.state, State::Idle);
        assert_eq!(sig.context.as_deref(), Some("4"));
    }

    /// 不认识的帧(普通 JSON-RPC 响应、未知通知)不产出信号。
    #[test]
    fn unknown_frames_yield_none() {
        assert!(classify_codex_frame(&json!({"jsonrpc": "2.0", "id": 1, "result": {}})).is_none());
        assert!(classify_codex_frame(&json!({"method": "notifications/foo", "params": {}})).is_none());
        assert!(classify_codex_frame(&json!({"foo": "bar"})).is_none());
    }

    /// 长上下文按字符截断(中文安全)。
    #[test]
    fn long_context_truncated() {
        let long = "字".repeat(200);
        let msg = json!({"type": "task_complete", "last_agent_message": long});
        let sig = classify_codex_msg(&msg).unwrap();
        let ctx = sig.context.unwrap();
        assert!(ctx.chars().count() <= 121, "应截断到 ~120 字 + 省略号");
        assert!(ctx.ends_with('…'));
    }
}
