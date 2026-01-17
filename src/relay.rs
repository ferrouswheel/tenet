use std::collections::{HashMap, VecDeque};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
use tokio::sync::{Mutex, MutexGuard};
use tokio::time::sleep;

#[derive(Clone)]
pub struct RelayConfig {
    pub ttl: Duration,
    pub max_messages: usize,
    pub max_bytes: usize,
    pub retry_backoff: Vec<Duration>,
    pub peer_log_window: Duration,
    pub peer_log_interval: Duration,
    pub log_sink: Option<Arc<dyn Fn(String) + Send + Sync>>,
    pub pause_flag: Arc<AtomicBool>,
}

#[derive(Clone)]
pub struct RelayState {
    config: RelayConfig,
    inner: Arc<Mutex<RelayStateInner>>,
}

struct RelayStateInner {
    queues: HashMap<String, VecDeque<StoredEnvelope>>,
    dedup: HashMap<String, Instant>,
    peer_last_seen: HashMap<String, Instant>,
}

#[derive(Clone)]
pub struct RelayControl {
    pause_flag: Arc<AtomicBool>,
}

impl RelayControl {
    pub fn new(pause_flag: Arc<AtomicBool>) -> Self {
        Self { pause_flag }
    }

    pub fn set_paused(&self, paused: bool) {
        self.pause_flag.store(paused, Ordering::SeqCst);
    }
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

#[derive(Deserialize)]
struct BatchInboxRequest {
    recipient_ids: Vec<String>,
    limit: Option<usize>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum BatchStoreStatus {
    Accepted,
    Duplicate,
    Expired,
    TooLarge,
    Invalid,
}

#[derive(Serialize)]
struct BatchStoreResult {
    message_id: Option<String>,
    status: BatchStoreStatus,
    detail: Option<String>,
}

pub fn app(state: RelayState) -> Router {
    Router::new()
        .route("/health", get(healthcheck))
        .route("/envelopes", post(store_envelope))
        .route("/envelopes/batch", post(store_envelopes_batch))
        .route("/inbox/:recipient_id", get(fetch_inbox))
        .route("/inbox/batch", post(fetch_inbox_batch))
        .with_state(state)
}

impl RelayState {
    pub fn new(config: RelayConfig) -> Self {
        Self {
            config,
            inner: Arc::new(Mutex::new(RelayStateInner {
                queues: HashMap::new(),
                dedup: HashMap::new(),
                peer_last_seen: HashMap::new(),
            })),
        }
    }

    pub fn start_peer_log_task(&self) {
        if self.config.peer_log_interval.is_zero() {
            return;
        }
        let state = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(state.config.peer_log_interval);
            loop {
                ticker.tick().await;
                let now = Instant::now();
                let count = {
                    let inner = state.inner.lock().await;
                    count_recent_peers(&inner, now, state.config.peer_log_window)
                };
                log_message(
                    &state.config,
                    format!(
                        "relay: {} peers connected in last {}s",
                        count,
                        state.config.peer_log_window.as_secs()
                    ),
                );
            }
        });
    }
}

async fn healthcheck() -> impl IntoResponse {
    StatusCode::OK
}

async fn store_envelope(
    State(state): State<RelayState>,
    Json(payload): Json<Value>,
) -> impl IntoResponse {
    let mut inner = match lock_with_retry(&state).await {
        Ok(inner) => inner,
        Err(status) => return (status, "relay busy").into_response(),
    };

    let now = Instant::now();
    let now_epoch = SystemTime::now();
    match store_envelope_locked(&state.config, &mut inner, payload, now, now_epoch) {
        Ok(StoreDecision::Accepted { message_id })
        | Ok(StoreDecision::Duplicate { message_id }) => {
            (StatusCode::ACCEPTED, Json(StoreResponse { message_id })).into_response()
        }
        Ok(StoreDecision::Expired { .. }) => (StatusCode::GONE, "envelope expired").into_response(),
        Ok(StoreDecision::TooLarge { .. }) => {
            (StatusCode::PAYLOAD_TOO_LARGE, "payload exceeds max size").into_response()
        }
        Err(StoreError::MissingRecipient) => {
            (StatusCode::BAD_REQUEST, "missing recipient_id").into_response()
        }
        Err(StoreError::InvalidPayload(detail)) => {
            (StatusCode::BAD_REQUEST, detail).into_response()
        }
    }
}

