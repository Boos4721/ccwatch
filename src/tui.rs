//! TUI 总览面板:一屏实时表格(session / profile / state / 时长 / context)。
//!
//! 按 poll_interval 刷新,`q` / `Esc` / `Ctrl-C` 退出。状态用颜色区分:
//! working 黄 / waiting 红 / idle 绿 / stuck 闪红 / unknown 灰。

use crate::backend::Backend;
use crate::classify::{Classifier, State};
use crate::config::Config;
use crate::state::StateStore;
use crate::util::{fmt_dur, now_secs};
use crate::watch;
use anyhow::Result;
use crossterm::event::{self, Event as CEvent, KeyCode, KeyModifiers};
use crossterm::{execute, terminal};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Row, Table};
use ratatui::{Terminal};
use std::io::{self, Stdout};
use std::time::{Duration, Instant};

/// 一行表格数据。
struct Rowinfo {
    session: String,
    profile: String,
    state: State,
    /// 当前状态已持续秒数。
    dur_secs: u64,
    context: String,
    /// 是否疑似卡死(working 且长时间无输出)。
    stuck: bool,
}

/// 启动 TUI 主循环(阻塞,直到用户退出)。
pub fn run(cfg: &Config, classifier: &Classifier, backend: &dyn Backend) -> Result<()> {
    let mut term = setup()?;
    let res = run_loop(&mut term, cfg, classifier, backend);
    teardown(&mut term)?;
    res
}

fn setup() -> Result<Terminal<ratatui::backend::CrosstermBackend<Stdout>>> {
    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, terminal::EnterAlternateScreen)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    Ok(Terminal::new(backend)?)
}

fn teardown(term: &mut Terminal<ratatui::backend::CrosstermBackend<Stdout>>) -> Result<()> {
    terminal::disable_raw_mode()?;
    execute!(term.backend_mut(), terminal::LeaveAlternateScreen)?;
    term.show_cursor()?;
    Ok(())
}

fn run_loop(
    term: &mut Terminal<ratatui::backend::CrosstermBackend<Stdout>>,
    cfg: &Config,
    classifier: &Classifier,
    backend: &dyn Backend,
) -> Result<()> {
    let refresh = Duration::from_secs(cfg.general.poll_interval_secs.max(1));
    let mut rows = collect_rows(cfg, classifier, backend);
    let mut last = Instant::now();

    loop {
        term.draw(|f| draw(f, &rows))?;

        // 键盘轮询(最多等 200ms),保证退出响应快。
        if event::poll(Duration::from_millis(200))? {
            if let CEvent::Key(k) = event::read()? {
                let quit = matches!(k.code, KeyCode::Char('q') | KeyCode::Esc)
                    || (k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL));
                if quit {
                    return Ok(());
                }
            }
        }

        // 到刷新间隔则重扫。
        if last.elapsed() >= refresh {
            rows = collect_rows(cfg, classifier, backend);
            last = Instant::now();
        }
    }
}

/// 扫描当前会话 + 读状态文件,拼出表格行。扫描失败时返回空表(不崩 TUI)。
fn collect_rows(cfg: &Config, classifier: &Classifier, backend: &dyn Backend) -> Vec<Rowinfo> {
    let snaps = match watch::scan_snapshots(cfg, classifier, backend) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let store = StateStore::load(&cfg.state_file_path()).unwrap_or_default();
    let now = now_secs();

    let mut rows = Vec::new();
    for snap in snaps {
        // 当前状态已持续时长:now - 上次转移时刻(changed_at)。
        let dur_secs = store
            .changed_at
            .get(&snap.session)
            .map(|t| now.saturating_sub(*t))
            .unwrap_or(0);
        // 卡死旗标:stuck 元数据已 reported(由 scan_once 的卡住检测置位)。
        let stuck = store
            .stuck
            .get(&snap.session)
            .map(|m| m.reported)
            .unwrap_or(false);
        rows.push(Rowinfo {
            session: snap.session,
            profile: snap.profile,
            state: snap.state,
            dur_secs,
            context: snap.context.unwrap_or_default(),
            stuck,
        });
    }
    rows.sort_by(|a, b| a.session.cmp(&b.session));
    rows
}

fn state_style(state: State, stuck: bool) -> Style {
    if stuck {
        return Style::default()
            .fg(Color::Red)
            .add_modifier(Modifier::BOLD | Modifier::RAPID_BLINK);
    }
    match state {
        State::Working => Style::default().fg(Color::Yellow),
        State::Waiting => Style::default().fg(Color::Red),
        State::Idle => Style::default().fg(Color::Green),
        State::Unknown => Style::default().fg(Color::DarkGray),
    }
}

fn draw(f: &mut ratatui::Frame, rows: &[Rowinfo]) {
    let chunks = Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).split(f.area());

    let header = Row::new(vec![
        Cell::from("SESSION"),
        Cell::from("PROFILE"),
        Cell::from("STATE"),
        Cell::from("FOR"),
        Cell::from("CONTEXT"),
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));

    let body: Vec<Row> = rows
        .iter()
        .map(|r| {
            let label = if r.stuck {
                "stuck".to_string()
            } else {
                r.state.as_str().to_string()
            };
            Row::new(vec![
                Cell::from(r.session.clone()),
                Cell::from(r.profile.clone()),
                Cell::from(label).style(state_style(r.state, r.stuck)),
                Cell::from(fmt_dur(r.dur_secs)),
                Cell::from(r.context.clone()),
            ])
        })
        .collect();

    let widths = [
        Constraint::Length(16),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Min(10),
    ];

    let title = format!(" ccwatch — {} session(s) ", rows.len());
    let table = Table::new(body, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(title));
    f.render_widget(table, chunks[0]);

    let help = Line::from(vec![Span::styled(
        " q / Esc 退出 · 自动刷新 ",
        Style::default().fg(Color::DarkGray),
    )]);
    f.render_widget(ratatui::widgets::Paragraph::new(help), chunks[1]);
}
