use crate::{
    config::Config,
    github::{Gh, Github},
    watcher::{Delivery, NtfyDelivery, Row, Store},
};
use anyhow::{Context, Result, bail};
use chrono::Utc;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::{
    DefaultTerminal,
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    widgets::{Block, Paragraph, Row as TableRow, Table, TableState, Wrap},
};
use std::{
    io::IsTerminal,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

pub fn run(config: &Config) -> Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("The dashboard needs an interactive terminal; use check for unattended runs");
    }
    let mut terminal = ratatui::try_init()?;
    let result = dashboard(&mut terminal, config);
    ratatui::restore();
    result
}

type RefreshResult = Result<(Vec<Row>, String)>;

struct Refresh {
    worker: Option<JoinHandle<RefreshResult>>,
    cancelled: Arc<AtomicBool>,
    started: Instant,
}

impl Refresh {
    fn start(work: impl FnOnce(Arc<AtomicBool>) -> RefreshResult + Send + 'static) -> Self {
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = cancelled.clone();
        Self {
            worker: Some(std::thread::spawn(move || work(flag))),
            cancelled,
            started: Instant::now(),
        }
    }

    fn poll(&mut self) -> Option<RefreshResult> {
        if !self.worker.as_ref()?.is_finished() {
            return None;
        }
        Some(
            self.worker
                .take()?
                .join()
                .unwrap_or_else(|_| Err(anyhow::anyhow!("Refresh worker panicked"))),
        )
    }
}

impl Drop for Refresh {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn fetch_dashboard(config: &Config, cancelled: Arc<AtomicBool>) -> RefreshResult {
    let delivery = config
        .ntfy
        .as_ref()
        .filter(|ntfy| ntfy.enabled)
        .map(NtfyDelivery::new)
        .transpose()?;
    let mut store = Store::open(&config.database)?;
    let checked = store.check(
        config,
        &Github(Gh::new(cancelled)),
        delivery.as_ref().map(|d| d as &dyn Delivery),
        Utc::now,
        false,
    );
    let rows = store.rows()?;
    let last = store
        .meta("last_refresh")?
        .unwrap_or_else(|| "never".into());
    let status = match checked {
        Ok(_) => format!(
            "Last successful refresh: {last} | ntfy: {}",
            if delivery.is_some() {
                "enabled"
            } else {
                "disabled (alerts remain pending)"
            }
        ),
        Err(error) => {
            format!("Refresh failed; cached data may be stale. Last success: {last}: {error:#}")
        }
    };
    Ok((rows, status))
}

fn dashboard(terminal: &mut DefaultTerminal, config: &Config) -> Result<()> {
    let mut rows: Vec<Row>;
    let mut selection = TableState::default().with_selected(Some(0));
    let mut status;
    match (|| -> Result<(Vec<Row>, String)> {
        let store = Store::open(&config.database)?;
        let last = store
            .meta("last_refresh")?
            .unwrap_or_else(|| "never".into());
        Ok((
            store.rows()?,
            format!("Showing cached data. Last successful refresh: {last}"),
        ))
    })() {
        Ok((cached, message)) => {
            rows = cached;
            status = message;
        }
        Err(error) => {
            rows = Vec::new();
            status = format!("Cannot load cached dashboard: {error:#}");
        }
    }
    let mut refresh = true;
    let mut refreshed = Instant::now();
    let mut job: Option<Refresh> = None;
    loop {
        if refresh && job.is_none() {
            let config = config.clone();
            job = Some(Refresh::start(move |cancelled| {
                fetch_dashboard(&config, cancelled)
            }));
            refresh = false;
        }
        if let Some(result) = job.as_mut().and_then(Refresh::poll) {
            job = None;
            match result {
                Ok((updated, message)) => {
                    rows = updated;
                    status = message;
                }
                Err(error) => {
                    status = format!("Refresh failed; cached data may be stale: {error:#}")
                }
            }
            if selection
                .selected()
                .is_some_and(|index| index >= rows.len())
            {
                selection.select(rows.len().checked_sub(1));
            } else if selection.selected().is_none() && !rows.is_empty() {
                selection.select(Some(0));
            }
            refreshed = Instant::now();
        }
        terminal.draw(|frame| {
            let areas = Layout::vertical([Constraint::Min(5), Constraint::Length(3), Constraint::Length(4), Constraint::Length(2)]).split(frame.area());
            let now = Utc::now();
            let body = rows.iter().map(|row| {
                TableRow::new(vec![
                    format!("{}{}",if row.pinned { "*" } else { " " },if row.snooze_until > now.timestamp() { "z" } else { " " }),
                    row.pr.author.clone(), format!("{}#{}",row.pr.repo,row.pr.number),
                    elapsed((now-row.pr.created).num_seconds()),
                    elapsed((now-row.activity).num_seconds()),
                    if row.pr.draft { "DRAFT".into() } else { row.pr.review.clone() },
                    row.pr.title.clone(),
                ])
            });
            let table = Table::new(body, [
                Constraint::Length(2),Constraint::Length(16),Constraint::Percentage(24),
                Constraint::Length(7),Constraint::Length(7),Constraint::Length(18),Constraint::Min(16),
            ]).header(TableRow::new(["", "Author","Repository / PR","Age","Quiet","Review","Title"])
                .style(Style::default().add_modifier(Modifier::BOLD)))
                .block(Block::bordered().title("Review watch"))
                .row_highlight_style(Style::default().bg(Color::DarkGray));
            frame.render_stateful_widget(table,areas[0],&mut selection);
            let detail = selection.selected().and_then(|i| rows.get(i)).map(|row| {
                format!("{}\nReview requested: {}",row.pr.url,
                    if row.pr.requested.is_empty() { "none".into() } else { row.pr.requested.join(", ") })
            }).unwrap_or_default();
            frame.render_widget(Paragraph::new(detail).wrap(Wrap { trim:false }),areas[1]);
            let progress=job.as_ref().map(|job|format!("Fetching GitHub activity... {}s elapsed | q: quit\n{status}",job.started.elapsed().as_secs()))
                .unwrap_or_else(||status.clone());
            frame.render_widget(Paragraph::new(progress).wrap(Wrap {trim:false}),areas[2]);
            frame.render_widget(Paragraph::new("j/k or arrows: select | r: refresh | o/Enter: open | p: pin | s: snooze 8h | q: quit\nAuto-refresh: 5m. Pins only change display order; snooze delays all alerts."),areas[3]);
        })?;
        if event::poll(Duration::from_millis(250))?
            && let Event::Key(key) = event::read()?
        {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            let selected = selection.selected().and_then(|i| rows.get(i));
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Char('c') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                    return Ok(());
                }
                KeyCode::Char('r') if job.is_none() => refresh = true,
                KeyCode::Down | KeyCode::Char('j') if !rows.is_empty() => selection.select(Some(
                    (selection.selected().unwrap_or(0) + 1).min(rows.len() - 1),
                )),
                KeyCode::Up | KeyCode::Char('k') => {
                    selection.select(Some(selection.selected().unwrap_or(0).saturating_sub(1)));
                }
                KeyCode::Enter | KeyCode::Char('o') => {
                    if let Some(row) = selected {
                        let output = Command::new("gh")
                            .args(["browse", &row.pr.number.to_string(), "--repo", &row.pr.repo])
                            .output()
                            .context("Open PR with gh browse");
                        status = match output {
                            Ok(output) if output.status.success() => "Opened PR in browser".into(),
                            Ok(output) => {
                                format!("Open failed: {}", String::from_utf8_lossy(&output.stderr))
                            }
                            Err(error) => format!("{error:#}"),
                        };
                    }
                }
                KeyCode::Char('p') | KeyCode::Char('s') => {
                    if let Some(row) = selected {
                        let key_value = row.pr.key();
                        let result = (|| -> Result<Vec<Row>> {
                            let store = Store::open(&config.database)?;
                            if key.code == KeyCode::Char('p') {
                                store.toggle_pin(&key_value)?;
                            } else {
                                store.snooze(&key_value, Utc::now())?;
                            }
                            store.rows()
                        })();
                        match result {
                            Ok(updated) => {
                                rows = updated;
                                selection
                                    .select(rows.iter().position(|row| row.pr.key() == key_value));
                                status = if key.code == KeyCode::Char('p') {
                                    "Pin updated".into()
                                } else {
                                    "Alerts snoozed for 8 hours".into()
                                };
                            }
                            Err(error) => status = format!("Action failed: {error:#}"),
                        }
                    }
                }
                _ => {}
            }
        }
        if job.is_none() && refreshed.elapsed() >= Duration::from_secs(300) {
            refresh = true;
        }
    }
}