async fn store_envelopes_batch(
    State(state): State<RelayState>,
    Json(payloads): Json<Vec<Value>>,
) -> impl IntoResponse {
    let mut inner = match lock_with_retry(&state).await {
        Ok(inner) => inner,
        Err(status) => return (status, "relay busy").into_response(),
    };

    let now = Instant::now();
    let now_epoch = SystemTime::now();
    let results = payloads
        .into_iter()
        .map(|payload| {
            match store_envelope_locked(&state.config, &mut inner, payload, now, now_epoch) {
                Ok(StoreDecision::Accepted { message_id }) => BatchStoreResult {
                    message_id: Some(message_id),
                    status: BatchStoreStatus::Accepted,
                    detail: None,
                },
                Ok(StoreDecision::Duplicate { message_id }) => BatchStoreResult {
                    message_id: Some(message_id),
                    status: BatchStoreStatus::Duplicate,
                    detail: None,
                },
                Ok(StoreDecision::Expired { message_id }) => BatchStoreResult {
                    message_id: Some(message_id),
                    status: BatchStoreStatus::Expired,
                    detail: None,
                },
                Ok(StoreDecision::TooLarge { message_id }) => BatchStoreResult {
                    message_id: Some(message_id),
                    status: BatchStoreStatus::TooLarge,
                    detail: None,
                },
                Err(StoreError::MissingRecipient) => BatchStoreResult {
                    message_id: None,
                    status: BatchStoreStatus::Invalid,
                    detail: Some("missing recipient_id".to_string()),
                },
                Err(StoreError::InvalidPayload(detail)) => BatchStoreResult {
                    message_id: None,
                    status: BatchStoreStatus::Invalid,
                    detail: Some(detail),
                },
            }
        })
        .collect::<Vec<_>>();

    (StatusCode::ACCEPTED, Json(results)).into_response()
}

async fn fetch_inbox(
    State(state): State<RelayState>,
    Path(recipient_id): Path<String>,
    Query(query): Query<InboxQuery>,
) -> impl IntoResponse {
    let mut inner = match lock_with_retry(&state).await {
        Ok(inner) => inner,
        Err(status) => return (status, "relay busy").into_response(),
    };
    let now = Instant::now();
    record_peer_connection(&state.config, &mut inner, &recipient_id, now);
    let (expired_ids, envelopes) = match inner.queues.get_mut(&recipient_id) {
        Some(queue) => pop_envelopes(queue, query.limit, now),
        None => return (StatusCode::OK, Json(Vec::<Value>::new())).into_response(),
    };

    for message_id in expired_ids {
        inner.dedup.remove(&message_id);
    }
    prune_dedup(&mut inner.dedup);

    (StatusCode::OK, Json(envelopes)).into_response()
}

async fn fetch_inbox_batch(
    State(state): State<RelayState>,
    Json(request): Json<BatchInboxRequest>,
) -> impl IntoResponse {
    let mut inner = match lock_with_retry(&state).await {
        Ok(inner) => inner,
        Err(status) => return (status, "relay busy").into_response(),
    };
    let now = Instant::now();
    let mut expired_ids = Vec::new();
    let mut results = HashMap::new();

    for recipient_id in request.recipient_ids {
        record_peer_connection(&state.config, &mut inner, &recipient_id, now);
        let (expired, envelopes) = match inner.queues.get_mut(&recipient_id) {
            Some(queue) => pop_envelopes(queue, request.limit, now),
            None => (Vec::new(), Vec::new()),
        };
        expired_ids.extend(expired);
        results.insert(recipient_id, envelopes);
    }

    for message_id in expired_ids {
        inner.dedup.remove(&message_id);
    }
    prune_dedup(&mut inner.dedup);

    (StatusCode::OK, Json(results)).into_response()
}

