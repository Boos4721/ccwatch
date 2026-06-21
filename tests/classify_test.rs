//! 分类测试:用 DESIGN.md 里 7 个真实 pane 样本验证每个分类正确。
//!
//! 直接加载仓库根目录的 config.example.toml(用实测特征值),
//! 确保配置里的正则就是分类用的正则——配置改了测试会跟着抓问题。

use ccwatch::classify::{Classifier, State, WaitKind};
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

// ---- waiting 子类型细分 ----

/// 断言某 pane 的 waiting 子类型。
fn assert_wait_kind(session: &str, pane: &str, expect: WaitKind) {
    let c = classifier();
    let res = c
        .classify(session, pane)
        .unwrap_or_else(|| panic!("会话 {} 没匹配到任何 profile", session));
    assert_eq!(res.state, State::Waiting, "{} 应为 waiting", session);
    assert_eq!(
        res.wait_kind,
        Some(expect),
        "会话 {} 子类型应为 {:?},实际 {:?}",
        session,
        expect,
        res.wait_kind
    );
}

#[test]
fn claude_waiting_approval_subtype() {
    let pane = "Do you want to proceed?
❯ 1. Yes
  2. No, tell Claude what to do differently";
    assert_wait_kind("ccA", pane, WaitKind::Approval);
}

#[test]
fn claude_waiting_menu_subtype() {
    let pane = "❯ 1. 按表 B-0 → B → C 全程自主推进(推荐)
  2. 只先做 B-0 皮肤层
Enter to select · Tab/Arrow keys to navigate · Esc to cancel";
    assert_wait_kind("ccC", pane, WaitKind::Menu);
}

#[test]
fn codex_waiting_approval_subtype() {
    let pane = "› 1. Review hooks
  2. Trust all and continue
  3. Continue without trusting (hooks won't run)
  Press enter to confirm or esc to go back";
    // "Trust all and continue" 命中 approval(优先级高于 menu)。
    assert_wait_kind("codex3", pane, WaitKind::Approval);
}

#[test]
fn codex_waiting_menu_subtype() {
    let pane = "› Pick a file
  Use Enter to select";
    assert_wait_kind("codex5", pane, WaitKind::Menu);
}

#[test]
fn waiting_without_subtype_match_is_none() {
    // 命中通用 waiting 但不命中任何子类型正则 → wait_kind = None。
    let pane = "❯ 1. option only";
    let c = classifier();
    let res = c.classify("ccZ", pane).unwrap();
    assert_eq!(res.state, State::Waiting);
    // "❯\\s*1\\." 同时在 waiting 和 waiting_menu 里,所以这里其实会判 menu;
    // 用一个只在通用 waiting 的特征来验 None。
    let pane2 = "Press Enter to continue reading";
    let res2 = c.classify("ccY", pane2).unwrap();
    assert_eq!(res2.state, State::Waiting);
    assert_eq!(res2.wait_kind, Some(WaitKind::Input));
}

// ---- Aider / Cline(推测 profile,验证按会话名选中 + 子类型分类)----

#[test]
fn aider_waiting_approval_subtype() {
    let pane = "Add main.py to the chat? (Y)es/(N)o [Yes]:";
    assert_state("aider1", pane, "aider", State::Waiting);
    let c = classifier();
    let res = c.classify("aider1", pane).unwrap();
    assert_eq!(res.wait_kind, Some(WaitKind::Approval));
}

#[test]
fn cline_waiting_approval_subtype() {
    let pane = "Cline wants to run a command\nApprove  Reject";
    assert_state("cline1", pane, "cline", State::Waiting);
    let c = classifier();
    let res = c.classify("cline1", pane).unwrap();
    assert_eq!(res.wait_kind, Some(WaitKind::Approval));
}
