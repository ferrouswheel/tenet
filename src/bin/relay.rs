use std::env;
use std::net::SocketAddr;
use std::time::Duration;

use tenet::relay::{app, RelayConfig, RelayQosConfig, RelayState, TierLimits};

#[tokio::main]
async fn main() {
    tenet::logging::init();
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
        pause_flag: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        qos: RelayQosConfig {
            tier1: TierLimits {
                max_envelopes: env_usize("TENET_QOS_TIER1_MAX_ENVELOPES", 20),
                max_bytes: env_usize("TENET_QOS_TIER1_MAX_BYTES", 256 * 1024),
            },
            tier2: TierLimits {
                max_envelopes: env_usize("TENET_QOS_TIER2_MAX_ENVELOPES", 100),
                max_bytes: env_usize("TENET_QOS_TIER2_MAX_BYTES", 2 * 1024 * 1024),
            },
            tier3: TierLimits {
                max_envelopes: env_usize("TENET_QOS_TIER3_MAX_ENVELOPES", 500),
                max_bytes: env_usize("TENET_QOS_TIER3_MAX_BYTES", 10 * 1024 * 1024),
            },
            bidirectional_window: Duration::from_secs(env_u64(
                "TENET_QOS_BIDIRECTIONAL_WINDOW_SECS",
                7 * 24 * 3600,
            )),
        },
        blob_max_chunk_bytes: env_usize("TENET_BLOB_MAX_CHUNK_BYTES", 512 * 1024),
        blob_daily_quota_bytes: env_u64("TENET_BLOB_DAILY_QUOTA_BYTES", 500 * 1024 * 1024),
    };

    let state = RelayState::new(config);
    let (_shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    state.start_peer_log_task(shutdown_rx);
    let app = app(state);

    let bind = env::var("TENET_RELAY_BIND").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .unwrap_or_else(|error| panic!("failed to bind {bind}: {error}"));

    let local_addr = listener.local_addr().expect("failed to get local address");
    tenet::tlog!("tenet-relay listening on http://{}", local_addr);

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
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
