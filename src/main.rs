//! ccwatch — 通用 AI agent 会话监控器。
//!
//! 三种模式:
//!   once   跑一遍,把转移事件打印到 stdout(给 cron 用,空输出=静默)。
//!   daemon 常驻循环,按 poll_interval_secs 自投递(stdout 或 Telegram)。
//!   check  列出当前所有匹配会话 + 识别到的状态(调试)。

use anyhow::{Context, Result};
use ccwatch::{acp, classify, config, notify, state, watch};
use clap::{Parser, Subcommand};
use config::{expand_tilde, Config, EffectiveMode, Mode};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "ccwatch",
    version,
    about = "通用 AI agent 会话监控器:监控 tmux 里的 Claude Code / Codex / Gemini 会话,只在状态转移时播报。"
)]
struct Cli {
    /// 配置文件路径(默认 ~/.config/ccwatch/config.toml,回退到 ./config.example.toml)。
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// 跑一遍,转移事件打印到 stdout(给 cron;无转移则静默)。
    Once {
        /// 运行轨道:screen(抓屏)/ protocol(协议)/ auto(优先协议,回退抓屏)。
        #[arg(long, default_value = "screen")]
        mode: String,
        /// 协议模式目标 agent(当前支持:codex)。
        #[arg(long, default_value = "codex")]
        agent: String,
        /// 协议模式:发给 agent 的 prompt。
        #[arg(long, default_value = "what is 2+2? reply with one word")]
        prompt: String,
        /// 协议模式:会话标签(播报里的 session 名;默认用 agent 名)。
        #[arg(long)]
        label: Option<String>,
        /// 协议模式:整体超时(秒)。
        #[arg(long, default_value_t = 90)]
        timeout: u64,
        /// 协议模式:codex 审批策略。
        #[arg(long, default_value = "never")]
        approval_policy: String,
        /// 协议模式:codex sandbox。
        #[arg(long, default_value = "read-only")]
        sandbox: String,
    },
    /// 常驻循环,按间隔轮询并自投递。
    Daemon {
        /// 覆盖配置里的轮询间隔(秒)。
        #[arg(long)]
        interval: Option<u64>,
    },
    /// 列出当前所有匹配会话 + 状态(调试)。
    Check,
    /// 协议模式探针:以 ACP/MCP client 身份拉起 agent,跑一个 turn,
    /// 实时打印从协议事件流读到的权威状态(验证用)。
    AcpProbe {
        /// 目标 agent(当前支持:codex)。
        #[arg(long, default_value = "codex")]
        agent: String,
        /// 发给 agent 的 prompt。
        #[arg(long, default_value = "what is 2+2? reply with one word")]
        prompt: String,
        /// 整体超时(秒)。
        #[arg(long, default_value_t = 90)]
        timeout: u64,
        /// codex 审批策略(untrusted/on-failure/on-request/never)。
        #[arg(long, default_value = "never")]
        approval_policy: String,
        /// codex sandbox(read-only/workspace-write/danger-full-access)。
        #[arg(long, default_value = "read-only")]
        sandbox: String,
    },
}

/// 找配置文件:命令行指定 > ~/.config/ccwatch/config.toml > ./config.example.toml。
fn resolve_config_path(cli_path: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = cli_path {
        return Ok(p);
    }
    let default = expand_tilde("~/.config/ccwatch/config.toml");
    if default.exists() {
        return Ok(default);
    }
    let example = PathBuf::from("config.example.toml");
    if example.exists() {
        return Ok(example);
    }
    anyhow::bail!(
        "找不到配置:既无 {} 也无 ./config.example.toml,请用 --config 指定。",
        default.display()
    );
}

fn load(cli_path: Option<PathBuf>) -> Result<(Config, classify::Classifier)> {
    let path = resolve_config_path(cli_path)?;
    let cfg = Config::load(&path)?;
    let classifier = classify::Classifier::from_config(&cfg)
        .context("从配置编译分类器失败(正则有误?)")?;
    Ok((cfg, classifier))
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Once {
            mode,
            agent,
            prompt,
            label,
            timeout,
            approval_policy,
            sandbox,
        } => {
            run_once(
                cli.config,
                &mode,
                &agent,
                &prompt,
                label.as_deref(),
                timeout,
                &approval_policy,
                &sandbox,
            )
            .await
        }
        Commands::Daemon { interval } => run_daemon(cli.config, interval).await,
        Commands::Check => run_check(cli.config),
        Commands::AcpProbe {
            agent,
            prompt,
            timeout,
            approval_policy,
            sandbox,
        } => run_acp_probe(&agent, &prompt, timeout, &approval_policy, &sandbox).await,
    }
}

