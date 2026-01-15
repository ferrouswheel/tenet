use std::collections::{HashMap, VecDeque};
use std::env;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Clone)]
struct RelayConfig {
    ttl: Duration,
    max_messages: usize,
    max_bytes: usize,
}

#[derive(Clone)]
struct RelayState {
    config: RelayConfig,
    queues: Arc<Mutex<HashMap<String, VecDeque<StoredEnvelope>>>>,
}

struct StoredEnvelope {
    body: Value,
    size_bytes: usize,
    expires_at: Instant,
}

#[derive(Deserialize)]
struct InboxQuery {
    limit: Option<usize>,
}

#[derive(Serialize)]
struct StoreResponse {
    message_id: String,
}

#[tokio::main]
async fn main() {
    let config = RelayConfig {
        ttl: Duration::from_secs(env_u64("TENET_RELAY_TTL_SECS", 86_400)),
        max_messages: env_usize("TENET_RELAY_MAX_MESSAGES", 1_000),
        max_bytes: env_usize("TENET_RELAY_MAX_BYTES", 5 * 1024 * 1024),
    };

    let state = RelayState {
        config,
        queues: Arc::new(Mutex::new(HashMap::new())),
    };

    let app = Router::new()
        .route("/health", get(healthcheck))
        .route("/envelopes", post(store_envelope))
        .route("/inbox/:recipient_id", get(fetch_inbox))
        .with_state(state);

    let bind = env::var("TENET_RELAY_BIND").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .unwrap_or_else(|error| panic!("failed to bind {bind}: {error}"));

    axum::serve(listener, app)
        .await
        .unwrap_or_else(|error| panic!("server error: {error}"));
}

async fn healthcheck() -> impl IntoResponse {
    StatusCode::OK
}

async fn store_envelope(
    State(state): State<RelayState>,
    Json(payload): Json<Value>,
) -> impl IntoResponse {
    let recipient_id = match recipient_id_from(&payload) {
        Some(value) => value,
        None => return (StatusCode::BAD_REQUEST, "missing recipient_id").into_response(),
    };

    let body_bytes = match serde_json::to_vec(&payload) {
        Ok(bytes) => bytes,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid json").into_response(),
    };
    let size_bytes = body_bytes.len();
    if size_bytes > state.config.max_bytes {
        return (StatusCode::PAYLOAD_TOO_LARGE, "payload exceeds max size").into_response();
    }

    let message_id = message_id_from(&payload, &body_bytes);
    let stored = StoredEnvelope {
        body: payload,
        size_bytes,
        expires_at: Instant::now() + state.config.ttl,
    };

    let mut queues = state
        .queues
        .lock()
        .expect("relay queue lock poisoned");
    let queue = queues.entry(recipient_id).or_insert_with(VecDeque::new);
    prune_expired(queue);

    while queue.len() >= state.config.max_messages || queue_bytes(queue) + size_bytes > state.config.max_bytes
    {
        queue.pop_front();
    }

    queue.push_back(stored);

    (StatusCode::ACCEPTED, Json(StoreResponse { message_id })).into_response()
}

async fn fetch_inbox(
    State(state): State<RelayState>,
    Path(recipient_id): Path<String>,
    Query(query): Query<InboxQuery>,
) -> impl IntoResponse {
    let mut queues = state
        .queues
        .lock()
        .expect("relay queue lock poisoned");
    let queue = match queues.get_mut(&recipient_id) {
        Some(queue) => queue,
        None => return (StatusCode::OK, Json(Vec::<Value>::new())).into_response(),
    };

    prune_expired(queue);
    let limit = query.limit.unwrap_or(queue.len());
    let mut envelopes = Vec::new();

    for _ in 0..limit.min(queue.len()) {
        if let Some(item) = queue.pop_front() {
            envelopes.push(item.body);
        }
    }

    (StatusCode::OK, Json(envelopes)).into_response()
}

fn recipient_id_from(value: &Value) -> Option<String> {
    value
        .pointer("/header/recipient_id")
        .and_then(|value| value.as_str())
        .map(str::to_string)
}

fn message_id_from(value: &Value, body_bytes: &[u8]) -> String {
    if let Some(id) = value
        .pointer("/message_id")
        .and_then(|value| value.as_str())
    {
        return id.to_string();
    }

    let digest = Sha256::digest(body_bytes);
    hex::encode(digest)
}

fn prune_expired(queue: &mut VecDeque<StoredEnvelope>) {
    let now = Instant::now();
    while matches!(queue.front(), Some(item) if item.expires_at <= now) {
        queue.pop_front();
    }
}

fn queue_bytes(queue: &VecDeque<StoredEnvelope>) -> usize {
    queue.iter().map(|item| item.size_bytes).sum()
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
