//! 分类测试:用 DESIGN.md 里 7 个真实 pane 样本验证每个分类正确。
//!
//! 直接加载仓库根目录的 config.example.toml(用实测特征值),
//! 确保配置里的正则就是分类用的正则——配置改了测试会跟着抓问题。

use ccwatch::classify::{Classifier, State};
use ccwatch::config::Config;
use std::path::PathBuf;

/// 加载仓库根的 config.example.toml 构造分类器。
fn classifier() -> Classifier {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config.example.toml");
    let cfg = Config::load(&path).expect("加载 config.example.toml");
    Classifier::from_config(&cfg).expect("编译分类器")
}

/// 断言某会话名 + pane 文本的分类结果。
fn assert_state(session: &str, pane: &str, expect_profile: &str, expect_state: State) {
    let c = classifier();
    let res = c
        .classify(session, pane)
        .unwrap_or_else(|| panic!("会话 {} 没匹配到任何 profile", session));
    assert_eq!(
        res.profile, expect_profile,
        "会话 {} 应匹配 profile {},实际 {}",
        session, expect_profile, res.profile
    );
    assert_eq!(
        res.state, expect_state,
        "会话 {} 应为 {:?},实际 {:?}(profile={})",
        session, expect_state, res.state, res.profile
    );
}

// ---- Claude Code ----

#[test]
fn claude_working() {
    let pane = "  ⏵⏵ bypass permissions on (shift+tab to cycle) · esc to interrupt · ctrl+t to hide tasks";
    assert_state("ccA", pane, "claude", State::Working);
}

#[test]
fn claude_idle() {
    let pane = "❯
────────────────────────────────────────
  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents
                                        new task? /clear to save 330.5k tokens";
    assert_state("ccB", pane, "claude", State::Idle);
}

#[test]
fn claude_waiting() {
    let pane = "❯ 1. 按表 B-0 → B → C 全程自主推进(推荐)
  2. 只先做 B-0 皮肤层
Enter to select · Tab/Arrow keys to navigate · Esc to cancel";
    assert_state("ccC", pane, "claude", State::Waiting);
}

// ---- Codex ----

#[test]
fn codex_working() {
    // 含 "esc to interrupt" + "Working (3s",但优先级保证判为 working。
    let pane = "› what is 2+2? answer in one word
• Working (3s • esc to interrupt)
  gpt-5.5 xhigh · ~/campus-auto-backup/campus-auto";
    assert_state("codex1", pane, "codex", State::Working);
}

#[test]
fn codex_idle() {
    // 有 › 输入框 + 底部 · ~/ 状态行,无 Working → idle。
    let pane = "› Use /skills to list available skills
  gpt-5.5 xhigh · ~/campus-auto-backup/campus-auto";
    assert_state("codex2", pane, "codex", State::Idle);
}

#[test]
fn codex_waiting() {
    let pane = "› 1. Review hooks
  2. Trust all and continue
  3. Continue without trusting (hooks won't run)
  Press enter to confirm or esc to go back";
    assert_state("codex3", pane, "codex", State::Waiting);
}

// ---- Gemini ----

#[test]
fn gemini_waiting() {
    let pane = "  ● 1. Sign in with Google
    2. Use Gemini API Key
  (Use Enter to select)";
    assert_state("gemini1", pane, "gemini", State::Waiting);
}

// ---- 额外:codex working 不能被误判成 idle ----
// codex working 的 pane 同时含 › 输入框行(顶部历史),靠 working 优先级排除。

#[test]
fn codex_working_not_idle_despite_prompt() {
    let pane = "› earlier question
• Working (12s • esc to interrupt)
  gpt-5.5 xhigh · ~/some/path";
    let c = classifier();
    let res = c.classify("codex9", pane).unwrap();
    assert_ne!(res.state, State::Idle, "codex working 不该被判为 idle");
    assert_eq!(res.state, State::Working);
}