/// once:按 --mode 选轨道,跑一遍,投递转移事件,写回状态。
/// 两条轨道共用同一套转移规则(state.rs)、Event 类型、Notifier 投递路径。
#[allow(clippy::too_many_arguments)]
async fn run_once(
    cli_path: Option<PathBuf>,
    mode_str: &str,
    agent: &str,
    prompt: &str,
    label: Option<&str>,
    timeout_secs: u64,
    approval_policy: &str,
    sandbox: &str,
) -> Result<()> {
    let mode = Mode::parse(mode_str)?;
    // auto 择优:协议可用(codex 在 PATH)就走协议,否则抓屏。
    let protocol_available = agent == "codex" && acp::codex_available();
    let effective = mode.resolve(protocol_available);

    // 显式 --mode protocol 但协议不可用:直接报错,不偷偷回退。
    if mode == Mode::Protocol && effective != EffectiveMode::Protocol {
        anyhow::bail!(
            "--mode protocol 但协议不可用(agent={agent};codex 在 PATH 上?当前仅支持 codex)"
        );
    }

    match effective {
        EffectiveMode::Screen => run_once_screen(cli_path).await,
        EffectiveMode::Protocol => {
            run_once_protocol(
                cli_path,
                agent,
                prompt,
                label.unwrap_or(agent),
                timeout_secs,
                approval_policy,
                sandbox,
            )
            .await
        }
    }
}

/// 抓屏轨道:扫描一遍,投递事件,写回状态。(原行为,保持不变。)
async fn run_once_screen(cli_path: Option<PathBuf>) -> Result<()> {
    let (cfg, classifier) = load(cli_path)?;
    let state_path = cfg.state_file_path();
    let prev = state::StateStore::load(&state_path)?;

    let (events, new_store) = watch::scan_once(&cfg, &classifier, &prev)?;

    let notifier = notify::Notifier::from_delivery(&cfg.delivery)?;
    notifier.deliver(&events).await?;

    new_store.save(&state_path)?;
    Ok(())
}

/// 协议轨道:拉起 agent 跑一个 turn,把权威状态流经**同一套** TransitionTracker
/// (复用 detect_transition 规则)→ Event → Notifier 投递,并写回同一个状态文件。
#[allow(clippy::too_many_arguments)]
async fn run_once_protocol(
    cli_path: Option<PathBuf>,
    agent: &str,
    prompt: &str,
    label: &str,
    timeout_secs: u64,
    approval_policy: &str,
    sandbox: &str,
) -> Result<()> {
    if agent != "codex" {
        anyhow::bail!(
            "协议轨道当前只实现了 codex;gemini/claude 见 docs/ACP_RESEARCH.md,后续接入。"
        );
    }
    let cfg = {
        let path = resolve_config_path(cli_path)?;
        Config::load(&path)?
    };
    let state_path = cfg.state_file_path();
    let prev = state::StateStore::load(&state_path)?;
    // 用磁盘状态初始化追踪器:协议轨道也能跨进程接上上次状态。
    let mut tracker = state::TransitionTracker::from_store(cfg.transitions.clone(), &prev);
    let notifier = notify::Notifier::from_delivery(&cfg.delivery)?;

    let mut client = acp::CodexClient::spawn(None).context("拉起 codex mcp-server 失败")?;

    // run_turn 的回调是同步的;在回调里只收集事件,turn 结束后统一异步投递。
    let mut events: Vec<state::Event> = Vec::new();
    client
        .run_turn(
            prompt,
            approval_policy,
            sandbox,
            Duration::from_secs(timeout_secs),
            |ev| {
                if let Some(sig) = ev.signal {
                    if let Some(event) = tracker.observe(label, sig.state, sig.context) {
                        events.push(event);
                    }
                }
            },
        )
        .await?;
    client.shutdown().await;

    notifier.deliver(&events).await?;
    tracker.to_store().save(&state_path)?;
    Ok(())
}