fn recipient_id_from(value: &Value) -> Option<String> {
    value
        .pointer("/header/recipient_id")
        .and_then(|value| value.as_str())
        .map(str::to_string)
}

fn sender_id_from(value: &Value) -> Option<String> {
    value
        .pointer("/header/sender_id")
        .and_then(|value| value.as_str())
        .map(str::to_string)
}

fn message_id_from(value: &Value, body_bytes: &[u8]) -> String {
    if let Some(id) = value
        .pointer("/header/message_id")
        .and_then(|value| value.as_str())
    {
        return id.to_string();
    }
    if let Some(id) = value
        .pointer("/message_id")
        .and_then(|value| value.as_str())
    {
        return id.to_string();
    }

    let digest = Sha256::digest(body_bytes);
    hex::encode(digest)
}

fn pop_envelopes(
    queue: &mut VecDeque<StoredEnvelope>,
    limit: Option<usize>,
    now: Instant,
) -> (Vec<String>, Vec<Value>) {
    let expired_ids = prune_expired(queue, now);
    let limit = limit.unwrap_or(queue.len());
    let mut envelopes = Vec::new();

    for _ in 0..limit.min(queue.len()) {
        if let Some(item) = queue.pop_front() {
            envelopes.push(item.body);
        }
    }

    (expired_ids, envelopes)
}

fn prune_expired(queue: &mut VecDeque<StoredEnvelope>, now: Instant) -> Vec<String> {
    let mut expired_ids = Vec::new();
    queue.retain(|item| {
        if item.expires_at <= now {
            expired_ids.push(item.message_id.clone());
            false
        } else {
            true
        }
    });
    expired_ids
}

fn prune_dedup(dedup: &mut HashMap<String, Instant>) {
    let now = Instant::now();
    dedup.retain(|_, expires_at| *expires_at > now);
}

fn queue_bytes(queue: &VecDeque<StoredEnvelope>) -> usize {
    queue.iter().map(|item| item.size_bytes).sum()
}

async fn lock_with_retry(
    state: &RelayState,
) -> Result<MutexGuard<'_, RelayStateInner>, StatusCode> {
    if let Ok(inner) = state.inner.try_lock() {
        return Ok(inner);
    }

    for delay in &state.config.retry_backoff {
        sleep(*delay).await;
        if let Ok(inner) = state.inner.try_lock() {
            return Ok(inner);
        }
    }

    Err(StatusCode::SERVICE_UNAVAILABLE)
}

enum StoreDecision {
    Accepted { message_id: String },
    Duplicate { message_id: String },
    Expired { message_id: String },
    TooLarge { message_id: String },
}

enum StoreError {
    MissingRecipient,
    InvalidPayload(String),
}