fn elapsed(seconds: i64) -> String {
    let seconds = seconds.max(0);
    if seconds >= 86400 {
        format!("{}d{}h", seconds / 86400, (seconds % 86400) / 3600)
    } else if seconds >= 3600 {
        format!("{}h{}m", seconds / 3600, (seconds % 3600) / 60)
    } else {
        format!("{}m", seconds / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn slow_refresh_does_not_block_poll_and_completion_can_repeat() -> Result<()> {
        use std::sync::mpsc;
        for _ in 0..2 {
            let (entered_tx, entered_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let mut job = Refresh::start(move |_| {
                entered_tx.send(())?;
                release_rx.recv()?;
                Ok((Vec::new(), "complete".into()))
            });
            entered_rx.recv_timeout(Duration::from_secs(5))?;
            let started = Instant::now();
            assert!(job.poll().is_none());
            assert!(started.elapsed() < Duration::from_millis(100));
            release_tx.send(())?;
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Some(result) = job.poll() {
                    assert_eq!(result?.1, "complete");
                    assert!(job.poll().is_none());
                    break;
                }
                assert!(Instant::now() < deadline, "Worker did not complete");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        Ok(())
    }

    #[test]
    fn dropping_refresh_signals_cancellation_and_joins_worker() -> Result<()> {
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (stopped_tx, stopped_rx) = std::sync::mpsc::channel();
        let job = Refresh::start(move |cancelled| {
            entered_tx.send(())?;
            let deadline = Instant::now() + Duration::from_secs(5);
            while !cancelled.load(Ordering::Relaxed) {
                assert!(Instant::now() < deadline, "Cancellation did not arrive");
                std::thread::sleep(Duration::from_millis(5));
            }
            stopped_tx.send(())?;
            Ok((Vec::new(), "cancelled".into()))
        });
        entered_rx.recv_timeout(Duration::from_secs(5))?;
        drop(job);
        stopped_rx.try_recv()?;
        Ok(())
    }
    #[test]
    fn durations_remain_readable() {
        assert_eq!(elapsed(8 * 3600), "8h0m");
        assert_eq!(elapsed(-1), "0m");
        assert_eq!(elapsed(25 * 3600), "1d1h");
    }
}
