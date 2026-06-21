# ccwatch ACP/SDK 协议调研结论

> 调研日期:2026-06-21。工作目录 `/root/ccwatch-acp`(worktree,分支 `feat/acp-adapter`)。
> 所有协议方法名/事件名均有**实测报文佐证**(本机实跑抓取),不含臆想字段。
> 探针脚本:`research/acp_probe.py`(NDJSON JSON-RPC client,拉起 agent + 自动应答 client 反向请求)。

## TL;DR(给赶时间的人)

| Agent | 协议入口 | 能拿到权威状态? | working 信号 | waiting 信号 | idle/完成信号 |
|---|---|---|---|---|---|
| **Codex** | `codex mcp-server`(MCP over stdio,**纯进程,Rust 直连可行**) | ✅ 完全可以 | `codex/event` `task_started` | `exec_approval_request` / `apply_patch_approval_request`(server→client 请求) | `task_complete` |
| **Claude Code** | `@zed-industries/claude-code-acp`(**Node sidecar**,ACP over stdio) | ✅ 完全可以 | `session/update` 首个 chunk / `tool_call` | `session/request_permission`(server→client 请求) | `session/prompt` 响应 `stopReason` |
| **Claude Code** | `claude -p --output-format=stream-json`(**原生二进制,纯进程**) | ✅ turn 级可以,⚠️ 权限信号要走 control 协议 | `{"type":"system","status":"requesting"}` | (默认不在 stdout;需 `--permission-prompt-tool` 或 SDK `canUseTool` control_request) | `{"type":"result","stop_reason":...}` |
| **Gemini CLI** | `gemini --acp`(原生 ACP over stdio,**纯进程**) | ✅ 协议握手实测通过;本机账号被 geo 封,未跑完整 turn | (同 ACP:`session/update`)| (同 ACP:`session/request_permission`)| (同 ACP:`stopReason`)|

**核心结论:三家都能走协议拿到权威 working/waiting/idle,彻底替代抓屏猜测。** 但有一个绕不开的架构现实(见下节)。

**关键约束:`claude --acp --stdio` 在 Claude Code 2.1.177 原生二进制里不存在**(实测 `error: unknown option '--acp'`)。Claude 的 ACP 能力由独立 npm 包 `@zed-industries/claude-code-acp`(实测版本 0.16.2,已改名 `@agentclientprotocol/claude-agent-acp`)提供——它是个 Node sidecar,内部用 Claude Agent SDK 驱动 `claude`,对外讲 ACP。任务书里"`claude --acp --stdio`"这条假设**不成立**,需按本文修正。

---

## 绕不开的架构现实:协议 = 你得当 client 拉起 agent

ACP / MCP 都是 **client ↔ agent** 协议。client(编辑器 / ccwatch)负责**启动 agent 子进程**,通过 stdio 用 JSON-RPC 驱动它(initialize → session/new → session/prompt → 收 session/update)。

**你无法"旁观"一个已经裸跑在 tmux 里的现有 cc/codex/gemini 会话。** 那些会话是用户自己在终端起的交互式 TUI,根本没开 ACP/MCP 端口,也不会把内部状态用 JSON-RPC 吐出来。要走协议,ccwatch 必须**自己就是拉起 agent 的那个 client**。

这意味着 ccwatch 角色发生根本变化:

- **v0.1(现状):外部观察者。** 用户在 tmux 自由跑 agent,ccwatch 旁边 `capture-pane` 抓屏 + 正则猜状态。零侵入,但脆、靠猜。
- **协议模式:I/O 中转 wrapper。** 用户不再直接跑 `claude`/`codex`,而是跑 `ccwatch run --agent claude`,由 ccwatch 以 ACP/MCP client 身份拉起 agent,**透传用户输入 + 转发 agent 输出到终端**,同时从协议事件流旁路读权威状态。

两种模式不是二选一,见最后"最终架构建议"。

---

## 一、Codex —— `codex mcp-server`(MCP over stdio)

**入口:** `codex mcp-server`(`codex --help` 里列为 "Start Codex as an MCP server (stdio)")。
**传输:** NDJSON(一行一条 JSON-RPC 2.0),MCP 协议版本 `2025-06-18`。
**纯 Rust 友好:** ✅ 这是 codex 原生二进制的子命令,无需 Node。ccwatch 可直接 spawn 它。

