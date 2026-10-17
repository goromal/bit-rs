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
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap},
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
    serde_json::json!({"name":c.name,"kind":Kind::try_from(c.kind).unwrap_or(Kind::Unspecified).as_str_name(),"state":state(c).as_str_name(),"run_id":c.run_id,"detail":c.detail,"started_unix_ms":c.started_unix_ms,"finished_unix_ms":c.finished_unix_ms,"rate_hz":c.rate_hz,"timeout_ms":c.timeout_ms,"remediation":c.remediation})
}
/// Duration as its two largest units, e.g. "5s", "1m 30s", "6d 5h"; a zero
/// second unit is dropped ("5m", not "5m 0s").
fn ago(ms: u64) -> String {
    let s = ms / 1000;
    let (d, h, m) = (s / 86_400, s / 3_600 % 24, s / 60 % 60);
    let (big, small) = match (d, h, m) {
        (0, 0, 0) => return format!("{s}s"),
        (0, 0, _) => ((m, "m"), (s % 60, "s")),
        (0, _, _) => ((h, "h"), (m, "m")),
        _ => ((d, "d"), (h, "h")),
    };
    match small.0 {
        0 => format!("{}{}", big.0, big.1),
        _ => format!("{}{} {}{}", big.0, big.1, small.0, small.1),
    }
}
/// Health checks show their period (1 / rate) in the same units as "Last run".
fn kind(c: &Check) -> String {
    if c.kind != Kind::Health as i32 {
        return "regression".into();
    }
    match c.rate_hz {
        r if r > 0.0 && r.is_finite() => match (1000.0 / r).round() as u64 {
            ms if ms < 1000 => format!("health · every {ms}ms"),
            ms => format!("health · every {}", ago(ms)),
        },
        _ => "health".into(),
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
fn remediation_text(c: &Check) -> &str {
    match c.remediation.as_str() {
        "" => "No suggestions from this plugin; see journalctl -u bitd.",
        text => text,
    }
}
/// Rows `text` occupies when greedily word-wrapped to `width`.
fn wrapped_rows(text: &str, width: u16) -> u16 {
    let width = usize::from(width.max(1));
    let mut rows = 0usize;
    for line in text.lines() {
        let (mut used, mut line_rows) = (0usize, 1usize);
        for word in line.split(' ').map(|w| w.chars().count()) {
            if used > 0 && used + 1 + word <= width {
                used += 1 + word;
            } else {
                if used > 0 {
                    line_rows += 1;
                }
                // A word longer than the width is broken across rows.
                line_rows += word.saturating_sub(1) / width;
                used = (word.saturating_sub(1) % width) + 1;
            }
        }
        rows += line_rows;
    }
    rows.min(usize::from(u16::MAX)) as u16
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
        let selected = checks.get(index);
        terminal.draw(|f| {
            let hint_rows =
                selected.map_or(0, |c| 1 + wrapped_rows(remediation_text(c), f.area().width));
            let areas = Layout::vertical([
                Constraint::Length(3),
                Constraint::Min(3),
                Constraint::Length(3),
                Constraint::Max(hint_rows),
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
                    Cell::from(kind(c)),
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
            .header(Row::new(["Check", "Kind · period", "Status", "Last run"]).bold())
            .row_highlight_style(Style::default().bg(Color::DarkGray))
            .highlight_symbol("› ");
            f.render_stateful_widget(table, areas[1], &mut selection);
            let detail = selected
                .map(|c| c.detail.as_str())
                .unwrap_or("No checks configured");
            f.render_widget(
                Paragraph::new(format!("{detail}\n{message}"))
                    .block(Block::default().borders(Borders::TOP)),
                areas[2],
            );
            if let Some(c) = selected {
                f.render_widget(
                    Paragraph::new(remediation_text(c))
                        .wrap(Wrap { trim: false })
                        .block(
                            Block::default()
                                .borders(Borders::TOP)
                                .title(format!(" If {} fails ", c.name))
                                .title_style(Style::default().fg(Color::Yellow).bold()),
                        ),
                    areas[3],
                );
            }
            f.render_widget(
                Paragraph::new("↑/↓ select   Enter run selected   r run regressions   q quit")
                    .fg(Color::Cyan),
                areas[4],
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
    fn health_shows_period_in_last_run_units() {
        let health = |rate_hz| Check {
            kind: Kind::Health as i32,
            rate_hz,
            ..Default::default()
        };
        assert_eq!(kind(&health(0.2)), "health · every 5s");
        assert_eq!(kind(&health(1.0 / 300.0)), "health · every 5m");
        assert_eq!(kind(&health(1.0 / 90.0)), "health · every 1m 30s");
        assert_eq!(kind(&health(4.0)), "health · every 250ms");
        assert_eq!(kind(&health(0.0)), "health");
        assert_eq!(kind(&Check::default()), "regression");
    }

    #[test]
    fn remediation_falls_back_and_sizes_by_wrapping() {
        assert!(remediation_text(&Check::default()).starts_with("No suggestions"));
        assert_eq!(wrapped_rows("abc\n\nabcdefghij", 5), 4);
        // "ab" / "cdef" / "gh": character counting would say 2 rows.
        assert_eq!(wrapped_rows("ab cdef gh", 5), 3);
        assert_eq!(wrapped_rows("", 0), 0);
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
        assert_eq!(
            last_run(&check(State::Passed, 0, now - 300_000), now),
            "5m ago"
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
