use std::env;
use std::fs;

use tenet_crypto::simulation::{run_simulation_scenario, SimulationScenarioConfig};

#[tokio::main]
async fn main() -> Result<(), String> {
    let path = env::args()
        .nth(1)
        .ok_or_else(|| "usage: simulation <path-to-scenario.toml>".to_string())?;
    let contents = fs::read_to_string(&path).map_err(|err| err.to_string())?;
    let scenario: SimulationScenarioConfig =
        toml::from_str(&contents).map_err(|err| err.to_string())?;

    let report = run_simulation_scenario(scenario).await?;
    let output = serde_json::to_string_pretty(&report.metrics).map_err(|err| err.to_string())?;
    println!("{output}");
    Ok(())
}