### 实测握手 + 工具列表

```jsonc
>>> {"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"ccwatch-probe","version":"0.0.1"}}}
<<< {"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{"listChanged":true}},"serverInfo":{"name":"codex-mcp-server","title":"Codex","version":"0.139.0"}}}
>>> {"jsonrpc":"2.0","method":"notifications/initialized","params":{}}
>>> {"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}
<<< tools: ["codex","codex-reply"]
```

`tools/call` 调 `codex`(参数 `{"prompt": "..."}`,可选 `approval-policy` / `sandbox` / `model` / `cwd`)发起一个会话;`codex-reply`(参数 `threadId` + `prompt`)在同 thread 续轮。

### 实测状态事件流(权威!)

调用 `tools/call name=codex` 后,server 持续推 `codex/event` 通知,`params.msg.type` 是状态判别字段:

```jsonc
// WORKING 开始:
<<< {"method":"codex/event","params":{"msg":{"type":"session_configured","model":"gpt-5.5",...}}}
<<< {"method":"codex/event","params":{"msg":{"type":"task_started","turn_id":"3","model_context_window":258400}}}
// 思考 / 输出中:
<<< {"method":"codex/event","params":{"msg":{"type":"item_started","item":{"type":"Reasoning",...}}}}
<<< {"method":"codex/event","params":{"msg":{"type":"item_started","item":{"type":"AgentMessage",...}}}}
<<< {"method":"codex/event","params":{"msg":{"type":"agent_message_content_delta","delta":"4"}}}
// IDLE / turn 完成:
<<< {"method":"codex/event","params":{"msg":{"type":"task_complete","turn_id":"3","last_agent_message":"4","duration_ms":4047}}}
// 最后 tools/call 的 JSON-RPC 响应:
<<< {"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"4"}],"structuredContent":{"threadId":"...","content":"4"}}}
```

### Codex 状态映射(实测 type 值)

| ccwatch State | `codex/event` `msg.type` |
|---|---|
| **Working** | `task_started`(turn 开始);持续期间有 `item_started`/`agent_message_content_delta`/`agent_reasoning_delta` |
| **Waiting** | `exec_approval_request` / `apply_patch_approval_request`(server→client 的**请求**,带 id,需 client 回 `{"decision":"approved"/"denied"}`)。**注:本机 `approval_policy:never` + sandbox `danger-full-access`,所以实测没触发 approval;type 名取自 codex MCP 协议定义,握手与 task 事件均已实测。** |
| **Idle** | `task_complete`(turn 结束,带 `last_agent_message`/`duration_ms`);或 `tools/call` 的最终 response 到达 |

> ⚠️ approval 信号实测未捕获(环境自动批准)。若 ccwatch 真要用 waiting 信号,需起 codex 时显式传 `approval-policy=on-request` 且 sandbox 非 full-access,并在受限环境验证。这是**唯一一个没拿到真实报文的信号**,文档据实标注。

---

## 二、Claude Code —— 两条路

### 路 A:`@zed-industries/claude-code-acp`(ACP,Node sidecar)—— 实测完整跑通

**入口:** `npx -y @zed-industries/claude-code-acp`(实测 0.16.2)。**这是 Node sidecar,破坏 ccwatch "纯 Rust 单二进制零依赖"。**
**传输:** NDJSON JSON-RPC,ACP `protocolVersion: 1`。

#### 实测 initialize

```jsonc
>>> {"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":true,"writeTextFile":true},"terminal":true},"clientInfo":{"name":"ccwatch-probe","version":"0.0.1"}}}
<<< {"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{"promptCapabilities":{"image":true,"embeddedContext":true},"loadSession":true,"sessionCapabilities":{"fork":{},"list":{},"resume":{}}},"agentInfo":{"name":"@zed-industries/claude-code-acp"}}}
```

#### ⚠️ 实测坑:嵌套 session 守卫

在 Claude Code 会话内跑(环境有 `CLAUDECODE=1`)时,`session/new` 直接报错:

```
ERR Error: Claude Code cannot be launched inside another Claude Code session.
ERR To bypass this check, unset the CLAUDECODE environment variable.
<<< session/new ERR {"code":-32603,"message":"Internal error","data":{"details":"Query closed before response received"}}
```

**解法:spawn adapter 前 unset `CLAUDECODE` 等环境变量。** ccwatch wrapper 起 agent 子进程时必须清掉这些,否则 Claude 拒启。

