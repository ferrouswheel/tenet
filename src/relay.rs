use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::{
    extract::{
        ws::{Message, WebSocket},
        ConnectInfo, Path, Query, State, WebSocketUpgrade,
    },
    http::StatusCode,
    response::{Html, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::{broadcast, Mutex, MutexGuard, RwLock};
use tokio::time::sleep;

const DASHBOARD_HTML: &str = include_str!("../web/src/relay_dashboard.html");

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

const WS_BROADCAST_CAPACITY: usize = 128;

#[derive(Clone)]
pub struct RelayState {
    config: RelayConfig,
    inner: Arc<Mutex<RelayStateInner>>,
    subscribers: Arc<RwLock<HashMap<String, broadcast::Sender<Value>>>>,
    start_time: Instant,
    ws_connections: Arc<AtomicUsize>,
}

struct RelayStateInner {
    queues: HashMap<String, VecDeque<StoredEnvelope>>,
    dedup: HashMap<String, Instant>,
    peer_last_seen: HashMap<String, Instant>,
    // Lifetime stats
    total_stored: u64,
    total_delivered: u64,
    latency_min_ms: u64,
    latency_max_ms: u64,
    latency_sum_ms: u64,
    latency_count: u64,
    // Rolling activity windows (pruned to 24 h)
    inbox_deliveries: HashMap<String, VecDeque<Instant>>,
    peer_sends: HashMap<String, VecDeque<Instant>>,
    // Active WebSocket connections
    ws_clients: HashMap<u64, WsClientInfo>,
    next_conn_id: u64,
}

struct WsClientInfo {
    peer_id: String,
    ip: Option<String>,
    web_ui_url: Option<String>,
    version: Option<String>,
    connected_at: Instant,
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
    stored_at: Instant,
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
        .route("/", get(index_handler))
        .route("/health", get(healthcheck))
        .route("/envelopes", post(store_envelope))
        .route("/envelopes/batch", post(store_envelopes_batch))
        .route("/inbox/:recipient_id", get(fetch_inbox))
        .route("/inbox/batch", post(fetch_inbox_batch))
        .route("/ws/:recipient_id", get(ws_handler))
        .route("/debug/stats", get(debug_stats))
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
                total_stored: 0,
                total_delivered: 0,
                latency_min_ms: u64::MAX,
                latency_max_ms: 0,
                latency_sum_ms: 0,
                latency_count: 0,
                inbox_deliveries: HashMap::new(),
                peer_sends: HashMap::new(),
                ws_clients: HashMap::new(),
                next_conn_id: 0,
            })),
            subscribers: Arc::new(RwLock::new(HashMap::new())),
            start_time: Instant::now(),
            ws_connections: Arc::new(AtomicUsize::new(0)),
        }
    }

    async fn subscribe(&self, recipient_id: &str) -> broadcast::Receiver<Value> {
        let subs = self.subscribers.read().await;
        if let Some(tx) = subs.get(recipient_id) {
            return tx.subscribe();
        }
        drop(subs);

        let mut subs = self.subscribers.write().await;
        let tx = subs
            .entry(recipient_id.to_string())
            .or_insert_with(|| broadcast::channel(WS_BROADCAST_CAPACITY).0);
        tx.subscribe()
    }

    async fn notify_subscribers(&self, recipient_id: &str, envelope: Value) {
        let subs = self.subscribers.read().await;
        if let Some(tx) = subs.get(recipient_id) {
            let _ = tx.send(envelope);
        }
    }

    pub fn start_peer_log_task(&self, mut shutdown_rx: tokio::sync::oneshot::Receiver<()>) {
        if self.config.peer_log_interval.is_zero() {
            return;
        }
        let state = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(state.config.peer_log_interval);
            loop {
                tokio::select! {
                    _ = ticker.tick() => {
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
                    _ = &mut shutdown_rx => {
                        break;
                    }
                }
            }
        });
    }
}

async fn index_handler() -> impl IntoResponse {
    Html(DASHBOARD_HTML)
}

async fn healthcheck() -> impl IntoResponse {
    StatusCode::OK
}

#[derive(Serialize)]
struct InboxInfo {
    peer_id: String,
    queue_depth: usize,
    queue_bytes: usize,
    last_seen_secs: u64,
    delivered_1m: usize,
    delivered_1h: usize,
    delivered_24h: usize,
}

#[derive(Serialize)]
struct LatencyInfo {
    min: u64,
    max: u64,
    avg: u64,
    count: u64,
}

