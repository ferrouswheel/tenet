use std::collections::VecDeque;
use std::env;
use std::fs;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, Paragraph};
use ratatui::Terminal;
use tokio::sync::mpsc;

use tenet::crypto::generate_keypair;
use tenet::protocol::MessageKind;
use tenet::simulation::{
    CountSummary, LogEntry, RollingLatencySnapshot, SimulationAggregateMetrics, SimulationClient,
    SimulationControlCommand, SimulationScenarioConfig, SimulationStepUpdate, SizeSummary,
};

const MESSAGE_KIND_ORDER: [MessageKind; 5] = [
    MessageKind::Public,
    MessageKind::Meta,
    MessageKind::Direct,
    MessageKind::FriendGroup,
    MessageKind::StoreForPeer,
];

#[derive(Debug)]
enum InputMode {
    Normal,
    AddPeer { buffer: String },
    AddFriend { buffer: String },
}

#[derive(Debug, PartialEq, Eq)]
enum InputAction {
    None,
    Quit,
}

/// Adapts the `tlog!` global writer to the TUI log channel.
///
/// Lines written by [`tlog!`] (from any module — message handler, sync loop,
/// etc.) are forwarded as [`LogEntry`] values and appear in the TUI log panel
/// alongside relay and peer entries.
struct TuiLogWriter {
    tx: mpsc::UnboundedSender<LogEntry>,
    sim_time: Arc<Mutex<f64>>,
}

