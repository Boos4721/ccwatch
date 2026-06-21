//! 状态分类:Profile 选择 + 正则分类 + 上下文提取。
//!
//! 优先级:working > waiting > idle。每个状态是一组正则,任一命中即该状态。
//! codex/claude 的 WORKING 都含 "esc to interrupt",靠优先级 + codex idle 的
//! `(?m)^›\s*$` 输入框区分。

use crate::config::{Config, Profile};
use regex::Regex;
use std::fmt;

/// 会话状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Working,
    Waiting,
    Idle,
    /// 选中了 profile 但三类规则都不中。
    Unknown,
}

impl State {
    pub fn as_str(&self) -> &'static str {
        match self {
            State::Working => "working",
            State::Waiting => "waiting",
            State::Idle => "idle",
            State::Unknown => "unknown",
        }
    }

    /// 从状态文件里的字符串还原。
    pub fn from_str(s: &str) -> State {
        match s {
            "working" => State::Working,
            "waiting" => State::Waiting,
            "idle" => State::Idle,
            _ => State::Unknown,
        }
    }
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// waiting 的子类型:在等什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitKind {
    /// 等 y/n 审批(如 bypass permissions 提示、Do you want)。
    Approval,
    /// 等用户输入文本。
    Input,
    /// 等选择菜单 / 方向键选项。
    Menu,
}

impl WaitKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            WaitKind::Approval => "approval",
            WaitKind::Input => "input",
            WaitKind::Menu => "menu",
        }
    }

    /// 中文播报标签。
    pub fn label_zh(&self) -> &'static str {
        match self {
            WaitKind::Approval => "等审批",
            WaitKind::Input => "等输入",
            WaitKind::Menu => "等选择",
        }
    }

    pub fn from_str(s: &str) -> Option<WaitKind> {
        match s {
            "approval" => Some(WaitKind::Approval),
            "input" => Some(WaitKind::Input),
            "menu" => Some(WaitKind::Menu),
            _ => None,
        }
    }
}

/// 一次分类的结果。
#[derive(Debug, Clone)]
pub struct Classification {
    /// 命中的 profile 名。
    pub profile: String,
    pub state: State,
    /// 给播报用的一句上下文(可能为空)。
    pub context: Option<String>,
    /// waiting 时的子类型(非 waiting 为 None)。
    pub wait_kind: Option<WaitKind>,
}

/// 编译好的单个 profile(正则已编译)。
struct CompiledProfile {
    name: String,
    session_match: Option<Regex>,
    detect: Option<Regex>,
    working: Vec<Regex>,
    waiting: Vec<Regex>,
    idle: Vec<Regex>,
    /// waiting 子类型正则(可选;命中即细分,否则子类型为 None)。
    waiting_approval: Vec<Regex>,
    waiting_input: Vec<Regex>,
    waiting_menu: Vec<Regex>,
}

/// 编译好的分类器,持有所有 profile。
pub struct Classifier {
    profiles: Vec<CompiledProfile>,
}

/// 编译一组正则,逐条带上下文报错。
fn compile_rules(rules: &[String], who: &str, kind: &str) -> anyhow::Result<Vec<Regex>> {
    rules
        .iter()
        .map(|r| {
            Regex::new(r).map_err(|e| {
                anyhow::anyhow!("profile [{}] 的 {} 正则 `{}` 编译失败: {}", who, kind, r, e)
            })
        })
        .collect()
}

fn compile_opt(pat: &Option<String>, who: &str, kind: &str) -> anyhow::Result<Option<Regex>> {
    match pat {
        Some(p) => Ok(Some(Regex::new(p).map_err(|e| {
            anyhow::anyhow!("profile [{}] 的 {} 正则 `{}` 编译失败: {}", who, kind, p, e)
        })?)),
        None => Ok(None),
    }
}

impl Classifier {
    /// 从配置编译出分类器。
    pub fn from_config(cfg: &Config) -> anyhow::Result<Classifier> {
        let mut profiles = Vec::with_capacity(cfg.profiles.len());
        for p in &cfg.profiles {
            profiles.push(Self::compile(p)?);
        }
        Ok(Classifier { profiles })
    }

