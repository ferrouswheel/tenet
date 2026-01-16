use std::collections::VecDeque;
use std::env;
use std::fs;
use std::io;
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
use tenet::simulation::{
    run_simulation_scenario, RollingLatencySnapshot, SimulationControlCommand,
    SimulationScenarioConfig, SimulationStepUpdate,
};

#[derive(Debug)]
enum InputMode {
    Normal,
    AddPeer { buffer: String },
    AddFriend { buffer: String },
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
            return Err("usage: tenet-sim [--tui] <path-to-scenario.toml>".to_string());
        }
    }
    let path =
        path.ok_or_else(|| "usage: tenet-sim [--tui] <path-to-scenario.toml>".to_string())?;
    let contents = fs::read_to_string(&path).map_err(|err| err.to_string())?;
    let scenario: SimulationScenarioConfig =
        toml::from_str(&contents).map_err(|err| err.to_string())?;

    let report = if use_tui {
        run_with_tui(scenario).await?
    } else {
        run_simulation_scenario(scenario).await?
    };
    let output = serde_json::to_string_pretty(&report.metrics).map_err(|err| err.to_string())?;
    println!("{output}");
    Ok(())
}

async fn run_with_tui(
    scenario: SimulationScenarioConfig,
) -> Result<tenet::simulation::SimulationReport, String> {
    const RELAY_LOG_LIMIT: usize = 200;
    const REAL_TIME_STEP_DELAY_MS: u64 = 100;
    const SPEED_UP_FACTOR: f64 = 1.25;
    const SLOW_DOWN_FACTOR: f64 = 0.8;
    enable_raw_mode().map_err(|err| err.to_string())?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).map_err(|err| err.to_string())?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).map_err(|err| err.to_string())?;

    let (tx, mut rx) = mpsc::unbounded_channel();
    let (control_tx, control_rx) = mpsc::unbounded_channel();
    let (log_tx, mut log_rx) = mpsc::unbounded_channel();
    let (input_tx, mut input_rx) = mpsc::unbounded_channel();
    let mut relay_config = scenario.relay.clone().into_relay_config();
    relay_config.log_sink = Some(Arc::new(move |line: String| {
        let _ = log_tx.send(line);
    }));
    let scenario_for_task = scenario.clone();
    let sim_handle = tokio::spawn(async move {
        let (base_url, shutdown_tx) = tenet::simulation::start_relay(relay_config).await;
        let mut inputs = tenet::simulation::build_simulation_inputs(&scenario_for_task.simulation);
        inputs.timing.base_real_time_per_step = Duration::from_millis(REAL_TIME_STEP_DELAY_MS);
        let mut harness = tenet::simulation::SimulationHarness::new(
            base_url,
            inputs.nodes,
            inputs.direct_links,
            scenario_for_task.direct_enabled.unwrap_or(true),
            scenario_for_task.relay.ttl_seconds,
            inputs.encryption,
            inputs.keypairs,
            inputs.timing,
            inputs.cohort_online_rates,
        );
        let metrics = harness
            .run_with_progress_and_controls(
                scenario_for_task.simulation.steps,
                inputs.planned_sends,
                control_rx,
                |update| {
                    let _ = tx.send(update);
                },
            )
            .await;
        let report = harness.metrics_report();
        shutdown_tx.send(()).ok();
        Ok(tenet::simulation::SimulationReport { metrics, report })
    });

    let input_handle = tokio::task::spawn_blocking(move || loop {
        match event::read() {
            Ok(event) => {
                if input_tx.send(event).is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    });

    let mut last_update: Option<SimulationStepUpdate> = None;
    let mut relay_logs: VecDeque<String> = VecDeque::with_capacity(RELAY_LOG_LIMIT);
    let mut input_mode = InputMode::Normal;
    let mut status_message =
        "Simulation running. Press a to add peers, f to add friends, +/- to adjust speed."
            .to_string();
    let mut auto_peer_index = 1usize;

    loop {
        tokio::select! {
            update = rx.recv() => {
                match update {
                    Some(update) => {
                        last_update = Some(update.clone());
                        render(
                            &mut terminal,
                            &update,
                            relay_logs.make_contiguous(),
                            &status_message,
                        )
                        .map_err(|err| err.to_string())?;
                    }
                    None => break,
                }
            }
            log_line = log_rx.recv() => {
                if let Some(line) = log_line {
                    if relay_logs.len() == RELAY_LOG_LIMIT {
                        relay_logs.pop_front();
                    }
                    relay_logs.push_back(line);
                    if let Some(update) = &last_update {
                        render(
                            &mut terminal,
                            update,
                            relay_logs.make_contiguous(),
                            &status_message,
                        )
                        .map_err(|err| err.to_string())?;
                    }
                } else if rx.is_closed() {
                    break;
                }
            }
            input_event = input_rx.recv() => {
                if let Some(Event::Key(key)) = input_event {
                    handle_key_event(
                        key.code,
                        &mut input_mode,
                        &mut status_message,
                        &control_tx,
                        &mut auto_peer_index,
                        scenario.simulation.steps,
                        SPEED_UP_FACTOR,
                        SLOW_DOWN_FACTOR,
                    );
                    if let Some(update) = &last_update {
                        render(
                            &mut terminal,
                            update,
                            relay_logs.make_contiguous(),
                            &status_message,
                        )
                        .map_err(|err| err.to_string())?;
                    }
                }
            }
        }
    }

    let completion_update = last_update.unwrap_or_else(|| SimulationStepUpdate {
        step: scenario.simulation.steps,
        total_steps: scenario.simulation.steps,
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
    });
    render(
        &mut terminal,
        &completion_update,
        relay_logs.make_contiguous(),
        "Simulation complete. Press q to exit.",
    )
    .map_err(|err| err.to_string())?;
    wait_for_quit(&mut input_rx)
        .await
        .map_err(|err| err.to_string())?;
    input_handle.abort();

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
    relay_logs: &[String],
    status: &str,
) -> io::Result<()> {
    terminal
        .draw(|frame| {
            let size = frame.size();
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .margin(1)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Length(8),
                    Constraint::Min(8),
                    Constraint::Length(3),
                ])
                .split(size);

            let progress = update.step as f64 / update.total_steps.max(1) as f64;
            let progress_gauge = Gauge::default()
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Simulation Progress"),
                )
                .gauge_style(Style::default().fg(Color::Cyan))
                .ratio(progress)
                .label(format!("Step {}/{}", update.step, update.total_steps));
            frame.render_widget(progress_gauge, chunks[0]);

            let metrics_lines = vec![
                Line::from(Span::raw(format!("Total peers: {}", update.total_peers))),
                Line::from(Span::raw(format!("Online nodes: {}", update.online_nodes))),
                Line::from(Span::raw(format!(
                    "Speed factor: {:.2}x",
                    update.speed_factor
                ))),
                Line::from(Span::raw(format!(
                    "Messages sent (step): {}",
                    update.sent_messages
                ))),
                Line::from(Span::raw(format!(
                    "Messages received (step): {}",
                    update.received_messages
                ))),
                Line::from(Span::raw(format!(
                    "Rolling latency (last {}): {}",
                    update.rolling_latency.window,
                    format_latency(&update.rolling_latency)
                ))),
            ];
            let metrics_block = Paragraph::new(metrics_lines)
                .block(Block::default().borders(Borders::ALL).title("Step Metrics"));
            frame.render_widget(metrics_block, chunks[1]);

            let relay_lines = relay_logs
                .iter()
                .rev()
                .take(chunks[2].height.saturating_sub(2) as usize)
                .cloned()
                .collect::<Vec<_>>();
            let relay_lines = relay_lines
                .into_iter()
                .rev()
                .map(Line::from)
                .collect::<Vec<_>>();
            let relay_block = Paragraph::new(relay_lines)
                .block(Block::default().borders(Borders::ALL).title("Relay Logs"));
            frame.render_widget(relay_block, chunks[2]);

            let hint = Paragraph::new(status)
                .block(Block::default().borders(Borders::ALL).title("Status"));
            frame.render_widget(hint, chunks[3]);
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

fn handle_key_event(
    key: KeyCode,
    input_mode: &mut InputMode,
    status_message: &mut String,
    control_tx: &mpsc::UnboundedSender<SimulationControlCommand>,
    auto_peer_index: &mut usize,
    total_steps: usize,
    speed_up_factor: f64,
    slow_down_factor: f64,
) {
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
                let _ = control_tx.send(SimulationControlCommand::AdjustSpeedFactor {
                    multiplier: speed_up_factor,
                });
                *status_message = format!("Speeding up (x{speed_up_factor:.2}).");
            }
            KeyCode::Char('-') => {
                let _ = control_tx.send(SimulationControlCommand::AdjustSpeedFactor {
                    multiplier: slow_down_factor,
                });
                *status_message = format!("Slowing down (x{slow_down_factor:.2}).");
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
                let schedule = vec![true; total_steps];
                let node = tenet::simulation::Node::new(&node_id, schedule);
                let keypair = generate_keypair();
                let _ = control_tx.send(SimulationControlCommand::AddPeer { node, keypair });
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
                    return;
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
}