#[derive(Serialize)]
struct WsClientStats {
    peer_id: String,
    ip: Option<String>,
    web_ui_url: Option<String>,
    version: Option<String>,
    connected_secs: u64,
    sent_1m: usize,
    sent_1h: usize,
    sent_24h: usize,
}

async fn debug_stats(State(state): State<RelayState>) -> impl IntoResponse {
    let inner = state.inner.lock().await;
    let now = Instant::now();

    // Legacy fields kept for backwards compatibility with the debugger CLI
    let queues: HashMap<String, usize> = inner
        .queues
        .iter()
        .map(|(k, v)| (k.clone(), v.len()))
        .collect();
    let total: usize = queues.values().sum();
    let peer_count = inner.peer_last_seen.len();

    // Per-inbox detail, sorted by queue depth descending
    let mut inboxes: Vec<InboxInfo> = inner
        .queues
        .iter()
        .map(|(peer_id, queue)| {
            let queue_depth = queue.iter().filter(|e| e.expires_at > now).count();
            let queue_bytes: usize = queue
                .iter()
                .filter(|e| e.expires_at > now)
                .map(|e| e.size_bytes)
                .sum();
            let last_seen_secs = inner
                .peer_last_seen
                .get(peer_id)
                .map(|t| now.duration_since(*t).as_secs())
                .unwrap_or(0);
            let deliveries = inner.inbox_deliveries.get(peer_id);
            InboxInfo {
                peer_id: peer_id.clone(),
                queue_depth,
                queue_bytes,
                last_seen_secs,
                delivered_1m: deliveries.map(|d| count_in_window(d, now, 60)).unwrap_or(0),
                delivered_1h: deliveries.map(|d| count_in_window(d, now, 3600)).unwrap_or(0),
                delivered_24h: deliveries.map(|d| count_in_window(d, now, 86400)).unwrap_or(0),
            }
        })
        .collect();
    inboxes.sort_by(|a, b| b.queue_depth.cmp(&a.queue_depth).then(a.peer_id.cmp(&b.peer_id)));

    let total_queued_bytes: usize = inboxes.iter().map(|i| i.queue_bytes).sum();

    let latency = if inner.latency_count > 0 {
        Some(LatencyInfo {
            min: inner.latency_min_ms,
            max: inner.latency_max_ms,
            avg: inner.latency_sum_ms / inner.latency_count,
            count: inner.latency_count,
        })
    } else {
        None
    };

    let mut ws_clients: Vec<WsClientStats> = inner
        .ws_clients
        .values()
        .map(|c| {
            let sends = inner.peer_sends.get(&c.peer_id);
            WsClientStats {
                peer_id: c.peer_id.clone(),
                ip: c.ip.clone(),
                web_ui_url: c.web_ui_url.clone(),
                version: c.version.clone(),
                connected_secs: now.duration_since(c.connected_at).as_secs(),
                sent_1m: sends.map(|s| count_in_window(s, now, 60)).unwrap_or(0),
                sent_1h: sends.map(|s| count_in_window(s, now, 3600)).unwrap_or(0),
                sent_24h: sends.map(|s| count_in_window(s, now, 86400)).unwrap_or(0),
            }
        })
        .collect();
    ws_clients.sort_by(|a, b| a.peer_id.cmp(&b.peer_id));

    let uptime_secs = state.start_time.elapsed().as_secs();
    let ws_connections = state.ws_connections.load(Ordering::Relaxed);

    Json(serde_json::json!({
        // Legacy fields
        "queues": queues,
        "total": total,
        "peer_count": peer_count,
        // Extended fields
        "uptime_secs": uptime_secs,
        "total_inboxes": inboxes.len(),
        "total_queued": total,
        "total_queued_bytes": total_queued_bytes,
        "total_stored": inner.total_stored,
        "total_delivered": inner.total_delivered,
        "ws_connections": ws_connections,
        "ws_clients": ws_clients,
        "inboxes": inboxes,
        "latency_ms": latency,
        "config": {
            "ttl_secs": state.config.ttl.as_secs(),
            "max_messages": state.config.max_messages,
            "max_bytes": state.config.max_bytes,
        }
    }))
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    Path(recipient_id): Path<String>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<RelayState>,
) -> impl IntoResponse {
    let ip = addr.ip().to_string();
    ws.on_upgrade(move |socket| handle_ws_connection(socket, recipient_id, ip, state))
}