    fn compile(p: &Profile) -> anyhow::Result<CompiledProfile> {
        Ok(CompiledProfile {
            name: p.name.clone(),
            session_match: compile_opt(&p.session_match, &p.name, "session_match")?,
            detect: compile_opt(&p.detect, &p.name, "detect")?,
            working: compile_rules(&p.working, &p.name, "working")?,
            waiting: compile_rules(&p.waiting, &p.name, "waiting")?,
            idle: compile_rules(&p.idle, &p.name, "idle")?,
            waiting_approval: compile_rules(&p.waiting_approval, &p.name, "waiting_approval")?,
            waiting_input: compile_rules(&p.waiting_input, &p.name, "waiting_input")?,
            waiting_menu: compile_rules(&p.waiting_menu, &p.name, "waiting_menu")?,
        })
    }

    /// 给定会话名 + pane 文本,选 profile 并分类。认不出任何 profile 返回 None。
    pub fn classify(&self, session: &str, pane: &str) -> Option<Classification> {
        let profile = self.select_profile(session, pane)?;
        let state = profile.classify_state(pane);
        let context = extract_context(state, pane);
        let wait_kind = if state == State::Waiting {
            profile.classify_wait_kind(pane)
        } else {
            None
        };
        Some(Classification {
            profile: profile.name.clone(),
            state,
            context,
            wait_kind,
        })
    }

    /// 选 profile:先按 session_match 匹配会话名,认不出再用 detect 嗅探 pane。
    fn select_profile(&self, session: &str, pane: &str) -> Option<&CompiledProfile> {
        // 第一轮:会话名匹配(更可靠)。
        for p in &self.profiles {
            if let Some(re) = &p.session_match {
                if re.is_match(session) {
                    return Some(p);
                }
            }
        }
        // 第二轮:pane 内容嗅探。
        for p in &self.profiles {
            if let Some(re) = &p.detect {
                if re.is_match(pane) {
                    return Some(p);
                }
            }
        }
        None
    }
}

impl CompiledProfile {
    /// 按优先级分类:working > waiting > idle。
    fn classify_state(&self, pane: &str) -> State {
        if self.working.iter().any(|re| re.is_match(pane)) {
            return State::Working;
        }
        if self.waiting.iter().any(|re| re.is_match(pane)) {
            return State::Waiting;
        }
        if self.idle.iter().any(|re| re.is_match(pane)) {
            return State::Idle;
        }
        State::Unknown
    }

    /// waiting 子类型细分:approval > menu > input(命中靠前者优先)。
    /// 三组都没配/都不中时返回 None(只知道在等,不知等什么)。
    fn classify_wait_kind(&self, pane: &str) -> Option<WaitKind> {
        if self.waiting_approval.iter().any(|re| re.is_match(pane)) {
            return Some(WaitKind::Approval);
        }
        if self.waiting_menu.iter().any(|re| re.is_match(pane)) {
            return Some(WaitKind::Menu);
        }
        if self.waiting_input.iter().any(|re| re.is_match(pane)) {
            return Some(WaitKind::Input);
        }
        None
    }
}

/// 按字符截断(不是字节),保留中文。
fn truncate_chars(s: &str, max: usize) -> String {
    let trimmed = s.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }
    let cut: String = trimmed.chars().take(max).collect();
    format!("{}…", cut)
}

/// 提取播报上下文。
/// - waiting:最后一个含 `❯`/`›`/问号的菜单/提问行。
/// - idle:最近的 `●`/`✻`/`✔` 总结行(去前缀符号)。
fn extract_context(state: State, pane: &str) -> Option<String> {
    match state {
        State::Waiting => extract_waiting_context(pane),
        State::Idle => extract_idle_context(pane),
        _ => None,
    }
}

/// waiting:从下往上找最后一个非空的提示/提问行。
fn extract_waiting_context(pane: &str) -> Option<String> {
    for line in pane.lines().rev() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let is_prompt = t.starts_with('❯')
            || t.starts_with('›')
            || t.starts_with('>')
            || t.contains('?')
            || t.contains('？');
        if is_prompt {
            let cleaned = t.trim_start_matches(['❯', '›', '>', '●', ' ']).trim();
            if !cleaned.is_empty() {
                return Some(truncate_chars(cleaned, 120));
            }
        }
    }
    None
}

/// idle:从下往上找最近的总结行(`●`/`✻`/`✔`),去掉前缀符号。
fn extract_idle_context(pane: &str) -> Option<String> {
    for line in pane.lines().rev() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let has_marker = t.starts_with('●')
            || t.starts_with('✻')
            || t.starts_with('✔')
            || t.starts_with('✓');
        if has_marker {
            let cleaned = t
                .trim_start_matches(['●', '✻', '✔', '✓', ' '])
                .trim();
            if !cleaned.is_empty() {
                return Some(truncate_chars(cleaned, 120));
            }
        }
    }
    None
}