/// daemon:常驻循环。
async fn run_daemon(cli_path: Option<PathBuf>, interval_override: Option<u64>) -> Result<()> {
    let (cfg, classifier) = load(cli_path)?;
    let state_path = cfg.state_file_path();
    let interval = interval_override.unwrap_or(cfg.general.poll_interval_secs);
    let notifier = notify::Notifier::from_delivery(&cfg.delivery)?;

    tracing::info!(
        "ccwatch daemon 启动:轮询间隔 {}s,投递={},状态文件={}",
        interval,
        cfg.delivery.mode,
        state_path.display()
    );

    let mut ticker = tokio::time::interval(Duration::from_secs(interval.max(1)));
    loop {
        ticker.tick().await;
        // 每轮重新读状态文件(允许外部 once 也在更新)。
        let prev = match state::StateStore::load(&state_path) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("读状态文件失败,本轮当空: {}", e);
                state::StateStore::default()
            }
        };
        match watch::scan_once(&cfg, &classifier, &prev) {
            Ok((events, new_store)) => {
                if let Err(e) = notifier.deliver(&events).await {
                    tracing::warn!("投递失败,本轮不写状态(下轮重试): {}", e);
                    continue;
                }
                if let Err(e) = new_store.save(&state_path) {
                    tracing::warn!("写状态文件失败: {}", e);
                }
            }
            Err(e) => tracing::warn!("扫描失败: {}", e),
        }
    }
}

/// check:列出当前所有匹配会话 + 状态。
fn run_check(cli_path: Option<PathBuf>) -> Result<()> {
    let (cfg, classifier) = load(cli_path)?;
    let snaps = watch::scan_snapshots(&cfg, &classifier)?;

    if snaps.is_empty() {
        println!("(没有匹配的 agent 会话)");
        return Ok(());
    }

    println!("{:<16} {:<8} {:<8} {}", "SESSION", "PROFILE", "STATE", "CONTEXT");
    for s in &snaps {
        let ctx = s.context.as_deref().unwrap_or("");
        println!(
            "{:<16} {:<8} {:<8} {}",
            s.session,
            s.profile,
            s.state.as_str(),
            ctx
        );
    }
    Ok(())
}

/// acp-probe:协议模式验证。拉起 agent,跑一个 turn,实时打印权威状态流。
async fn run_acp_probe(
    agent: &str,
    prompt: &str,
    timeout_secs: u64,
    approval_policy: &str,
    sandbox: &str,
) -> Result<()> {
    if agent != "codex" {
        anyhow::bail!(
            "acp-probe 当前只实现了 codex(纯 Rust 直连 `codex mcp-server`);\
             gemini(--acp)/claude(stream-json)见 docs/ACP_RESEARCH.md,后续接入。"
        );
    }

    println!("== acp-probe agent=codex approval={approval_policy} sandbox={sandbox} ==");
    println!("prompt: {prompt}\n");

    let mut client = acp::CodexClient::spawn(None).context("拉起 codex mcp-server 失败")?;
    let start = std::time::Instant::now();

    client
        .run_turn(
            prompt,
            approval_policy,
            sandbox,
            Duration::from_secs(timeout_secs),
            |ev| {
                let t = start.elapsed().as_secs_f64();
                match ev.signal {
                    Some(sig) => {
                        let ctx = sig
                            .context
                            .as_deref()
                            .map(|c| format!("  | {c}"))
                            .unwrap_or_default();
                        // 权威状态转移:醒目打印。
                        println!("[{t:6.2}s] {:<8} <- {}{}", sig.state.as_str(), ev.raw_kind, ctx);
                    }
                    None => {
                        // 中间事件:低调打印,看得到流动即可。
                        println!("[{t:6.2}s] ·        ({})", ev.raw_kind);
                    }
                }
            },
        )
        .await?;

    client.shutdown().await;
    println!("\n== turn 结束(状态流来自协议事件,非抓屏猜测)==");
    Ok(())
}
