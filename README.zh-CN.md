# ccwatch（中文）

> [English](README.md)

通用 **AI agent 会话监控器**。监控 tmux 里跑的 agent CLI（Claude Code / Codex / Gemini / 以后更多），检测每个会话的状态（干活中 `working` / 卡住等输入 `waiting` / 空闲待命 `idle`），**只在状态发生转移时**播报一条消息。

背景：同时开多个 cc/codex 会话干活，不想反复手动问"进度?"，也受够了 agent 干完/卡住不吭声。

- 纯 Rust 单二进制，零运行时依赖。
- 配置驱动：加新 agent = 加一段 `[[profiles]]`，不改代码。
- 双轨:**抓屏**(tmux pane 正则,零侵入)或**协议**(以 ACP/MCP 拉起 agent 读权威状态)。`auto` 优先协议、不可用回退抓屏。
- 三模式:`once`(给 cron)/ `daemon`(常驻自投递)/ `check`(调试),外加 `status`(一屏总览)和 `say`(往会话发消息)。

## 构建

```bash
cargo build --release
# 产物：target/release/ccwatch（单二进制）
```

## 配置

复制示例配置到默认位置（或用 `--config` 指定任意路径）：

```bash
mkdir -p ~/.config/ccwatch
cp config.example.toml ~/.config/ccwatch/config.toml
```

配置查找顺序：`--config <PATH>` > `~/.config/ccwatch/config.toml` > `./config.example.toml`。

关键项见 `config.example.toml` 注释。摘要：

- `[general]` — 监控哪些会话（`session_prefixes`）、抓多少行（`capture_lines`）、轮询间隔（`poll_interval_secs`）、状态文件路径（`state_file`，支持 `~`）。
- `[delivery]` — `none`（只打印 stdout）或 `telegram`（填 `bot_token` / `chat_id`，可选 `proxy`）。
- `[transitions]` — 哪些转移要播报的开关。
- `[[profiles]]` — 每个 agent 的 pane 特征正则。

## 三种模式

### `ccwatch once` —— 跑一遍，打印 stdout（给 cron）

扫描一遍，把**状态转移事件**打印到 stdout；无转移则**完全静默**（空输出）。首遍运行只建基线，不播报。适合挂定时任务：

```bash
ccwatch once   # 无变化时 stdout 为空
```

### `ccwatch daemon` —— 常驻循环，自投递

按 `poll_interval_secs` 轮询，自己投递（`delivery.mode` 决定打印 stdout 还是发 Telegram）：

```bash
ccwatch daemon                 # 用配置里的间隔
ccwatch daemon --interval 15   # 覆盖为 15s
```

投递失败时本轮不写状态文件，下一轮自动重试（不会漏报）。

### `ccwatch check` —— 列出当前会话 + 状态（调试）

```bash
ccwatch check
# SESSION          PROFILE  STATE    CONTEXT
# ccA              claude   idle     C-2 系统监控 API
# ccD              claude   working
```

### `ccwatch say` —— 往会话发指令（双向控制）

```bash
ccwatch say ccA "继续推进 B 计划"               # 抓屏:tmux send-keys + Enter
ccwatch say codex-a "修一下测试" --mode protocol
```

抓屏模式是可靠两步:`send-keys -l` 先把字面文本塞进输入行,再单独发一次 `send-keys Enter` 提交(tmux 经常不补 Enter 就不提交)。协议模式把消息当一个 turn 经 CodexClient 发给 Codex。

## 状态分类逻辑

对每个匹配的 tmux 会话：

1. `tmux capture-pane -t <session> -p -S -<capture_lines>` 抓 pane 末尾文本。
2. 选 profile：先按 `session_match` 正则匹配会话名；认不出再用 `detect` 正则嗅探 pane 内容。都不中跳过。
3. 按优先级匹配：**working > waiting > idle**。每个状态一组正则，任一命中即该状态；都不中 = `unknown`。

