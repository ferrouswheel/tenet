use std::env;
use std::fs;
use std::io;

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

use tenet_crypto::simulation::{
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
        run_simulation_scenario(scenario).await?
    };
    let output = serde_json::to_string_pretty(&report.metrics).map_err(|err| err.to_string())?;
    println!("{output}");
    Ok(())
}

async fn run_with_tui(
    scenario: SimulationScenarioConfig,
) -> Result<tenet_crypto::simulation::SimulationReport, String> {
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

    while let Some(update) = rx.recv().await {
        render(&mut terminal, &update).map_err(|err| err.to_string())?;
    }

    let result = sim_handle
        .await
        .map_err(|err| format!("simulation task failed: {err}"))?;

    disable_raw_mode().map_err(|err| err.to_string())?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen).map_err(|err| err.to_string())?;
    terminal.show_cursor().map_err(|err| err.to_string())?;

    result
}

fn render(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    update: &SimulationStepUpdate,
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

            let hint = Paragraph::new("Simulation running. Terminal will exit on completion.")
                .block(Block::default().borders(Borders::ALL).title("Status"));
            frame.render_widget(hint, chunks[2]);
        })
        .map(|_| ())
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