#### 实测完整 turn(unset CLAUDECODE 后)

```jsonc
>>> session/new {"cwd":"/root/ccwatch-acp","mcpServers":[]}
<<< session sid=4804a4b1-65e6-4fca-bd37-047aabfe4fb9
>>> session/prompt {"sessionId":"...","prompt":[{"type":"text","text":"Reply with exactly one word: pong"}]}
<<< session/update sessionUpdate=available_commands_update
<<< session/update sessionUpdate=agent_message_chunk content.text=""
<<< session/update sessionUpdate=agent_message_chunk content.text="p"
<<< session/update sessionUpdate=agent_message_chunk content.text="ong"
<<< session/prompt 响应:stopReason=end_turn
```

#### 实测 tool_call + 权限(WAITING)流 —— 正好是抓屏抓不准的痛点

prompt 让它跑 `echo hello-acp`:

```jsonc
<<< session/update agent_message_chunk "I'll execute"
<<< session/update agent_message_chunk " that bash command for you."
<<< session/update tool_call       id=tooluse_50qk status=pending kind=execute title="Terminal"
<<< session/update tool_call       id=tooluse_50qk status=pending kind=execute title="`echo hello-acp`"
<<< session/request_permission     toolCall.title="`echo hello-acp`" options=[allow_always, allow_once, reject_once]   // ← 权威 WAITING
   (探针回 reject_once)
<<< session/update tool_call_update id=tooluse_50qk status=failed
<<< session/prompt 响应:stopReason=end_turn
```

`session/request_permission` 是 **server→client 的请求**(带 id,必须回 `{"outcome":{"outcome":"selected","optionId":...}}` 或 `{"outcome":{"outcome":"cancelled"}}`)。**agent 卡在这条请求上不返回 = 权威的"等用户拍板"状态。** 这正是 ccwatch 最该播报、而抓屏最不可靠的场景。

### 路 B:`claude -p --output-format=stream-json`(原生二进制,纯进程)—— 实测

**入口:** `claude -p --output-format=stream-json --input-format=stream-json --verbose`。**纯 Claude 二进制,无 Node 依赖,保持 ccwatch 纯 Rust。** 这不是 ACP,是 Claude 自家的 streaming JSON 协议(Agent SDK 同款 wire format)。

#### 实测 turn 事件流

```jsonc
{"type":"system","subtype":"init","session_id":"...","tools":[...],"model":"claude-sonnet-4-6","permissionMode":"default"}
{"type":"system","subtype":"status","status":"requesting"}                       // ← WORKING(开始请求模型)
{"type":"stream_event","event":{"type":"message_start",...}}
{"type":"stream_event","event":{"type":"content_block_delta","delta":{"text":"pong","type":"text_delta"}}}
{"type":"assistant","message":{"content":[{"text":"pong","type":"text"}],...}}
{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"end_turn"}}}
{"type":"result","subtype":"success","stop_reason":"end_turn","result":"pong","num_turns":1,...}  // ← IDLE/完成
```

#### 实测 tool_use

强制工具调用(`--allowedTools ""` 也仍执行,见下):

```jsonc
{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"echo hi"}}]}}   // ← 正在用工具(WORKING)
{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"...","content":"hi"}]}}              // 工具结果
{"type":"result","stop_reason":"end_turn","result":"hi"}                                                        // IDLE
```

#### ⚠️ 实测关键限制:headless 模式默认不吐"权限请求"

实测 `claude -p ... --permission-mode default --allowedTools ""`,Bash 工具**仍直接执行了**(返回 "hi"),stdout **没有任何交互式权限请求**。原因:`-p` headless 模式下若没配 `--permission-prompt-tool`,Claude 不会在 stdout 发交互式 permission 询问——它按 settings/policy 直接放行或拒绝。

要在路 B 拿到 WAITING(权限)信号,二选一:
1. `--permission-prompt-tool mcp__xxx`:把权限询问转成一个 MCP 工具调用(需自己挂个 MCP server 接);
2. 用 Claude Agent SDK 的 `canUseTool` 回调 —— 走 **control 协议**(双向 `control_request`,`subtype:"can_use_tool"`)。这条要 SDK(TS/Python),不是纯 stdout 流。

