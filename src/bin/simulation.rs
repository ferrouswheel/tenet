use std::collections::VecDeque;
use std::env;
use std::fs;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
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
    CountSummary, RollingLatencySnapshot, SimulationAggregateMetrics, SimulationClient,
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
    const RELAY_LOG_LIMIT: usize = 200;
    const SPEED_STEP: f64 = 0.10;
    enable_raw_mode().map_err(|err| err.to_string())?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).map_err(|err| err.to_string())?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).map_err(|err| err.to_string())?;

    let (tx, mut rx) = mpsc::unbounded_channel();
    let (control_tx, control_rx) = mpsc::unbounded_channel();
    let (relay_log_tx, mut relay_log_rx) = mpsc::unbounded_channel();
    let (sim_log_tx, mut sim_log_rx) = mpsc::unbounded_channel();
    let (input_tx, mut input_rx) = mpsc::unbounded_channel();
    let mut relay_config = scenario.relay.clone().into_relay_config();
    relay_config.log_sink = Some(Arc::new(move |line: String| {
        let _ = relay_log_tx.send(line);
    }));
    let scenario_for_task = scenario.clone();
    // For progress display, compute equivalent total steps from duration
    let total_steps = scenario_for_task.simulation.effective_steps();
    let sim_handle = tokio::spawn(async move {
        tenet::simulation::run_event_based_scenario_with_tui(
            scenario_for_task,
            control_rx,
            relay_config,
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

    let mut last_update: Option<SimulationStepUpdate> = None;
    let mut relay_logs: VecDeque<String> = VecDeque::with_capacity(RELAY_LOG_LIMIT);
    let mut sim_logs: VecDeque<String> = VecDeque::with_capacity(RELAY_LOG_LIMIT);
    let mut input_mode = InputMode::Normal;
    let mut status_message = "Simulation running.".to_string();
    let mut auto_peer_index = 1usize;
    let mut quit_requested = false;
    let mut paused = false;
    let base_seconds_per_step = scenario.simulation.simulated_time.seconds_per_step as f64;

    loop {
        tokio::select! {
            update = rx.recv() => {
                match update {
                    Some(update) => {
                        last_update = Some(update.clone());
                        render(
                            &mut terminal,
                            &update,
                            sim_logs.make_contiguous(),
                            relay_logs.make_contiguous(),
                            &status_message,
                            paused,
                            base_seconds_per_step,
                        )
                        .map_err(|err| err.to_string())?;
                    }
                    None => break,
                }
            }
            sim_log_line = sim_log_rx.recv() => {
                if let Some(line) = sim_log_line {
                    if sim_logs.len() == RELAY_LOG_LIMIT {
                        sim_logs.pop_front();
                    }
                    sim_logs.push_back(line);
                    if let Some(update) = &last_update {
                        render(
                            &mut terminal,
                            update,
                            sim_logs.make_contiguous(),
                            relay_logs.make_contiguous(),
                            &status_message,
                            paused,
                            base_seconds_per_step,
                        )
                        .map_err(|err| err.to_string())?;
                    }
                } else if rx.is_closed() {
                    break;
                }
            }
            relay_log_line = relay_log_rx.recv() => {
                if let Some(line) = relay_log_line {
                    if relay_logs.len() == RELAY_LOG_LIMIT {
                        relay_logs.pop_front();
                    }
                    relay_logs.push_back(line);
                    if let Some(update) = &last_update {
                        render(
                            &mut terminal,
                            update,
                            sim_logs.make_contiguous(),
                            relay_logs.make_contiguous(),
                            &status_message,
                            paused,
                            base_seconds_per_step,
                        )
                        .map_err(|err| err.to_string())?;
                    }
                } else if rx.is_closed() {
                    break;
                }
            }
            input_event = input_rx.recv() => {
                if let Some(Event::Key(key)) = input_event {
                    let action = handle_key_event(
                        key.code,
                        &mut input_mode,
                        &mut status_message,
                        &control_tx,
                        &mut auto_peer_index,
                        total_steps,
                        SPEED_STEP,
                        &mut paused,
                    );
                    if action == InputAction::Quit {
                        let _ = control_tx.send(SimulationControlCommand::Stop);
                        quit_requested = true;
                        break;
                    }
                    if let Some(update) = &last_update {
                        render(
                            &mut terminal,
                            update,
                            sim_logs.make_contiguous(),
                            relay_logs.make_contiguous(),
                            &status_message,
                            paused,
                            base_seconds_per_step,
                        )
                        .map_err(|err| err.to_string())?;
                    }
                }
            }
        }
    }

    if quit_requested {
        input_shutdown.store(true, Ordering::SeqCst);
        let _ = input_handle.await;
        disable_raw_mode().map_err(|err| err.to_string())?;
        execute!(terminal.backend_mut(), LeaveAlternateScreen).map_err(|err| err.to_string())?;
        terminal.show_cursor().map_err(|err| err.to_string())?;
        return sim_handle
            .await
            .map_err(|err| format!("simulation task failed: {err}"))?;
    }

    let completion_update = last_update.unwrap_or_else(|| SimulationStepUpdate {
        step: total_steps,
        total_steps,
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
    });
    render(
        &mut terminal,
        &completion_update,
        sim_logs.make_contiguous(),
        relay_logs.make_contiguous(),
        "Simulation complete. Press q to exit.",
        paused,
        base_seconds_per_step,
    )
    .map_err(|err| err.to_string())?;
    wait_for_quit(&mut input_rx)
        .await
        .map_err(|err| err.to_string())?;
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
    sim_logs: &[String],
    relay_logs: &[String],
    status: &str,
    paused: bool,
    base_seconds_per_step: f64,
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

            let sim_seconds_per_step = base_seconds_per_step * update.speed_factor.max(0.0);
            let current_sim_seconds = update.step as f64 * sim_seconds_per_step;
            let total_sim_seconds = update.total_steps as f64 * sim_seconds_per_step;
            let progress = update.step as f64 / update.total_steps.max(1) as f64;
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

            let metrics_lines = vec![
                Line::from(Span::raw(format!("Total peers: {}", update.total_peers))),
                Line::from(Span::raw(format!("Online peers: {}", update.online_nodes))),
                Line::from(Span::raw(format!(
                    "Speed factor: {:.2}x",
                    update.speed_factor
                ))),
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

            let log_chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(chunks[3]);

            let sim_lines = sim_logs
                .iter()
                .rev()
                .take(log_chunks[0].height.saturating_sub(2) as usize)
                .cloned()
                .collect::<Vec<_>>();
            let sim_lines = sim_lines
                .into_iter()
                .rev()
                .map(Line::from)
                .collect::<Vec<_>>();
            let sim_block = Paragraph::new(sim_lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Simulation Logs"),
            );
            frame.render_widget(sim_block, log_chunks[0]);

            let relay_lines = relay_logs
                .iter()
                .rev()
                .take(log_chunks[1].height.saturating_sub(2) as usize)
                .cloned()
                .collect::<Vec<_>>();
            let relay_lines = relay_lines
                .into_iter()
                .rev()
                .map(Line::from)
                .collect::<Vec<_>>();
            let relay_block = Paragraph::new(relay_lines)
                .block(Block::default().borders(Borders::ALL).title("Relay Logs"));
            frame.render_widget(relay_block, log_chunks[1]);

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

async fn wait_for_quit(input_rx: &mut mpsc::UnboundedReceiver<Event>) -> io::Result<()> {
    loop {
        if let Some(Event::Key(key)) = input_rx.recv().await {
            if key.code == KeyCode::Char('q') {
                break;
            }
        }
    }
    Ok(())
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
) -> InputAction {
    if key == KeyCode::Char('q') {
        return InputAction::Quit;
    }
    match input_mode {
        InputMode::Normal => match key {
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
