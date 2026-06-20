# ccwatch — 实现任务书(给 cc)

## 这是什么
一个 Rust 写的通用 **AI agent 会话监控器**。监控 tmux 里跑的 agent CLI(Claude Code / Codex / Gemini / 以后更多),
检测每个会话的状态(干活中 WORKING / 卡住等输入 WAITING / 空闲待命 IDLE),只在**状态发生转移**时播报一条消息。
背景:用户同时开多个 cc/codex 会话干活,不想反复手动问"进度?",也受够了 agent 干完/卡住不吭声。

已有一个 Python 原型(`~/.hermes/scripts/cc_watchdog.py`)验证了思路,现在用 Rust 重写成正式项目:单二进制、零运行时依赖、可做常驻 daemon。

## 架构(已定:C 双模式)
- **core 逻辑做成模块**:扫描 tmux → 抓 pane → 分类状态 → diff 上次状态 → 产出转移事件。
- **两种运行模式**:
  - `ccwatch once`:跑一遍,把转移事件打印到 stdout(给 Hermes no_agent cron 用,空输出=静默)。
  - `ccwatch daemon`:常驻循环(按 poll_interval_secs),自己投递(Telegram 直连或打印)。
- **配置驱动**:`config.example.toml` 已给出完整结构 + 三家 agent 的**真实 pane 特征**(我实测抓的,别改特征值,照用)。加新 agent = 加 `[[profiles]]`,不改代码。

## 状态分类逻辑(核心)
对每个匹配的 tmux 会话:
1. `tmux capture-pane -t <session> -p -S -<capture_lines>` 抓 pane 末尾文本。
2. 选 profile:先按 `session_match` 正则匹配会话名;认不出再用 `detect` 正则嗅探 pane 内容。都不中跳过该会话。
3. 按优先级匹配状态:**working 先于 waiting 先于 idle**。profile 里每个状态是一组正则(任一命中即该状态)。都不中 = `unknown`。
4. 注意 codex 和 claude 的 WORKING 都含 `esc to interrupt`,但 codex idle 有 `›` 输入框——靠 working 规则优先级 + idle 的 `(?m)^›\s*$` 区分。

## 转移播报规则(看 [transitions] 配置开关)
- working/waiting/unknown → idle:`✅ <session> 干完了,空闲待命`(+ 末尾一句上下文)
- 任意 → waiting:`⏸ <session> 卡住了,在等你拍板`(+ 菜单/提示原文)
- idle/waiting → working:默认**不报**(刚发指令开始干很正常,免噪音)
- 会话消失:`⚫ <session> 会话已结束`
- 新会话首次见到且已是 waiting:报一次
- 无转移 → 空输出(once 模式 stdout 空 = cron 静默)

## 上下文提取(让播报有信息量)
- waiting:抓最后一个非空的 `❯`/`›`/含问号的行(菜单问题)。
- idle:抓最近的 `●`/`✻`/`✔` 总结行(去掉前缀符号)。
- 截断到 ~120 字符。

## 文件结构(建议)
```
src/
  main.rs        # clap CLI:once / daemon / check(打印当前所有会话状态,调试用)
  config.rs      # TOML 配置加载 + Profile 结构 + ~ 展开
  classify.rs    # 状态分类:Profile 匹配 + 正则分类 + 上下文提取
  tmux.rs        # tmux 交互:list-sessions / capture-pane(用 std::process 或 tokio)
  state.rs       # 状态文件读写(JSON: session -> last_state)+ 转移检测
  notify.rs      # 投递:stdout 格式化 + Telegram Bot API(reqwest)
  watch.rs       # 核心循环:扫描→分类→diff→产出事件(once 和 daemon 共用)
tests/
  classify_test.rs  # 用真实 pane 文本样本测分类(见下方样本)
```

## 真实 pane 样本(写进测试,确保分类对)
### Claude Code WORKING
```
  ⏵⏵ bypass permissions on (shift+tab to cycle) · esc to interrupt · ctrl+t to hide tasks
```
### Claude Code IDLE
```
❯
────────────────────────────────────────
  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents
                                        new task? /clear to save 330.5k tokens
```
### Claude Code WAITING
```
❯ 1. 按表 B-0 → B → C 全程自主推进(推荐)
  2. 只先做 B-0 皮肤层
Enter to select · Tab/Arrow keys to navigate · Esc to cancel
```
### Codex WORKING
```
› what is 2+2? answer in one word
• Working (3s • esc to interrupt)
  gpt-5.5 xhigh · ~/campus-auto-backup/campus-auto
```
### Codex IDLE
```
› Use /skills to list available skills
  gpt-5.5 xhigh · ~/campus-auto-backup/campus-auto
```
### Codex WAITING
```
› 1. Review hooks
  2. Trust all and continue
  3. Continue without trusting (hooks won't run)
  Press enter to confirm or esc to go back
```
### Gemini WAITING (auth menu — 唯一抓到的真实态)
```
  ● 1. Sign in with Google
    2. Use Gemini API Key
  (Use Enter to select)
```

## 验收标准
1. `cargo build --release` 通过,产出单二进制。
2. `cargo test` 通过,classify 测试覆盖上面 7 个真实样本(每个分类正确)。
3. `ccwatch check` 能列出当前 tmux 所有 agent 会话 + 识别到的状态(调试用)。
4. `ccwatch once` 跑两遍:首遍建基线无输出,二遍无变化时 stdout 为空。
5. `ccwatch daemon --help` 和 Telegram 投递代码存在(可不实测真发,但要能编译、逻辑完整)。
6. README 写清三种模式用法 + 如何加新 agent profile + 如何挂 Hermes cron。

## 约束
- 纯 Rust,单二进制,release 用 opt-level="z"+lto 压到最小。
- 正则特征值用 config.example.toml 里我实测的,别瞎改。
- 代码注释中文 OK(用户习惯)。commit 按 feat/fix/chore 拆分,中文 message。
- 不要 push(没建 GitHub 仓,本地 commit 即可)。
