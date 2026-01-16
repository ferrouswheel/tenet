use std::env;
use std::fs;
use std::io;

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

use tenet::simulation::{
    run_simulation_scenario, run_simulation_scenario_with_progress, RollingLatencySnapshot,
    SimulationScenarioConfig, SimulationStepUpdate,
};

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
    enable_raw_mode().map_err(|err| err.to_string())?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).map_err(|err| err.to_string())?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).map_err(|err| err.to_string())?;

    let (tx, mut rx) = mpsc::unbounded_channel();
    let scenario_for_task = scenario.clone();
    let sim_handle = tokio::spawn(async move {
        run_simulation_scenario_with_progress(scenario_for_task, |update| {
            let _ = tx.send(update);
        })
        .await
    });

    let mut last_update: Option<SimulationStepUpdate> = None;
    while let Some(update) = rx.recv().await {
        render(
            &mut terminal,
            &update,
            "Simulation running. Terminal will wait for q on completion.",
        )
        .map_err(|err| err.to_string())?;
        last_update = Some(update);
    }

    let result = sim_handle
        .await
        .map_err(|err| format!("simulation task failed: {err}"))?;

    let completion_update = last_update.unwrap_or_else(|| SimulationStepUpdate {
        step: scenario.simulation.steps,
        total_steps: scenario.simulation.steps,
        online_nodes: 0,
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
        "Simulation complete. Press q to exit.",
    )
    .map_err(|err| err.to_string())?;
    wait_for_quit().map_err(|err| err.to_string())?;

    disable_raw_mode().map_err(|err| err.to_string())?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen).map_err(|err| err.to_string())?;
    terminal.show_cursor().map_err(|err| err.to_string())?;

    result
}

fn render(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    update: &SimulationStepUpdate,
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
                    Constraint::Length(6),
                    Constraint::Min(3),
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
                Line::from(Span::raw(format!("Online nodes: {}", update.online_nodes))),
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

            let hint = Paragraph::new(status)
                .block(Block::default().borders(Borders::ALL).title("Status"));
            frame.render_widget(hint, chunks[2]);
        })
        .map(|_| ())
}

fn wait_for_quit() -> io::Result<()> {
    loop {
        if let Event::Key(key) = event::read()? {
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