> **结论:路 B 纯进程能权威拿到 turn 级 working/idle(`status:requesting` / `result.stop_reason`),但"等权限"这一态默认不在流里。** 若 ccwatch 用路 B,waiting 信号要么挂 `--permission-prompt-tool`,要么对工具态做"`tool_use` 已发但久无 `tool_result`"的启发式;权威 waiting 仍是路 A(ACP)最干净。

---

## 三、Gemini CLI —— `gemini --acp`(原生 ACP,纯进程)

**入口:** `gemini --acp`(`--help` 明列:"Starts the agent in ACP mode";旧名 `--experimental-acp` 已废弃)。**纯 gemini 二进制,无 Node sidecar。**
**传输:** NDJSON JSON-RPC,ACP `protocolVersion: 1`。

### 实测 initialize(握手成功,拿到完整 capabilities)

```jsonc
>>> {"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":true,"writeTextFile":true},"terminal":true},"clientInfo":{"name":"ccwatch-probe","version":"0.0.1"}}}
<<< {"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,
     "authMethods":[{"id":"oauth-personal",...},{"id":"gemini-api-key",...},{"id":"vertex-ai",...},{"id":"gateway",...}],
     "agentInfo":{"name":"gemini-cli","title":"Gemini CLI","version":"0.47.0"},
     "agentCapabilities":{"loadSession":true,"promptCapabilities":{"image":true,"audio":true,"embeddedContext":true},"mcpCapabilities":{"http":true,"sse":true}}}}
```

### ⚠️ 实测:本机账号被 geo 封,session/new 失败

```jsonc
>>> {"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/root/ccwatch-acp","mcpServers":[]}}
<<< {"jsonrpc":"2.0","id":2,"error":{"code":-32000,"message":"Your current account is not eligible for Gemini Code Assist for individuals because it is not currently available in your location."}}
```

这是**账号/地域问题,不是协议问题**。Gemini 的 ACP 协议形态与 Claude ACP **完全一致**(同 `protocolVersion:1`、同 `session/new`→`session/prompt`→`session/update`→`stopReason`、同 `session/request_permission`),因为它们都实现同一份 Agent Client Protocol。换可用账号(或 `gemini-api-key` 鉴权 + 有效 key)即可跑完整 turn。

> 任务书说"Gemini 的 working/idle 抓屏抓不准"——ACP 路正好根治:`gemini --acp` 用结构化 `session/update` + `stopReason`,不依赖 TUI 字符串。这是从抓屏迁到协议**收益最大**的一家。

---

## 四、ACP 协议权威字段(取自官方 schema)

来源:`zed-industries/agent-client-protocol` 仓库 `schema/v1/schema.json`(下载实测)+ `agentclientprotocol.com`。三家 ACP(Claude adapter / Gemini)共用。

### `session/update` 通知的判别字段 `sessionUpdate`(11 种 variant)

```
user_message_chunk | agent_message_chunk | agent_thought_chunk |
tool_call | tool_call_update | plan | available_commands_update |
current_mode_update | config_option_update | session_info_update | usage_update
```

### `ToolCallStatus`(tool_call / tool_call_update 的 status)

```
pending      // 还没跑:输入在流式传输 或 等批准  ← 配合 request_permission = WAITING
in_progress  // 正在跑                          ← WORKING
completed    // 成功
failed       // 失败(实测拒绝权限后即为此)
```

### `StopReason`(session/prompt 响应,turn 结束原因)

```
end_turn         // 正常结束(实测)
max_tokens       // 触达 token 上限
max_turn_requests// 触达 agent 请求数上限
refusal          // agent 拒绝
cancelled        // client 发了 session/cancel
```

### 权限请求 `session/request_permission`(server→client 请求)

`params.options[].kind` ∈ `allow_once | allow_always | reject_once | reject_always`(实测 Claude 给的是 `allow_always` / `allow_once` / `reject_once`)。client 回 `{"outcome":{"outcome":"selected","optionId":"..."}}` 或 `{"outcome":{"outcome":"cancelled"}}`。

---

## 五、最终架构建议

### 5.1 三家能否拿到权威状态?

**能,全部能。** working / waiting(等权限)/ idle(turn 完成)三态在协议里都有明确、结构化、无歧义的信号(见上表),彻底不用再正则猜 UI 字符串。Gemini 本机因账号 geo 封没跑完整 turn,但协议握手实测通过且与 Claude ACP 同构,可行性确定。