impl std::io::Write for TuiLogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Ok(s) = std::str::from_utf8(buf) {
            let msg = s.trim_end_matches('\n').to_string();
            if !msg.is_empty() {
                let sim_t = *self.sim_time.lock().unwrap();
                let _ = self.tx.send(LogEntry {
                    sim_time: sim_t,
                    source: "tlog".to_string(),
                    message: msg,
                });
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), String> {
    let mut args = env::args().skip(1);
    let mut use_tui = false;
    let mut path = None;

    while let Some(arg) = args.next() {
        if arg == "--tui" {
            use_tui = true;
        } else if path.is_none() {
            path = Some(arg);
        } else {
            return Err("usage: simulation [--tui] <path-to-scenario.toml>".to_string());
        }
    }

    let path =
        path.ok_or_else(|| "usage: simulation [--tui] <path-to-scenario.toml>".to_string())?;
    let contents = fs::read_to_string(&path).map_err(|err| err.to_string())?;
    let scenario: SimulationScenarioConfig =
        toml::from_str(&contents).map_err(|err| err.to_string())?;

    let report = if use_tui {
        run_with_tui(scenario).await?
    } else {
        tenet::simulation::run_event_based_scenario(scenario).await?
    };
    let output = serde_json::to_string_pretty(&report).map_err(|err| err.to_string())?;
    println!("{output}");
    Ok(())
}

async fn run_with_tui(
    scenario: SimulationScenarioConfig,
) -> Result<tenet::simulation::SimulationReport, String> {
    const LOG_LIMIT: usize = 500;
    const SPEED_STEP: f64 = 0.10;

    let (tx, mut rx) = mpsc::unbounded_channel();
    let (control_tx, control_rx) = mpsc::unbounded_channel();
    // Single combined log channel for all sources (sim, relay, peers, tlog).
    let (log_tx, mut log_rx) = mpsc::unbounded_channel::<LogEntry>();
    let (input_tx, mut input_rx) = mpsc::unbounded_channel();

    // Shared clock: the relay log sink reads it to stamp entries with the current
    // sim time.  The harness updates it right after every clock jump (before
    // processing events), so relay HTTP calls see the correct sim time.
    let relay_sim_time: Arc<Mutex<f64>> = Arc::new(Mutex::new(0.0));

    // Route tlog! output into the TUI log panel instead of stderr, before raw
    // mode is enabled so no log lines can corrupt the terminal display.
    tenet::logging::set_writer(Box::new(TuiLogWriter {
        tx: log_tx.clone(),
        sim_time: relay_sim_time.clone(),
    }));

    enable_raw_mode().map_err(|err| err.to_string())?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).map_err(|err| err.to_string())?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).map_err(|err| err.to_string())?;

    let mut relay_config = scenario.relay.clone().into_relay_config();
    {
        let relay_sim_time = relay_sim_time.clone();
        let log_tx = log_tx.clone();
        relay_config.log_sink = Some(Arc::new(move |line: String| {
            let sim_t = *relay_sim_time.lock().unwrap();
            let message = line.strip_prefix("relay: ").unwrap_or(&line).to_string();
            let _ = log_tx.send(LogEntry {
                sim_time: sim_t,
                source: "relay".to_string(),
                message,
            });
        }));
    }
    let sim_log_sink: Arc<dyn Fn(LogEntry) + Send + Sync> = {
        let log_tx = log_tx.clone();
        Arc::new(move |entry: LogEntry| {
            let _ = log_tx.send(entry);
        })
    };
    let scenario_for_task = scenario.clone();
    // For event-based simulation, total_steps is the duration in seconds
    let total_duration_seconds = scenario.simulation.duration_seconds.unwrap_or_else(|| {
        let steps = scenario.simulation.effective_steps();
        let seconds_per_step = scenario.simulation.simulated_time.seconds_per_step;
        (steps as u64 * seconds_per_step) as u64
    });
    let sim_handle = tokio::spawn(async move {
        tenet::simulation::run_event_based_scenario_with_tui(
            scenario_for_task,
            control_rx,
            relay_config,
            Some(sim_log_sink),
            Some(relay_sim_time),
            |update| {
                let _ = tx.send(update);
            },
        )
        .await
    });

    let input_shutdown = Arc::new(AtomicBool::new(false));
    let input_shutdown_task = input_shutdown.clone();
    let input_handle = tokio::task::spawn_blocking(move || {
        while !input_shutdown_task.load(Ordering::SeqCst) {
            match event::poll(Duration::from_millis(50)) {
                Ok(true) => match event::read() {
                    Ok(event) => {
                        if input_tx.send(event).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                },
                Ok(false) => {}
                Err(_) => break,
            }
        }
    });

    let mut log_entries: VecDeque<LogEntry> = VecDeque::with_capacity(LOG_LIMIT);
    // None = auto-tail (follow new entries).
    // Some(n) = anchored: first visible entry is at sorted index n. New
    //           entries do not move the view. Reverts to None when the user
    //           scrolls back to the bottom.
    let mut log_scroll: Option<usize> = None;
    // Last known first-visible-line index, updated by every render call so
    // that Up/PgUp know where to anchor from when coming out of auto-tail.
    let mut log_view_start: usize = 0;
    let mut input_mode = InputMode::Normal;
    let mut status_message = "Simulation running.".to_string();
    let mut auto_peer_index = 1usize;
    let mut paused = false;
    let base_seconds_per_step = scenario.simulation.simulated_time.seconds_per_step as f64;

    // Create initial update
    let mut current_update = SimulationStepUpdate {
        step: 0,
        total_steps: total_duration_seconds as usize,
        online_nodes: 0,
        total_peers: scenario.simulation.node_ids.len(),
        speed_factor: scenario.simulation.simulated_time.default_speed_factor,
        sent_messages: 0,
        received_messages: 0,
        rolling_latency: RollingLatencySnapshot {
            min: None,
            max: None,
            average: None,
            samples: 0,
            window: 0,
        },
        aggregate_metrics: tenet::simulation::SimulationAggregateMetrics::empty(),
    };

    // Render initial state
    render(
        &mut terminal,
        &current_update,
        &[],
        &mut log_scroll,
        &mut log_view_start,
        &status_message,
        paused,
        base_seconds_per_step,
    )
    .map_err(|err| err.to_string())?;

    loop {
        tokio::select! {
            update = rx.recv() => {
                match update {
                    Some(update) => {
                        current_update = update;
                        render(
                            &mut terminal,
                            &current_update,
                            log_entries.make_contiguous(),
                            &mut log_scroll,
                            &mut log_view_start,
                            &status_message,
                            paused,
                            base_seconds_per_step,
                        )
                        .map_err(|err| err.to_string())?;
                    }
                    None => break,
                }
            }
            log_entry = log_rx.recv() => {
                if let Some(entry) = log_entry {
                    if log_entries.len() == LOG_LIMIT {
                        log_entries.pop_front();
                    }
                    log_entries.push_back(entry);
                    render(
                        &mut terminal,
                        &current_update,
                        log_entries.make_contiguous(),
                        &mut log_scroll,
                        &mut log_view_start,
                        &status_message,
                        paused,
                        base_seconds_per_step,
                    )
                    .map_err(|err| err.to_string())?;
                }
                // Don't break here - let the main rx.recv() handle loop exit.
            }
            input_event = input_rx.recv() => {
                if let Some(Event::Key(key)) = input_event {
                    let action = handle_key_event(
                        key.code,
                        &mut input_mode,
                        &mut status_message,
                        &control_tx,
                        &mut auto_peer_index,
                        total_duration_seconds as usize,
                        SPEED_STEP,
                        &mut paused,
                        &mut log_scroll,
                        log_view_start,
                    );
                    if action == InputAction::Quit {
                        let _ = control_tx.send(SimulationControlCommand::Stop);
                        status_message = "Stopping simulation...".to_string();
                        // Don't break - let simulation finish naturally
                    }
                    render(
                        &mut terminal,
                        &current_update,
                        log_entries.make_contiguous(),
                        &mut log_scroll,
                        &mut log_view_start,
                        &status_message,
                        paused,
                        base_seconds_per_step,
                    )
                    .map_err(|err| err.to_string())?;
                }
            }
        }
    }

    // Simulation has finished (rx closed). Drain any remaining log entries.
    while let Ok(entry) = log_rx.try_recv() {
        if log_entries.len() == LOG_LIMIT {
            log_entries.pop_front();
        }
        log_entries.push_back(entry);
    }
    status_message = "Simulation complete. Press q to exit.".to_string();
    render(
        &mut terminal,
        &current_update,
        log_entries.make_contiguous(),
        &mut log_scroll,
        &mut log_view_start,
        &status_message,
        paused,
        base_seconds_per_step,
    )
    .map_err(|err| err.to_string())?;

    // Post-simulation event loop: scroll the log and quit on q.
    loop {
        match input_rx.recv().await {
            Some(Event::Key(key)) => match key.code {
                KeyCode::Char('q') => break,
                KeyCode::Up => {
                    log_scroll = Some(log_view_start.saturating_sub(1));
                }
                KeyCode::Down => {
                    if let Some(n) = log_scroll {
                        log_scroll = Some(n.saturating_add(1));
                    }
                }
                KeyCode::PageUp => {
                    log_scroll = Some(log_view_start.saturating_sub(10));
                }
                KeyCode::PageDown => {
                    if let Some(n) = log_scroll {
                        log_scroll = Some(n.saturating_add(10));
                    }
                }
                _ => continue,
            },
            _ => continue,
        }
        render(
            &mut terminal,
            &current_update,
            log_entries.make_contiguous(),
            &mut log_scroll,
            &mut log_view_start,
            &status_message,
            paused,
            base_seconds_per_step,
        )
        .map_err(|err| err.to_string())?;
    }

    input_shutdown.store(true, Ordering::SeqCst);
    let _ = input_handle.await;

    disable_raw_mode().map_err(|err| err.to_string())?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen).map_err(|err| err.to_string())?;
    terminal.show_cursor().map_err(|err| err.to_string())?;

    sim_handle
        .await
        .map_err(|err| format!("simulation task failed: {err}"))?
}