> codex 和 claude 的 working 都含 `esc to interrupt`，codex idle 则有 `›` 空输入框——靠 working 规则的优先级 + idle 的 `(?m)^›\s*$` 区分。

## 转移播报规则

| 转移 | 播报 | 开关 |
|------|------|------|
| working/waiting/unknown → idle | `✅ <session> 干完了，空闲待命` | `notify_done` |
| 任意 → waiting | `⏸ <session> 卡住了，在等你…`（带子类型，见下） | `notify_waiting` |
| idle/waiting → working | `▶ <session> 开始干了`（默认关） | `notify_working` |
| 会话消失 | `⚫ <session> 会话已结束` | `notify_gone` |
| 新会话首次见到且已 waiting | `⏸ <session> 一上来就在等你…` | `notify_new_waiting` |
| working 但内容持续无变化超 `stuck_threshold_secs` | `⚠ <session> 疑似卡住了（已 Nm 无变化）` | `notify_stuck` |

播报带一句上下文：waiting 抓最后的菜单/提问行，idle 抓最近的 `●`/`✻`/`✔` 总结行，截断到 ~120 字符。

### waiting 子类型

`waiting` 进一步细分，让播报说清**在等什么**：

- `approval` —— 等 y/n 审批（如 bypass permissions / trust 提示）。
- `input` —— 等用户输入文本。
- `menu` —— 等菜单 / 方向键选择。

抓屏模式靠每个 profile 可选的子类型正则识别（`waiting_approval` / `waiting_input` / `waiting_menu`，优先级 approval > menu > input）；协议模式按事件类型映射（如 Codex 的 `*_approval_request` → `approval`）。子类型都不中时回退到通用文案。

### 卡住检测

会话停在 `working` 但实际卡死（等不存在的输入 / 死循环 / network hang）时不会转移，本来就不会被播报。ccwatch 据此补一刀：当 pane 内容(剥掉数字,让 spinner/计时器不算"进展")持续无变化超过 `stuck_threshold_secs`(默认 600)，播报一次 `⚠ 疑似卡住`。有真实进展(或离开 working)即重置计时，再次卡住能再报。

## 轨道与模式

`once` 和 `daemon` 都支持 `--mode screen|protocol|auto`：

- `screen`(默认)—— 抓 tmux pane。零侵入：监控你手动裸跑的会话。
- `protocol` —— ccwatch 以 ACP/MCP client 身份自己拉起 agent，读权威状态事件(当前 Codex 走 `codex mcp-server`)。两轨共用**同一套**转移规则与投递路径。
- `auto` —— 协议可用(如 `codex` 在 PATH)就走协议，否则回退抓屏。`daemon` 下协议运行期出错也回退抓屏循环。

```bash
ccwatch daemon --mode auto                       # 优先协议，回退抓屏
ccwatch daemon --mode protocol --agent codex     # 协议常驻监控
ccwatch once --mode protocol --label codex-a --prompt "..."
```

## 加新 agent profile

不用改代码，在配置里加一段：

```toml
[[profiles]]
name = "myagent"
session_match = "^my"               # 会话名正则（可选）
detect = "MyAgent banner|某关键词"   # pane 内容嗅探（会话名认不出时用）
working = ["esc to interrupt"]      # 任一命中即 working
waiting = ["Press Enter", "\\(y/n\\)"]
idle = ["(?m)^>\\s*$"]
```

用 `ccwatch check` 看分类对不对，必要时调正则。

## 挂 Hermes / cron

`once` 模式空输出即静默，天然适合 cron / Hermes no_agent 触发：

```cron
* * * * * /path/to/ccwatch once --config ~/.config/ccwatch/config.toml
```

有转移时 stdout 才有内容，由上层（cron 邮件 / Hermes）转发给你。要直连 Telegram 则改用 `daemon` + `delivery.mode = "telegram"`。
