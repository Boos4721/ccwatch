//! ccwatch — 通用 AI agent 会话监控器。
//!
//! 三种模式:
//!   once   跑一遍,把转移事件打印到 stdout(给 cron 用,空输出=静默)。
//!   daemon 常驻循环,按 poll_interval_secs 自投递(stdout 或 Telegram)。
//!   check  列出当前所有匹配会话 + 识别到的状态(调试)。

use anyhow::{Context, Result};
use ccwatch::{classify, config, notify, state, watch};
use clap::{Parser, Subcommand};
use config::{expand_tilde, Config};
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
    Once,
    /// 常驻循环,按间隔轮询并自投递。
    Daemon {
        /// 覆盖配置里的轮询间隔(秒)。
        #[arg(long)]
        interval: Option<u64>,
    },
    /// 列出当前所有匹配会话 + 状态(调试)。
    Check,
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
        Commands::Once => run_once(cli.config).await,
        Commands::Daemon { interval } => run_daemon(cli.config, interval).await,
        Commands::Check => run_check(cli.config),
    }
}

/// once:扫描一遍,投递事件,写回状态。
async fn run_once(cli_path: Option<PathBuf>) -> Result<()> {
    let (cfg, classifier) = load(cli_path)?;
    let state_path = cfg.state_file_path();
    let prev = state::StateStore::load(&state_path)?;

    let (events, new_store) = watch::scan_once(&cfg, &classifier, &prev)?;

    let notifier = notify::Notifier::from_delivery(&cfg.delivery)?;
    notifier.deliver(&events).await?;

    new_store.save(&state_path)?;
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