fn render(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    update: &SimulationStepUpdate,
    log_entries: &[LogEntry],
    log_scroll: &mut Option<usize>,
    log_view_start: &mut usize,
    status: &str,
    paused: bool,
    _base_seconds_per_step: f64,
) -> io::Result<()> {
    terminal
        .draw(|frame| {
            let size = frame.size();
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .margin(1)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Length(9),
                    Constraint::Length(10),
                    Constraint::Min(8),
                    Constraint::Length(4),
                    Constraint::Length(4),
                ])
                .split(size);

            // For event-based simulation, step and total_steps are already in seconds
            let current_sim_seconds = update.step as f64;
            let total_sim_seconds = update.total_steps as f64;
            let progress = current_sim_seconds / total_sim_seconds.max(1.0);
            let progress_gauge = Gauge::default()
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Simulation Progress"),
                )
                .gauge_style(Style::default().fg(Color::Cyan))
                .ratio(progress)
                .label(format!(
                    "Time {} / {} ({:.1}%)",
                    format_duration(current_sim_seconds),
                    format_duration(total_sim_seconds),
                    progress * 100.0
                ));
            frame.render_widget(progress_gauge, chunks[0]);

            let speed_display = if update.speed_factor.is_infinite() {
                "Speed: FastForward".to_string()
            } else if update.speed_factor == 0.0 {
                "Speed: Paused".to_string()
            } else {
                format!("Speed: {:.2}x", update.speed_factor)
            };
            let metrics_lines = vec![
                Line::from(Span::raw(format!("Total peers: {}", update.total_peers))),
                Line::from(Span::raw(format!("Online peers: {}", update.online_nodes))),
                Line::from(Span::raw(speed_display)),
                Line::from(Span::raw(format!("Sim time: {:.1}s", current_sim_seconds))),
                Line::from(Span::raw(format!(
                    "Messages sent (recent): {}",
                    update.sent_messages
                ))),
                Line::from(Span::raw(format!(
                    "Messages received (recent): {}",
                    update.received_messages
                ))),
                Line::from(Span::raw(format!(
                    "Rolling latency (last {}): {}",
                    update.rolling_latency.window,
                    format_latency(&update.rolling_latency)
                ))),
            ];
            let metrics_block = Paragraph::new(metrics_lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Current Metrics"),
            );
            frame.render_widget(metrics_block, chunks[1]);

            let aggregate_lines =
                aggregate_metrics_lines(&update.aggregate_metrics, chunks[2].height);
            let aggregate_block = Paragraph::new(aggregate_lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Aggregate Metrics"),
            );
            frame.render_widget(aggregate_block, chunks[2]);

            // Combined activity log with scroll support.
            // Sort by sim_time so that relay entries (which arrive with a lag
            // relative to the fast-forwarding simulator) appear at the correct
            // chronological position rather than at the end of the visible window.
            let mut sorted: Vec<&LogEntry> = log_entries.iter().collect();
            sorted.sort_by(|a, b| {
                a.sim_time
                    .partial_cmp(&b.sim_time)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            let visible_height = chunks[3].height.saturating_sub(2) as usize;
            let total_entries = sorted.len();
            // max_start: largest first-line index that still fills the view.
            let max_start = total_entries.saturating_sub(visible_height);
            let start_idx = match *log_scroll {
                None => max_start, // auto-tail: show newest entries
                Some(n) => {
                    let clamped = n.min(max_start);
                    if clamped >= max_start {
                        // Scrolled back to the bottom — resume auto-tail.
                        *log_scroll = None;
                    } else {
                        *log_scroll = Some(clamped);
                    }
                    clamped
                }
            };
            *log_view_start = start_idx;
            let end_idx = (start_idx + visible_height).min(total_entries);
            let visible = &sorted[start_idx..end_idx];
            let log_lines: Vec<Line> = visible.iter().map(|e| format_log_entry(e)).collect();
            let lines_from_bottom = max_start.saturating_sub(start_idx);
            let log_title = if lines_from_bottom > 0 {
                format!("Activity Log  ↑{lines_from_bottom} lines  (↑/↓ PgUp/PgDn to scroll)")
            } else {
                "Activity Log  (↑/↓ PgUp/PgDn to scroll)".to_string()
            };
            let log_block = Paragraph::new(log_lines)
                .block(Block::default().borders(Borders::ALL).title(log_title));
            frame.render_widget(log_block, chunks[3]);

            let status_lines = vec![
                Line::from(Span::raw(format!(
                    "State: {}",
                    if paused { "Paused" } else { "Running" }
                ))),
                Line::from(Span::raw(format!(
                    "Online: {}/{}",
                    update.online_nodes, update.total_peers
                ))),
                Line::from(Span::raw(format!("Last: {status}"))),
            ];
            let hint = Paragraph::new(status_lines)
                .block(Block::default().borders(Borders::ALL).title("Status"));
            frame.render_widget(hint, chunks[4]);

            let command_lines = vec![
                Line::from(Span::raw(
                    "a: add peer | f: add friends | +/-: adjust speed",
                )),
                Line::from(Span::raw(
                    "F: fast-forward | R: real-time | P: pause | space: pause/resume",
                )),
                Line::from(Span::raw("q: quit")),
            ];
            let command_block = Paragraph::new(command_lines)
                .block(Block::default().borders(Borders::ALL).title("Commands"));
            frame.render_widget(command_block, chunks[5]);
        })
        .map(|_| ())
}