fn store_envelope_locked(
    config: &RelayConfig,
    inner: &mut RelayStateInner,
    payload: Value,
    now: Instant,
    now_epoch: SystemTime,
) -> Result<StoreDecision, StoreError> {
    let recipient_id = recipient_id_from(&payload).ok_or(StoreError::MissingRecipient)?;
    let sender_id = sender_id_from(&payload);
    record_peer_connection(config, inner, &recipient_id, now);
    if let Some(sender_id) = sender_id.as_deref() {
        record_peer_connection(config, inner, sender_id, now);
    }

    let body_bytes = serde_json::to_vec(&payload)
        .map_err(|_| StoreError::InvalidPayload("invalid json".to_string()))?;
    let size_bytes = body_bytes.len();
    let message_id = message_id_from(&payload, &body_bytes);
    if size_bytes > config.max_bytes {
        return Ok(StoreDecision::TooLarge { message_id });
    }

    let expires_at = match expires_at_from(&payload, config, now, now_epoch)? {
        ExpiresAt::Expired => {
            return Ok(StoreDecision::Expired { message_id });
        }
        ExpiresAt::Valid(expires_at) => expires_at,
    };
    let stored = StoredEnvelope {
        body: payload,
        size_bytes,
        expires_at,
        message_id: message_id.clone(),
    };

    prune_dedup(&mut inner.dedup);
    if inner
        .dedup
        .get(&message_id)
        .is_some_and(|entry_expires| *entry_expires > now)
    {
        return Ok(StoreDecision::Duplicate { message_id });
    }

    inner.dedup.insert(message_id.clone(), expires_at);

    let mut evicted_ids = Vec::new();
    {
        let queue = inner
            .queues
            .entry(recipient_id.clone())
            .or_insert_with(VecDeque::new);
        evicted_ids.extend(prune_expired(queue, now));

        while queue.len() >= config.max_messages
            || queue_bytes(queue) + size_bytes > config.max_bytes
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

    if let Some(sender_id) = sender_id {
        log_message(
            config,
            format!(
                "relay: message sent {} -> {} (message_id: {})",
                format_peer_id(&sender_id),
                format_peer_id(&recipient_id),
                format_message_id(&message_id)
            ),
        );
    } else {
        log_message(
            config,
            format!(
                "relay: message sent -> {} (message_id: {})",
                format_peer_id(&recipient_id),
                format_message_id(&message_id)
            ),
        );
    }

    Ok(StoreDecision::Accepted { message_id })
}

fn expires_at_from(
    payload: &Value,
    config: &RelayConfig,
    now: Instant,
    now_epoch: SystemTime,
) -> Result<ExpiresAt, StoreError> {
    let ttl_seconds = payload
        .pointer("/header/ttl_seconds")
        .and_then(|value| value.as_u64());
    let timestamp = payload
        .pointer("/header/timestamp")
        .and_then(|value| value.as_u64());

    if let (Some(ttl_seconds), Some(timestamp)) = (ttl_seconds, timestamp) {
        if ttl_seconds == 0 {
            return Ok(ExpiresAt::Expired);
        }
        let expires_epoch = timestamp
            .checked_add(ttl_seconds)
            .ok_or_else(|| StoreError::InvalidPayload("ttl overflow".to_string()))?;
        let now_secs = now_epoch
            .duration_since(UNIX_EPOCH)
            .map_err(|_| StoreError::InvalidPayload("clock error".to_string()))?
            .as_secs();
        if expires_epoch <= now_secs {
            return Ok(ExpiresAt::Expired);
        }
        let remaining = Duration::from_secs(expires_epoch - now_secs);
        let capped = remaining.min(config.ttl);
        return Ok(ExpiresAt::Valid(now + capped));
    }

    if ttl_seconds.is_some() && timestamp.is_none() {
        return Err(StoreError::InvalidPayload("missing timestamp".to_string()));
    }

    Ok(ExpiresAt::Valid(now + config.ttl))
}

enum ExpiresAt {
    Expired,
    Valid(Instant),
}

fn record_peer_connection(
    config: &RelayConfig,
    inner: &mut RelayStateInner,
    peer_id: &str,
    now: Instant,
) {
    let should_log = match inner.peer_last_seen.get(peer_id) {
        Some(last_seen) => now.duration_since(*last_seen) >= config.peer_log_window,
        None => true,
    };
    inner.peer_last_seen.insert(peer_id.to_string(), now);
    if should_log {
        log_message(
            config,
            format!("relay: peer connected {}", format_peer_id(peer_id)),
        );
    }
}

fn log_message(config: &RelayConfig, message: String) {
    if config.pause_flag.load(Ordering::SeqCst) {
        return;
    }
    if let Some(log_sink) = &config.log_sink {
        log_sink(message);
    } else {
        println!("{message}");
    }
}

const LOG_ID_TRUNCATE_LEN: usize = 7;

fn format_peer_id(peer_id: &str) -> String {
    format!("p-{}", truncate_id(peer_id))
}

fn format_message_id(message_id: &str) -> String {
    format!("m-{}", truncate_id(message_id))
}

fn truncate_id(id: &str) -> String {
    id.chars().take(LOG_ID_TRUNCATE_LEN).collect()
}

fn count_recent_peers(inner: &RelayStateInner, now: Instant, window: Duration) -> usize {
    inner
        .peer_last_seen
        .values()
        .filter(|last_seen| now.duration_since(**last_seen) <= window)
        .count()
}