async fn handle_ws_connection(
    mut socket: WebSocket,
    recipient_id: String,
    ip: String,
    state: RelayState,
) {
    let mut rx = state.subscribe(&recipient_id).await;

    {
        let mut inner = state.inner.lock().await;
        let now = Instant::now();
        record_peer_connection(&state.config, &mut inner, &recipient_id, now);
    }

    let conn_id = {
        let mut inner = state.inner.lock().await;
        let id = inner.next_conn_id;
        inner.next_conn_id += 1;
        inner.ws_clients.insert(
            id,
            WsClientInfo {
                peer_id: recipient_id.clone(),
                ip: Some(ip),
                web_ui_url: None,
                version: None,
                connected_at: Instant::now(),
            },
        );
        id
    };

    state.ws_connections.fetch_add(1, Ordering::Relaxed);

    log_message(
        &state.config,
        format!(
            "relay: websocket connected {}",
            format_peer_id(&recipient_id)
        ),
    );

    loop {
        tokio::select! {
            result = rx.recv() => {
                match result {
                    Ok(envelope) => {
                        let text = match serde_json::to_string(&envelope) {
                            Ok(t) => t,
                            Err(_) => continue,
                        };
                        if socket.send(Message::Text(text)).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        log_message(
                            &state.config,
                            format!(
                                "relay: websocket {} lagged by {} messages",
                                format_peer_id(&recipient_id),
                                n
                            ),
                        );
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        // Handle hello message from client
                        if let Ok(hello) = serde_json::from_str::<Value>(&text) {
                            if hello.get("type").and_then(|v| v.as_str()) == Some("hello") {
                                let mut inner = state.inner.lock().await;
                                if let Some(client) = inner.ws_clients.get_mut(&conn_id) {
                                    if let Some(v) = hello.get("version").and_then(|v| v.as_str()) {
                                        client.version = Some(v.to_string());
                                    }
                                    if let Some(port) = hello.get("web_ui_port").and_then(|v| v.as_u64()) {
                                        let ip = client.ip.clone().unwrap_or_default();
                                        client.web_ui_url = Some(format!("http://{}:{}", ip, port));
                                    }
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        if socket.send(Message::Pong(data)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {}
                }
            }
        }
    }

    {
        let mut inner = state.inner.lock().await;
        inner.ws_clients.remove(&conn_id);
    }

    state.ws_connections.fetch_sub(1, Ordering::Relaxed);

    log_message(
        &state.config,
        format!(
            "relay: websocket disconnected {}",
            format_peer_id(&recipient_id)
        ),
    );
}

async fn store_envelope(
    State(state): State<RelayState>,
    Json(payload): Json<Value>,
) -> impl IntoResponse {
    let recipient_id = recipient_id_from(&payload);
    let ws_payload = payload.clone();

    let result = {
        let mut inner = match lock_with_retry(&state).await {
            Ok(inner) => inner,
            Err(status) => return (status, "relay busy").into_response(),
        };
        let now = Instant::now();
        let now_epoch = SystemTime::now();
        store_envelope_locked(&state.config, &mut inner, payload, now, now_epoch)
    };

    match result {
        Ok(StoreDecision::Accepted { message_id }) => {
            if let Some(rid) = &recipient_id {
                state.notify_subscribers(rid, ws_payload).await;
            }
            (StatusCode::ACCEPTED, Json(StoreResponse { message_id })).into_response()
        }
        Ok(StoreDecision::Broadcast {
            message_id,
            recipients,
        }) => {
            for rid in &recipients {
                state.notify_subscribers(rid, ws_payload.clone()).await;
            }
            (StatusCode::ACCEPTED, Json(StoreResponse { message_id })).into_response()
        }
        Ok(StoreDecision::Duplicate { message_id }) => {
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
    let payload_copies: Vec<_> = payloads
        .iter()
        .map(|p| (recipient_id_from(p), p.clone()))
        .collect();

    let (results, notify_targets): (Vec<_>, Vec<_>) = {
        let mut inner = match lock_with_retry(&state).await {
            Ok(inner) => inner,
            Err(status) => return (status, "relay busy").into_response(),
        };
        let now = Instant::now();
        let now_epoch = SystemTime::now();
        payloads
            .into_iter()
            .map(|payload| {
                match store_envelope_locked(&state.config, &mut inner, payload, now, now_epoch) {
                    Ok(StoreDecision::Accepted { message_id }) => (
                        BatchStoreResult {
                            message_id: Some(message_id),
                            status: BatchStoreStatus::Accepted,
                            detail: None,
                        },
                        None,
                    ),
                    Ok(StoreDecision::Broadcast {
                        message_id,
                        recipients,
                    }) => (
                        BatchStoreResult {
                            message_id: Some(message_id),
                            status: BatchStoreStatus::Accepted,
                            detail: None,
                        },
                        Some(recipients),
                    ),
                    Ok(StoreDecision::Duplicate { message_id }) => (
                        BatchStoreResult {
                            message_id: Some(message_id),
                            status: BatchStoreStatus::Duplicate,
                            detail: None,
                        },
                        None,
                    ),
                    Ok(StoreDecision::Expired { message_id }) => (
                        BatchStoreResult {
                            message_id: Some(message_id),
                            status: BatchStoreStatus::Expired,
                            detail: None,
                        },
                        None,
                    ),
                    Ok(StoreDecision::TooLarge { message_id }) => (
                        BatchStoreResult {
                            message_id: Some(message_id),
                            status: BatchStoreStatus::TooLarge,
                            detail: None,
                        },
                        None,
                    ),
                    Err(StoreError::MissingRecipient) => (
                        BatchStoreResult {
                            message_id: None,
                            status: BatchStoreStatus::Invalid,
                            detail: Some("missing recipient_id".to_string()),
                        },
                        None,
                    ),
                    Err(StoreError::InvalidPayload(detail)) => (
                        BatchStoreResult {
                            message_id: None,
                            status: BatchStoreStatus::Invalid,
                            detail: Some(detail),
                        },
                        None,
                    ),
                }
            })
            .unzip()
    };

    for (i, result) in results.iter().enumerate() {
        if matches!(result.status, BatchStoreStatus::Accepted) {
            if let Some(Some(broadcast_recipients)) = notify_targets.get(i) {
                // Broadcast: notify all recipient peers
                if let Some((_, payload)) = payload_copies.get(i) {
                    for rid in broadcast_recipients {
                        state.notify_subscribers(rid, payload.clone()).await;
                    }
                }
            } else if let Some((Some(rid), payload)) = payload_copies.get(i) {
                state.notify_subscribers(rid, payload.clone()).await;
            }
        }
    }

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
    let (expired_ids, envelopes, latencies) = match inner.queues.get_mut(&recipient_id) {
        Some(queue) => pop_envelopes(queue, query.limit, now),
        None => return (StatusCode::OK, Json(Vec::<Value>::new())).into_response(),
    };

    record_deliveries(&mut inner, &recipient_id, &latencies, now);

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

    // Collect (recipient_id, latencies) pairs before calling record_deliveries
    // to avoid borrowing inner.queues while mutably borrowing inner.inbox_deliveries.
    let mut delivery_records: Vec<(String, Vec<u128>)> = Vec::new();

    for recipient_id in request.recipient_ids {
        record_peer_connection(&state.config, &mut inner, &recipient_id, now);
        let (expired, envelopes, latencies) = match inner.queues.get_mut(&recipient_id) {
            Some(queue) => pop_envelopes(queue, request.limit, now),
            None => (Vec::new(), Vec::new(), Vec::new()),
        };
        expired_ids.extend(expired);
        if !latencies.is_empty() {
            delivery_records.push((recipient_id.clone(), latencies));
        }
        results.insert(recipient_id, envelopes);
    }

    for (recipient_id, latencies) in delivery_records {
        record_deliveries(&mut inner, &recipient_id, &latencies, now);
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
) -> (Vec<String>, Vec<Value>, Vec<u128>) {
    let expired_ids = prune_expired(queue, now);
    let limit = limit.unwrap_or(queue.len());
    let mut envelopes = Vec::new();
    let mut latencies = Vec::new();

    for _ in 0..limit.min(queue.len()) {
        if let Some(item) = queue.pop_front() {
            latencies.push(now.duration_since(item.stored_at).as_millis());
            envelopes.push(item.body);
        }
    }

    (expired_ids, envelopes, latencies)
}

/// Push a delivery timestamp for `recipient_id` and update lifetime latency stats.
fn record_deliveries(
    inner: &mut RelayStateInner,
    recipient_id: &str,
    latencies: &[u128],
    now: Instant,
) {
    for &lat_ms in latencies {
        let lat = lat_ms as u64;
        if lat < inner.latency_min_ms {
            inner.latency_min_ms = lat;
        }
        if lat > inner.latency_max_ms {
            inner.latency_max_ms = lat;
        }
        inner.latency_sum_ms = inner.latency_sum_ms.saturating_add(lat);
        inner.latency_count += 1;
        push_timestamped(
            inner
                .inbox_deliveries
                .entry(recipient_id.to_string())
                .or_default(),
            now,
        );
    }
    inner.total_delivered += latencies.len() as u64;
}

/// Push `now` into a rolling 24-hour timestamp window.
fn push_timestamped(deque: &mut VecDeque<Instant>, now: Instant) {
    deque.push_back(now);
    let max_age = Duration::from_secs(86400);
    while let Some(&front) = deque.front() {
        if now.duration_since(front) > max_age {
            deque.pop_front();
        } else {
            break;
        }
    }
}

/// Count timestamps in `deque` that fall within the last `window_secs` seconds.
fn count_in_window(deque: &VecDeque<Instant>, now: Instant, window_secs: u64) -> usize {
    let window = Duration::from_secs(window_secs);
    // Deque is ordered oldest-first, so iterate from the back for efficiency.
    deque
        .iter()
        .rev()
        .take_while(|&&t| now.duration_since(t) <= window)
        .count()
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
    Accepted {
        message_id: String,
    },
    Broadcast {
        message_id: String,
        recipients: Vec<String>,
    },
    Duplicate {
        message_id: String,
    },
    Expired {
        message_id: String,
    },
    TooLarge {
        message_id: String,
    },
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
    let is_broadcast = recipient_id == "*";
    if !is_broadcast {
        record_peer_connection(config, inner, &recipient_id, now);
    }
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

    prune_dedup(&mut inner.dedup);
    if inner
        .dedup
        .get(&message_id)
        .is_some_and(|entry_expires| *entry_expires > now)
    {
        return Ok(StoreDecision::Duplicate { message_id });
    }

    inner.dedup.insert(message_id.clone(), expires_at);

    if is_broadcast {
        // Fan out to all known peers except the sender
        let recipients: Vec<String> = inner
            .peer_last_seen
            .keys()
            .filter(|pid| sender_id.as_deref() != Some(pid.as_str()))
            .cloned()
            .collect();

        for rid in &recipients {
            let stored = StoredEnvelope {
                body: payload.clone(),
                size_bytes,
                expires_at,
                message_id: message_id.clone(),
                stored_at: now,
            };
            let queue = inner
                .queues
                .entry(rid.clone())
                .or_insert_with(VecDeque::new);
            let evicted_ids = prune_expired(queue, now);
            for eid in evicted_ids {
                inner.dedup.remove(&eid);
            }
            while queue.len() >= config.max_messages
                || queue_bytes(queue) + size_bytes > config.max_bytes
            {
                if let Some(evicted) = queue.pop_front() {
                    inner.dedup.remove(&evicted.message_id);
                }
            }
            queue.push_back(stored);
        }

        inner.total_stored += recipients.len() as u64;

        if let Some(ref sender_id) = sender_id {
            push_timestamped(
                inner.peer_sends.entry(sender_id.clone()).or_default(),
                now,
            );
            log_message(
                config,
                format!(
                    "relay: broadcast {} -> {} peers (message_id: {})",
                    format_peer_id(sender_id),
                    recipients.len(),
                    format_message_id(&message_id)
                ),
            );
        }

        return Ok(StoreDecision::Broadcast {
            message_id,
            recipients,
        });
    }

    let stored = StoredEnvelope {
        body: payload,
        size_bytes,
        expires_at,
        message_id: message_id.clone(),
        stored_at: now,
    };
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

    inner.total_stored += 1;

    for message_id in evicted_ids {
        inner.dedup.remove(&message_id);
    }

    if let Some(sender_id) = sender_id {
        push_timestamped(
            inner.peer_sends.entry(sender_id.clone()).or_default(),
            now,
        );
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
        let ts = crate::logging::format_timestamp();
        if crate::logging::colour_enabled() {
            eprintln!("\x1b[2m{ts}\x1b[0m {message}");
        } else {
            eprintln!("{ts} - {message}");
        }
    }
}

fn format_peer_id(peer_id: &str) -> String {
    crate::logging::peer_id(peer_id)
}

fn format_message_id(message_id: &str) -> String {
    crate::logging::msg_id(message_id)
}

fn count_recent_peers(inner: &RelayStateInner, now: Instant, window: Duration) -> usize {
    inner
        .peer_last_seen
        .values()
        .filter(|last_seen| now.duration_since(**last_seen) <= window)
        .count()
}
