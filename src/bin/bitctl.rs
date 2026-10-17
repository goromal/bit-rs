use anyhow::{bail, Result};
use bit_rs::proto::{
    bit_service_client::BitServiceClient, Check, Kind, ListRequest, RunRequest, State,
};
use clap::{Parser, Subcommand};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
};
use std::{io, time::Duration};
use tonic::transport::{Channel, Endpoint};

#[derive(Parser)]
#[command(about = "Monitor machine health and run regression checks (default: interactive TUI)")]
struct Args {
    #[arg(long, env = "BITD_ENDPOINT", default_value = "http://127.0.0.1:40041")]
    endpoint: String,
    #[command(subcommand)]
    command: Option<Action>,
}
#[derive(Subcommand)]
enum Action {
    List {
        #[arg(long)]
        json: bool,
    },
    Run {
        names: Vec<String>,
        #[arg(long)]
        wait: bool,
    },
}
fn state(c: &Check) -> State {
    State::try_from(c.state).unwrap_or(State::Unspecified)
}
fn json(c: &Check) -> serde_json::Value {
    serde_json::json!({"name":c.name,"kind":Kind::try_from(c.kind).unwrap_or(Kind::Unspecified).as_str_name(),"state":state(c).as_str_name(),"run_id":c.run_id,"detail":c.detail,"started_unix_ms":c.started_unix_ms,"finished_unix_ms":c.finished_unix_ms,"rate_hz":c.rate_hz,"timeout_ms":c.timeout_ms})
}
/// Age as its two largest units, e.g. "5s", "1m 30s", "6d 5h".
fn ago(ms: u64) -> String {
    let s = ms / 1000;
    let (d, h, m) = (s / 86_400, s / 3_600 % 24, s / 60 % 60);
    match (d, h, m) {
        (0, 0, 0) => format!("{s}s"),
        (0, 0, _) => format!("{m}m {}s", s % 60),
        (0, _, _) => format!("{h}h {m}m"),
        _ => format!("{d}d {h}h"),
    }
}
/// When the check last ran, relative to `now_ms` (Unix epoch milliseconds).
fn last_run(c: &Check, now_ms: i64) -> String {
    if state(c) == State::Running {
        return "now".into();
    }
    match c.finished_unix_ms.max(c.started_unix_ms) {
        at if at <= 0 => "never".into(),
        at => format!("{} ago", ago(now_ms.saturating_sub(at).max(0) as u64)),
    }
}
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}
fn print_checks(checks: &[Check]) {
    for c in checks {
        println!("{:<24} {:<12} {}", c.name, state(c).as_str_name(), c.detail);
    }
}
#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let endpoint = Endpoint::from_shared(args.endpoint)?
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(3));
    let mut client = BitServiceClient::new(endpoint.connect().await?);
    match args.command {
        Some(Action::List { json: as_json }) => {
            let checks = client.list(ListRequest {}).await?.into_inner().checks;
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&checks.iter().map(json).collect::<Vec<_>>())?
                );
            } else {
                print_checks(&checks);
            }
        }
        Some(Action::Run { names, wait }) => {
            let started = client.run(RunRequest { names }).await?.into_inner().checks;
            if !wait {
                print_checks(&started);
                return Ok(());
            }
            loop {
                let checks = client.list(ListRequest {}).await?.into_inner().checks;
                let mut results = Vec::new();
                for initial in &started {
                    let current = checks
                        .iter()
                        .find(|c| c.name == initial.name)
                        .ok_or_else(|| anyhow::anyhow!("check disappeared: {}", initial.name))?;
                    if current.run_id != initial.run_id {
                        bail!(
                            "run of {} was superseded; inspect the daemon journal",
                            initial.name
                        );
                    }
                    results.push(current.clone());
                }
                if results.iter().all(|c| state(c) != State::Running) {
                    print_checks(&results);
                    if results.iter().any(|c| state(c) != State::Passed) {
                        bail!("one or more checks failed");
                    }
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        None => tui(client).await?,
    }
    Ok(())
}
struct Restore;
impl Drop for Restore {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}
async fn tui(mut client: BitServiceClient<Channel>) -> Result<()> {
    enable_raw_mode()?;
    let _restore = Restore;
    execute!(io::stdout(), EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut selection = TableState::default().with_selected(0);
    let mut message = String::new();
    let mut checks = Vec::new();
    let mut tick = tokio::time::Instant::now();
    loop {
        if tick.elapsed() >= Duration::from_millis(500) || checks.is_empty() {
            match client.list(ListRequest {}).await {
                Ok(r) => checks = r.into_inner().checks,
                Err(e) => message = format!("Disconnected: {e}"),
            }
            tick = tokio::time::Instant::now();
        }
        let index = selection
            .selected()
            .unwrap_or(0)
            .min(checks.len().saturating_sub(1));
        selection.select(Some(index));
        terminal.draw(|f| {
            let areas = Layout::vertical([
                Constraint::Length(3),
                Constraint::Min(3),
                Constraint::Length(4),
                Constraint::Length(2),
            ])
            .split(f.area());
            f.render_widget(
                Paragraph::new("BIT • machine health & regression")
                    .style(Style::default().fg(Color::Cyan).bold())
                    .block(Block::default().borders(Borders::ALL)),
                areas[0],
            );
            let now = now_ms();
            let rows = checks.iter().map(|c| {
                let color = match state(c) {
                    State::Passed => Color::Green,
                    State::Failed | State::TimedOut => Color::Red,
                    State::Running => Color::Yellow,
                    _ => Color::DarkGray,
                };
                Row::new(vec![
                    Cell::from(c.name.clone()),
                    Cell::from(if c.kind == Kind::Health as i32 {
                        format!("health · {} Hz", c.rate_hz)
                    } else {
                        "regression".into()
                    }),
                    Cell::from(format!("{:?}", state(c))).style(Style::default().fg(color)),
                    Cell::from(last_run(c, now)),
                ])
            });
            let table = Table::new(
                rows,
                [
                    Constraint::Percentage(35),
                    Constraint::Percentage(25),
                    Constraint::Percentage(20),
                    Constraint::Percentage(20),
                ],
            )
            .header(Row::new(["Check", "Kind", "Status", "Last run"]).bold())
            .row_highlight_style(Style::default().bg(Color::DarkGray))
            .highlight_symbol("› ");
            f.render_stateful_widget(table, areas[1], &mut selection);
            let detail = checks
                .get(index)
                .map(|c| c.detail.as_str())
                .unwrap_or("No checks configured");
            f.render_widget(
                Paragraph::new(format!("{detail}\n{message}"))
                    .block(Block::default().borders(Borders::TOP)),
                areas[2],
            );
            f.render_widget(
                Paragraph::new("↑/↓ select   Enter run selected   r run regressions   q quit")
                    .fg(Color::Cyan),
                areas[3],
            );
        })?;
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Char('c') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                        break
                    }
                    KeyCode::Up => selection.select(Some(index.saturating_sub(1))),
                    KeyCode::Down => {
                        selection.select(Some((index + 1).min(checks.len().saturating_sub(1))))
                    }
                    KeyCode::Enter | KeyCode::Char('r') => {
                        let names = if key.code == KeyCode::Enter {
                            match checks.get(index) {
                                Some(c) => vec![c.name.clone()],
                                None => continue,
                            }
                        } else {
                            vec![]
                        };
                        message = match client.run(RunRequest { names }).await {
                            Ok(r) => format!("Started {} check(s)", r.into_inner().checks.len()),
                            Err(e) => e.message().into(),
                        };
                        tick = tokio::time::Instant::now() - Duration::from_secs(1);
                    }
                    _ => (),
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(state: State, started: i64, finished: i64) -> Check {
        Check {
            state: state as i32,
            started_unix_ms: started,
            finished_unix_ms: finished,
            ..Default::default()
        }
    }

    #[test]
    fn last_run_is_relative_and_compact() {
        let now = 1_000_000_000_000;
        assert_eq!(last_run(&check(State::Idle, 0, 0), now), "never");
        assert_eq!(last_run(&check(State::Running, now - 9_000, 0), now), "now");
        assert_eq!(
            last_run(&check(State::Passed, now - 6_000, now - 5_400), now),
            "5s ago"
        );
        assert_eq!(
            last_run(&check(State::Failed, 0, now - 90_000), now),
            "1m 30s ago"
        );
        assert_eq!(
            last_run(&check(State::Passed, 0, now - 7_500_000), now),
            "2h 5m ago"
        );
        let six_days_five_hours = (6 * 86_400 + 5 * 3_600 + 59) * 1000;
        assert_eq!(
            last_run(&check(State::Passed, 0, now - six_days_five_hours), now),
            "6d 5h ago"
        );
        // A clock that steps backwards never yields a negative age.
        assert_eq!(
            last_run(&check(State::Passed, 0, now + 4_000), now),
            "0s ago"
        );
    }
}
