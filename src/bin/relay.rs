use std::env;
use std::time::Duration;

use tenet_crypto::relay::{app, RelayConfig, RelayState};

#[tokio::main]
async fn main() {
    let config = RelayConfig {
        ttl: Duration::from_secs(env_u64("TENET_RELAY_TTL_SECS", 86_400)),
        max_messages: env_usize("TENET_RELAY_MAX_MESSAGES", 1_000),
        max_bytes: env_usize("TENET_RELAY_MAX_BYTES", 5 * 1024 * 1024),
    };

    let state = RelayState::new(config);
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
