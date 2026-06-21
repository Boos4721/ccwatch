//! 配置加载:TOML 反序列化 + Profile 结构 + `~` 展开。
//!
//! 配置完全驱动行为:加新 agent 只需加一段 `[[profiles]]`,不改代码。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 顶层配置。
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub general: General,
    #[serde(default)]
    pub delivery: Delivery,
    #[serde(default)]
    pub transitions: Transitions,
    #[serde(default)]
    pub profiles: Vec<Profile>,
}

/// 全局设置。
#[derive(Debug, Clone, Deserialize)]
pub struct General {
    /// 监控哪些 tmux 会话:按会话名前缀过滤(空 = 全部)。
    #[serde(default)]
    pub session_prefixes: Vec<String>,
    /// 抓 pane 末尾多少行做分类。
    #[serde(default = "default_capture_lines")]
    pub capture_lines: u32,
    /// daemon 轮询间隔(秒)。
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    /// 状态文件路径(支持 `~` 展开)。
    #[serde(default = "default_state_file")]
    pub state_file: String,
}

impl Default for General {
    fn default() -> Self {
        General {
            session_prefixes: Vec::new(),
            capture_lines: default_capture_lines(),
            poll_interval_secs: default_poll_interval(),
            state_file: default_state_file(),
        }
    }
}

fn default_capture_lines() -> u32 {
    60
}
fn default_poll_interval() -> u64 {
    30
}
fn default_state_file() -> String {
    "~/.config/ccwatch/state.json".to_string()
}

/// 投递方式。
#[derive(Debug, Clone, Deserialize)]
pub struct Delivery {
    /// none = 只打印 stdout;telegram = 直连 Bot API。
    #[serde(default = "default_delivery_mode")]
    pub mode: String,
    pub bot_token: Option<String>,
    pub chat_id: Option<String>,
    pub proxy: Option<String>,
}

impl Default for Delivery {
    fn default() -> Self {
        Delivery {
            mode: default_delivery_mode(),
            bot_token: None,
            chat_id: None,
            proxy: None,
        }
    }
}

fn default_delivery_mode() -> String {
    "none".to_string()
}

/// 播报规则开关。
#[derive(Debug, Clone, Deserialize)]
pub struct Transitions {
    /// 干完了(working/waiting/unknown → idle)。
    #[serde(default = "default_true")]
    pub notify_done: bool,
    /// 卡住了(任意 → waiting)。
    #[serde(default = "default_true")]
    pub notify_waiting: bool,
    /// 开始干(idle/waiting → working)。默认不报。
    #[serde(default = "default_false")]
    pub notify_working: bool,
    /// 会话消失。
    #[serde(default = "default_true")]
    pub notify_gone: bool,
    /// 新会话首次见到且已是 waiting。
    #[serde(default = "default_true")]
    pub notify_new_waiting: bool,
}

impl Default for Transitions {
    fn default() -> Self {
        Transitions {
            notify_done: true,
            notify_waiting: true,
            notify_working: false,
            notify_gone: true,
            notify_new_waiting: true,
        }
    }
}

fn default_true() -> bool {
    true
}
fn default_false() -> bool {
    false
}

/// 单个 agent 的 pane 特征。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Profile {
    /// profile 名(claude / codex / gemini / ...)。
    pub name: String,
    /// 会话名匹配(正则,可选)。
    #[serde(default)]
    pub session_match: Option<String>,
    /// pane 内容嗅探(会话名认不出时确认 agent 身份)。
    #[serde(default)]
    pub detect: Option<String>,
    /// WORKING 规则(任一命中即该状态)。
    #[serde(default)]
    pub working: Vec<String>,
    /// WAITING 规则。
    #[serde(default)]
    pub waiting: Vec<String>,
    /// IDLE 规则。
    #[serde(default)]
    pub idle: Vec<String>,
}

impl Config {
    /// 从指定路径加载配置。
    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("读取配置文件失败: {}", path.display()))?;
        let cfg: Config =
            toml::from_str(&text).with_context(|| format!("解析配置文件失败: {}", path.display()))?;
        Ok(cfg)
    }

    /// 状态文件路径(已做 `~` 展开)。
    pub fn state_file_path(&self) -> PathBuf {
        expand_tilde(&self.general.state_file)
    }
}