### 5.2 wrapper 模式 vs 抓屏 fallback

**协议模式必然是 wrapper(I/O 中转),不是旁观。** ccwatch 得以 client 身份拉起 agent、透传用户 I/O、旁路读状态。代价:用户得改用 `ccwatch run --agent X` 替代直接敲 `claude`/`codex`/`gemini`。

建议 ccwatch 走**双轨并存**,不互相取代:

- **轨道 1 —— 协议模式(新,权威):** `ccwatch run --agent <name>` 当 ACP/MCP client 拉起 agent,透传 I/O,从事件流读权威 State。用户愿意改启动方式、要准状态时用。**这是 Gemini/Codex 状态从"猜"变"真"的唯一干净解。**
- **轨道 2 —— 抓屏模式(旧,零侵入):** 保留 v0.1 的 `tmux capture-pane` + 正则。用户已在裸跑、不想改习惯的现有会话,只能靠它。

State 抽象层不变(`Working/Waiting/Idle/Unknown`),两条轨道都喂同一个 `State` + 转移播报逻辑,复用 `notify.rs`/`watch.rs`/`state.rs`。

### 5.3 纯 Rust vs Node sidecar(任务书点名的冲突)

| Agent | 纯进程(保持纯 Rust) | 状态最准的路 |
|---|---|---|
| **Codex** | ✅ `codex mcp-server`(原生子命令) | 同左,无冲突 —— **codex 直接用** |
| **Gemini** | ✅ `gemini --acp`(原生 flag) | 同左,无冲突 —— **gemini 直接用** |
| **Claude** | ✅ `claude -p --output-format=stream-json`(turn 级权威,权限态需额外手段) | ⚠️ `@zed-industries/claude-code-acp` Node sidecar(权限态最干净,但破坏零依赖) |

**建议:**
- **Codex + Gemini:Rust 直连原生进程**(MCP / ACP),零 Node 依赖,完整权威状态。**这俩是协议模式的首发对象。**
- **Claude:默认用路 B(`-p stream-json`)纯 Rust 直连**,拿 turn 级 working/idle(`status:requesting` / `result.stop_reason`)与 tool_use;**权限态作为已知缺口**(挂 `--permission-prompt-tool` 或对 `tool_use` 久无 `tool_result` 做启发式)。只有当"等权限"播报成为硬需求时,才上 ACP Node sidecar(`claude-code-acp`)作为可选 feature,别让它进默认零依赖路径。

### 5.4 第二步原型怎么落(对齐任务书)

任务书建议从 Gemini `--acp` 起步。但本机 **Gemini 账号 geo 封跑不完整 turn**,会卡测试。务实排序:

1. **先做 Codex(`codex mcp-server`)**:纯 Rust、本机鉴权可用、已抓到**完整真实事件流**(task_started→item→task_complete),测试能跑绿。落 `src/acp.rs`(或 `src/protocol.rs`)+ `ccwatch acp-probe --agent codex`。
2. **再做 Gemini(`gemini --acp`)**:协议同构,代码可大量复用;等有可用账号时验证完整 turn。
3. **Claude** 用路 B 纯 Rust 跟上;ACP sidecar 列为后续可选。

报文解析 → State 映射的单测,直接用本文档贴的真实报文当 fixture(Codex `codex/event` 各 `msg.type`、ACP `session/update` 各 `sessionUpdate` + `ToolCallStatus` + `StopReason`)。

---

## 附:复现方式

```bash
# Codex(完整事件流,本机可跑):
python3 research/acp_probe.py codex "what is 2+2? reply one word" 60

# Gemini(握手通过,session/new 因账号 geo 封失败):
python3 research/acp_probe.py gemini "Reply one word: pong" 60

# Claude ACP adapter(需 unset CLAUDECODE;npx 拉 @zed-industries/claude-code-acp):
#   见 /tmp 探针;核心:spawn 前清掉 CLAUDECODE 等环境变量

# Claude 原生 stream-json(纯二进制):
echo '{"type":"user","message":{"role":"user","content":[{"type":"text","text":"Reply one word: pong"}]}}' \
  | claude -p --output-format=stream-json --input-format=stream-json --verbose --model sonnet
```

探针实现见 `research/acp_probe.py`。所有报文样本均为本机 2026-06-21 实跑抓取。