/// Format a single log entry as a coloured ratatui `Line`.
///
/// Layout: `t=NNNNN.N [SOURCE   ] message`
/// - Relay entries are yellow, peer entries are cyan, sim entries are blue.
fn format_log_entry(entry: &LogEntry) -> Line<'static> {
    let time_part = format!("t={:>8.1}s ", entry.sim_time);
    // Truncate long source names (e.g. very long peer IDs) to keep layout tidy.
    let source_display = if entry.source.len() > 9 {
        format!("{}… ", &entry.source[..8])
    } else {
        format!("{:<9} ", entry.source)
    };
    let source_part = format!("[{source_display}] ");
    let source_color = match entry.source.as_str() {
        "sim" => Color::Blue,
        "relay" => Color::Yellow,
        _ => Color::Cyan,
    };
    Line::from(vec![
        Span::styled(time_part, Style::default().fg(Color::DarkGray)),
        Span::styled(source_part, Style::default().fg(source_color)),
        Span::raw(entry.message.clone()),
    ])
}

fn format_duration(seconds: f64) -> String {
    let total_seconds = seconds.max(0.0).round() as u64;
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let secs = total_seconds % 60;
    if hours > 0 {
        format!("{hours:02}:{minutes:02}:{secs:02}")
    } else {
        format!("{minutes:02}:{secs:02}")
    }
}

