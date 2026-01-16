use std::env;
use std::time::Duration;

use tenet::relay::{app, RelayConfig, RelayState};

#[tokio::main]
async fn main() {
    let config = RelayConfig {
        ttl: Duration::from_secs(env_u64("TENET_RELAY_TTL_SECS", 86_400)),
        max_messages: env_usize("TENET_RELAY_MAX_MESSAGES", 1_000),
        max_bytes: env_usize("TENET_RELAY_MAX_BYTES", 5 * 1024 * 1024),
        retry_backoff: env_backoff("TENET_RELAY_RETRY_BACKOFF_MS").unwrap_or_else(|| {
            vec![
                Duration::from_millis(5),
                Duration::from_millis(25),
                Duration::from_millis(100),
            ]
        }),
        peer_log_window: Duration::from_secs(env_u64("TENET_RELAY_PEER_LOG_WINDOW_SECS", 60)),
        peer_log_interval: Duration::from_secs(env_u64("TENET_RELAY_PEER_LOG_INTERVAL_SECS", 30)),
        log_sink: None,
    };

    let state = RelayState::new(config);
    state.start_peer_log_task();
    let app = app(state);

    let bind = env::var("TENET_RELAY_BIND").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .unwrap_or_else(|error| panic!("failed to bind {bind}: {error}"));

    axum::serve(listener, app)
        .await
        .unwrap_or_else(|error| panic!("server error: {error}"));
}

fn env_u64(key: &str, default_value: u64) -> u64 {
    env::var(key)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default_value)
}

fn env_usize(key: &str, default_value: usize) -> usize {
    env::var(key)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default_value)
}

fn env_backoff(key: &str) -> Option<Vec<Duration>> {
    let value = env::var(key).ok()?;
    let mut delays = Vec::new();
    for entry in value.split(',') {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(ms) = trimmed.parse::<u64>() {
            delays.push(Duration::from_millis(ms));
        }
    }
    if delays.is_empty() {
        None
    } else {
        Some(delays)
    }
}