/// 把开头的 `~` 展开成 $HOME。
pub fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    if path == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home);
        }
    }
    PathBuf::from(path)
}

/// 运行模式:抓屏 / 协议 / 自动择优。
///
/// 双轨并存的选择开关。`auto` 优先协议(权威),协议不可用时回退抓屏。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// 抓屏:tmux capture-pane + 正则(零侵入,监控已裸跑的会话)。
    Screen,
    /// 协议:以 ACP/MCP client 拉起 agent,读权威事件流。
    Protocol,
    /// 自动:协议可用走协议,否则回退抓屏。
    Auto,
}

/// `auto` 解析后实际落到的轨道(只会是 Screen 或 Protocol)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectiveMode {
    Screen,
    Protocol,
}

impl Mode {
    /// 从命令行字符串解析。
    pub fn parse(s: &str) -> Result<Mode> {
        match s.to_ascii_lowercase().as_str() {
            "screen" => Ok(Mode::Screen),
            "protocol" => Ok(Mode::Protocol),
            "auto" => Ok(Mode::Auto),
            other => anyhow::bail!("未知 --mode `{}`(可选:screen/protocol/auto)", other),
        }
    }

    /// 结合"协议是否可用"敲定实际轨道。
    /// - screen → 始终抓屏。
    /// - protocol → 始终协议(可用性由调用方另行校验并报错)。
    /// - auto → 协议可用则协议,否则抓屏。
    pub fn resolve(self, protocol_available: bool) -> EffectiveMode {
        match self {
            Mode::Screen => EffectiveMode::Screen,
            Mode::Protocol => EffectiveMode::Protocol,
            Mode::Auto => {
                if protocol_available {
                    EffectiveMode::Protocol
                } else {
                    EffectiveMode::Screen
                }
            }
        }
    }

    /// 协议轨道运行期出错时,是否应回退抓屏继续值守。
    /// 仅 auto 回退;显式 protocol 把错误抛给用户(别偷偷换轨)。
    pub fn fallback_on_error(self) -> bool {
        matches!(self, Mode::Auto)
    }
}

#[cfg(test)]
mod mode_tests {
    use super::*;

    #[test]
    fn parse_known_modes() {
        assert_eq!(Mode::parse("screen").unwrap(), Mode::Screen);
        assert_eq!(Mode::parse("protocol").unwrap(), Mode::Protocol);
        assert_eq!(Mode::parse("auto").unwrap(), Mode::Auto);
        // 大小写不敏感。
        assert_eq!(Mode::parse("AUTO").unwrap(), Mode::Auto);
    }

    #[test]
    fn parse_unknown_mode_errors() {
        assert!(Mode::parse("bogus").is_err());
    }

    #[test]
    fn screen_always_resolves_screen() {
        assert_eq!(Mode::Screen.resolve(true), EffectiveMode::Screen);
        assert_eq!(Mode::Screen.resolve(false), EffectiveMode::Screen);
    }

    #[test]
    fn protocol_always_resolves_protocol() {
        assert_eq!(Mode::Protocol.resolve(true), EffectiveMode::Protocol);
        // 可用性由调用方另行校验报错;resolve 本身只反映"想走协议"。
        assert_eq!(Mode::Protocol.resolve(false), EffectiveMode::Protocol);
    }

    #[test]
    fn auto_prefers_protocol_then_falls_back() {
        assert_eq!(Mode::Auto.resolve(true), EffectiveMode::Protocol);
        assert_eq!(Mode::Auto.resolve(false), EffectiveMode::Screen);
    }

    #[test]
    fn only_auto_falls_back_on_runtime_error() {
        // auto 出错回退抓屏;screen/protocol 不回退(protocol 把错抛给用户)。
        assert!(Mode::Auto.fallback_on_error());
        assert!(!Mode::Protocol.fallback_on_error());
        assert!(!Mode::Screen.fallback_on_error());
    }
}