fn format_latency(snapshot: &RollingLatencySnapshot) -> String {
    if snapshot.samples == 0 {
        return "no samples yet".to_string();
    }
    let min = snapshot.min.unwrap_or(0);
    let max = snapshot.max.unwrap_or(0);
    let avg = snapshot.average.unwrap_or(0.0);
    format!(
        "min {} / avg {:.2} / max {} ({} samples)",
        min, avg, max, snapshot.samples
    )
}

fn aggregate_metrics_lines(metrics: &SimulationAggregateMetrics, height: u16) -> Vec<Line<'_>> {
    let mut lines = vec![
        Line::from(Span::raw(format!(
            "Feed messages: {}",
            format_count_summary(&metrics.peer_feed_messages)
        ))),
        Line::from(Span::raw(format!(
            "Stored unforwarded: {}",
            format_count_summary(&metrics.stored_unforwarded_by_peer)
        ))),
        Line::from(Span::raw(format!(
            "Stored forwarded: {}",
            format_count_summary(&metrics.stored_forwarded_by_peer)
        ))),
    ];

    for kind in MESSAGE_KIND_ORDER.iter() {
        let sent_summary = metrics.sent_messages_by_kind.get(kind);
        let size_summary = metrics.message_size_by_kind.get(kind);
        let sent = sent_summary
            .map(format_count_summary)
            .unwrap_or_else(|| "no samples".to_string());
        let size = size_summary
            .map(format_size_summary)
            .unwrap_or_else(|| "no samples".to_string());
        lines.push(Line::from(Span::raw(format!(
            "{}: sent {} | size {}",
            message_kind_label(kind),
            sent,
            size
        ))));
    }

    let max_lines = height.saturating_sub(2) as usize;
    if lines.len() > max_lines {
        lines.truncate(max_lines);
    }
    lines
}

fn format_count_summary(summary: &CountSummary) -> String {
    let Some(average) = summary.average else {
        return "no samples".to_string();
    };
    let min = summary.min.unwrap_or(0);
    let max = summary.max.unwrap_or(0);
    format!("min {min} / avg {average:.2} / max {max}")
}

fn format_size_summary(summary: &SizeSummary) -> String {
    let Some(average) = summary.average else {
        return "no samples".to_string();
    };
    let min = summary.min.unwrap_or(0);
    let max = summary.max.unwrap_or(0);
    format!("min {min} bytes / avg {average:.2} bytes / max {max} bytes")
}

fn message_kind_label(kind: &MessageKind) -> &'static str {
    match kind {
        MessageKind::Public => "public",
        MessageKind::Meta => "meta",
        MessageKind::Direct => "direct",
        MessageKind::FriendGroup => "friend_group",
        MessageKind::StoreForPeer => "store_for_peer",
    }
}

