use std::collections::{HashMap, VecDeque};
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
pub struct RelayConfig {
    pub ttl: Duration,
    pub max_messages: usize,
    pub max_bytes: usize,
}

#[derive(Clone)]
pub struct RelayState {
    config: RelayConfig,
    inner: Arc<Mutex<RelayStateInner>>,
}

struct RelayStateInner {
    queues: HashMap<String, VecDeque<StoredEnvelope>>,
    dedup: HashMap<String, Instant>,
}

struct StoredEnvelope {
    body: Value,
    size_bytes: usize,
    expires_at: Instant,
    message_id: String,
}

#[derive(Deserialize)]
struct InboxQuery {
    limit: Option<usize>,
}

#[derive(Serialize)]
struct StoreResponse {
    message_id: String,
}

pub fn app(state: RelayState) -> Router {
    Router::new()
        .route("/health", get(healthcheck))
        .route("/envelopes", post(store_envelope))
        .route("/inbox/:recipient_id", get(fetch_inbox))
        .with_state(state)
}

impl RelayState {
    pub fn new(config: RelayConfig) -> Self {
        Self {
            config,
            inner: Arc::new(Mutex::new(RelayStateInner {
                queues: HashMap::new(),
                dedup: HashMap::new(),
            })),
        }
    }
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
    let now = Instant::now();
    let expires_at = now + state.config.ttl;

    let stored = StoredEnvelope {
        body: payload,
        size_bytes,
        expires_at,
        message_id: message_id.clone(),
    };

    let mut inner = state.inner.lock().expect("relay queue lock poisoned");
    prune_dedup(&mut inner.dedup);
    if inner
        .dedup
        .get(&message_id)
        .is_some_and(|entry_expires| *entry_expires > now)
    {
        return (StatusCode::ACCEPTED, Json(StoreResponse { message_id })).into_response();
    }

    inner.dedup.insert(message_id.clone(), expires_at);

    let mut evicted_ids = Vec::new();
    {
        let queue = inner
            .queues
            .entry(recipient_id)
            .or_insert_with(VecDeque::new);
        evicted_ids.extend(prune_expired(queue));

        while queue.len() >= state.config.max_messages
            || queue_bytes(queue) + size_bytes > state.config.max_bytes
        {
            if let Some(evicted) = queue.pop_front() {
                evicted_ids.push(evicted.message_id);
            }
        }

        queue.push_back(stored);
    }

    for message_id in evicted_ids {
        inner.dedup.remove(&message_id);
    }

    (StatusCode::ACCEPTED, Json(StoreResponse { message_id })).into_response()
}

async fn fetch_inbox(
    State(state): State<RelayState>,
    Path(recipient_id): Path<String>,
    Query(query): Query<InboxQuery>,
) -> impl IntoResponse {
    let mut inner = state.inner.lock().expect("relay queue lock poisoned");
    let (expired_ids, envelopes) = match inner.queues.get_mut(&recipient_id) {
        Some(queue) => {
            let expired_ids = prune_expired(queue);
            let limit = query.limit.unwrap_or(queue.len());
            let mut envelopes = Vec::new();

            for _ in 0..limit.min(queue.len()) {
                if let Some(item) = queue.pop_front() {
                    envelopes.push(item.body);
                }
            }

            (expired_ids, envelopes)
        }
        None => return (StatusCode::OK, Json(Vec::<Value>::new())).into_response(),
    };

    for message_id in expired_ids {
        inner.dedup.remove(&message_id);
    }
    prune_dedup(&mut inner.dedup);

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

fn prune_expired(queue: &mut VecDeque<StoredEnvelope>) -> Vec<String> {
    let now = Instant::now();
    let mut expired_ids = Vec::new();
    while matches!(queue.front(), Some(item) if item.expires_at <= now) {
        if let Some(expired) = queue.pop_front() {
            expired_ids.push(expired.message_id);
        }
    }
    expired_ids
}

fn prune_dedup(dedup: &mut HashMap<String, Instant>) {
    let now = Instant::now();
    dedup.retain(|_, expires_at| *expires_at > now);
}

fn queue_bytes(queue: &VecDeque<StoredEnvelope>) -> usize {
    queue.iter().map(|item| item.size_bytes).sum()
}