fn handle_key_event(
    key: KeyCode,
    input_mode: &mut InputMode,
    status_message: &mut String,
    control_tx: &mpsc::UnboundedSender<SimulationControlCommand>,
    auto_peer_index: &mut usize,
    _total_steps: usize,
    speed_step: f64,
    paused: &mut bool,
    log_scroll: &mut Option<usize>,
    log_view_start: usize,
) -> InputAction {
    if key == KeyCode::Char('q') {
        return InputAction::Quit;
    }
    match input_mode {
        InputMode::Normal => match key {
            KeyCode::Up => {
                *log_scroll = Some(log_view_start.saturating_sub(1));
            }
            KeyCode::Down => {
                if let Some(n) = *log_scroll {
                    // render will revert to None (auto-tail) if this reaches max_start
                    *log_scroll = Some(n.saturating_add(1));
                }
            }
            KeyCode::PageUp => {
                *log_scroll = Some(log_view_start.saturating_sub(10));
            }
            KeyCode::PageDown => {
                if let Some(n) = *log_scroll {
                    *log_scroll = Some(n.saturating_add(10));
                }
            }
            KeyCode::Char('a') => {
                *input_mode = InputMode::AddPeer {
                    buffer: String::new(),
                };
                *status_message =
                    "Add peer: enter id (or press Enter for auto-generated id).".to_string();
            }
            KeyCode::Char('f') => {
                *input_mode = InputMode::AddFriend {
                    buffer: String::new(),
                };
                *status_message =
                    "Add friends: enter two ids separated by space or comma.".to_string();
            }
            KeyCode::Char('+') | KeyCode::Char('=') => {
                let _ = control_tx
                    .send(SimulationControlCommand::AdjustSpeedFactor { delta: speed_step });
                *status_message = format!("Speed increased (+{speed_step:.2}).");
            }
            KeyCode::Char('-') => {
                let _ = control_tx
                    .send(SimulationControlCommand::AdjustSpeedFactor { delta: -speed_step });
                *status_message = format!("Speed decreased (-{speed_step:.2}).");
            }
            KeyCode::Char(' ') => {
                *paused = !*paused;
                let _ = control_tx.send(SimulationControlCommand::SetPaused { paused: *paused });
                if *paused {
                    *status_message = "Simulation paused. Press space to resume.".to_string();
                } else {
                    *status_message = "Simulation resumed.".to_string();
                }
            }
            KeyCode::Char('F') => {
                use tenet::simulation::TimeControlMode;
                let _ = control_tx.send(SimulationControlCommand::SetTimeControlMode {
                    mode: TimeControlMode::FastForward,
                });
                *paused = false;
                *status_message = "Switched to fast-forward mode.".to_string();
            }
            KeyCode::Char('R') => {
                use tenet::simulation::TimeControlMode;
                let _ = control_tx.send(SimulationControlCommand::SetTimeControlMode {
                    mode: TimeControlMode::RealTime { speed_factor: 1.0 },
                });
                *paused = false;
                *status_message = "Switched to real-time mode (1.0x speed).".to_string();
            }
            KeyCode::Char('P') => {
                use tenet::simulation::TimeControlMode;
                let _ = control_tx.send(SimulationControlCommand::SetTimeControlMode {
                    mode: TimeControlMode::Paused,
                });
                *paused = true;
                *status_message = "Simulation paused.".to_string();
            }
            _ => {}
        },
        InputMode::AddPeer { buffer } => match key {
            KeyCode::Esc => {
                *input_mode = InputMode::Normal;
                *status_message = "Add peer cancelled.".to_string();
            }
            KeyCode::Backspace => {
                buffer.pop();
                *status_message = format!("Add peer id: {buffer}");
            }
            KeyCode::Enter => {
                let trimmed = buffer.trim();
                let node_id = if trimmed.is_empty() {
                    let id = format!("peer-{}", auto_peer_index);
                    *auto_peer_index += 1;
                    id
                } else {
                    trimmed.to_string()
                };
                // For event-based simulation, create client with empty schedule
                let client = SimulationClient::new(&node_id, vec![], None);
                let keypair = generate_keypair();
                let _ = control_tx.send(SimulationControlCommand::AddPeer { client, keypair });
                *input_mode = InputMode::Normal;
                *status_message = format!("Added peer request queued for {node_id}.");
            }
            KeyCode::Char(c) => {
                buffer.push(c);
                *status_message = format!("Add peer id: {buffer}");
            }
            _ => {}
        },
        InputMode::AddFriend { buffer } => match key {
            KeyCode::Esc => {
                *input_mode = InputMode::Normal;
                *status_message = "Add friends cancelled.".to_string();
            }
            KeyCode::Backspace => {
                buffer.pop();
                *status_message = format!("Add friends: {buffer}");
            }
            KeyCode::Enter => {
                let peers: Vec<&str> = buffer
                    .split(|c: char| c == ',' || c.is_whitespace())
                    .filter(|id| !id.is_empty())
                    .collect();
                if peers.len() < 2 {
                    *status_message =
                        "Add friends needs two ids separated by space or comma.".to_string();
                    return InputAction::None;
                }
                let peer_a = peers[0].to_string();
                let peer_b = peers[1].to_string();
                let _ = control_tx.send(SimulationControlCommand::AddFriendship {
                    peer_a: peer_a.clone(),
                    peer_b: peer_b.clone(),
                });
                *input_mode = InputMode::Normal;
                *status_message = format!("Friendship request queued for {peer_a} and {peer_b}.");
            }
            KeyCode::Char(c) => {
                buffer.push(c);
                *status_message = format!("Add friends: {buffer}");
            }
            _ => {}
        },
    }
    InputAction::None
}
