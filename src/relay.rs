use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::DefaultBodyLimit;
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
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::{broadcast, Mutex, MutexGuard, RwLock};
use tokio::time::sleep;

const DASHBOARD_HTML: &str = include_str!("../web/src/relay_dashboard.html");

/// Per-tier limits for per-sender-per-inbox quotas.
#[derive(Clone, Debug)]
pub struct TierLimits {
    /// Maximum number of envelopes from this sender currently in the inbox.
    pub max_envelopes: usize,
    /// Maximum total bytes from this sender currently in the inbox.
    pub max_bytes: usize,
}

/// QoS configuration for the relay.
#[derive(Clone, Debug)]
pub struct RelayQosConfig {
    /// Limits for Tier 1 (unknown / no reply observed).
    pub tier1: TierLimits,
    /// Limits for Tier 2 (acknowledged — recipient has sent to sender at any point).
    pub tier2: TierLimits,
    /// Limits for Tier 3 (active — bidirectional traffic within the window).
    pub tier3: TierLimits,
    /// How long bidirectional activity must be absent before downgrading from Tier 3 to Tier 2.
    pub bidirectional_window: Duration,
}

impl Default for RelayQosConfig {
    fn default() -> Self {
        Self {
            tier1: TierLimits {
                max_envelopes: 20,
                max_bytes: 256 * 1024,
            },
            tier2: TierLimits {
                max_envelopes: 100,
                max_bytes: 2 * 1024 * 1024,
            },
            tier3: TierLimits {
                max_envelopes: 500,
                max_bytes: 10 * 1024 * 1024,
            },
            bidirectional_window: Duration::from_secs(7 * 24 * 3600),
        }
    }
}

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
    /// QoS configuration. Use `RelayQosConfig::default()` for standard limits.
    pub qos: RelayQosConfig,
    /// Maximum allowed size (bytes) of a single base64-encoded encrypted chunk
    /// in a `POST /blobs` request body.  Default: 512 KiB.
    pub blob_max_chunk_bytes: usize,
    /// Maximum bytes a single sender may upload via `POST /blobs` per calendar
    /// day (UTC).  Default: 500 MiB.
    pub blob_daily_quota_bytes: u64,
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
    // QoS: per-(sender_id, recipient_id) envelope/byte count of messages currently in inbox.
    sender_inbox_usage: HashMap<(String, String), SenderUsage>,
    // QoS: most recent send timestamp for each directed (sender, recipient) pair.
    directed_last_seen: HashMap<(String, String), Instant>,
    // QoS: set of (min, max) peer-id pairs where bidirectionality has been observed.
    bidirectional_pairs: HashSet<(String, String)>,
    // Blob store (Option D Phase 1): content-addressed encrypted chunks.
    // Key: SHA-256 hex of the encrypted chunk.
    blobs: HashMap<String, BlobEntry>,
    // Per-sender daily upload quota tracking.
    // Key: sender_id.  Value: (UTC epoch day, bytes uploaded that day).
    blob_daily_upload: HashMap<String, (u64, u64)>,
}

/// A single encrypted chunk stored by the relay blob endpoint.
struct BlobEntry {
    /// Raw encrypted chunk bytes.
    data: Vec<u8>,
    /// Uploader peer ID (reserved for future authorised DELETE enforcement).
    #[allow(dead_code)]
    uploader_id: Option<String>,
}

struct WsClientInfo {
    peer_id: String,
    ip: Option<String>,
    web_ui_url: Option<String>,
    version: Option<String>,
    connected_at: Instant,
}

/// Tracks how many envelopes and bytes from a given sender currently reside
/// in a specific recipient's inbox.
struct SenderUsage {
    count: usize,
    bytes: usize,
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
    /// Sender identity, used for QoS tracking and smart eviction.
    sender_id: Option<String>,
}

#[derive(Deserialize)]
struct InboxQuery {
    limit: Option<usize>,
    /// If set, only return envelopes whose serialised size is ≤ this threshold.
    /// Envelopes that exceed the threshold remain in the queue for a later fetch.
    max_size_bytes: Option<usize>,
}

#[derive(Deserialize)]
struct WsAuthQuery {
    token: Option<String>,
}

#[derive(Serialize)]
struct StoreResponse {
    message_id: String,
}

#[derive(Deserialize)]
struct BatchInboxRequest {
    recipient_ids: Vec<String>,
    limit: Option<usize>,
    /// If set, only return envelopes whose serialised size is ≤ this threshold.
    max_size_bytes: Option<usize>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum BatchStoreStatus {
    Accepted,
    Duplicate,
    Expired,
    TooLarge,
    Invalid,
    QuotaExceeded,
}

#[derive(Serialize)]
struct BatchStoreResult {
    message_id: Option<String>,
    status: BatchStoreStatus,
    detail: Option<String>,
}

/// Maximum HTTP body size for `POST /blobs` requests.
/// Sized to comfortably hold a base64-encoded 256 KiB encrypted chunk
/// plus JSON framing (≈ 350 KiB) with margin.
const BLOB_UPLOAD_BODY_LIMIT: usize = 512 * 1024;

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
        // Blob endpoints (Option D Phase 1)
        .route(
            "/blobs",
            post(upload_blob).layer(DefaultBodyLimit::max(BLOB_UPLOAD_BODY_LIMIT)),
        )
        .route("/blobs/:chunk_hash", get(download_blob).delete(delete_blob))
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
                sender_inbox_usage: HashMap::new(),
                directed_last_seen: HashMap::new(),
                bidirectional_pairs: HashSet::new(),
                blobs: HashMap::new(),
                blob_daily_upload: HashMap::new(),
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
struct InboxSenderInfo {
    sender_id: String,
    count: usize,
    bytes: usize,
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
    /// Per-sender usage currently queued in this inbox, sorted by bytes descending.
    senders: Vec<InboxSenderInfo>,
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

    // Build a per-recipient sender-usage map for the inbox detail rows.
    let mut recipient_senders: HashMap<&str, Vec<InboxSenderInfo>> = HashMap::new();
    for ((sender_id, recipient_id), usage) in &inner.sender_inbox_usage {
        if usage.count > 0 || usage.bytes > 0 {
            recipient_senders
                .entry(recipient_id.as_str())
                .or_default()
                .push(InboxSenderInfo {
                    sender_id: sender_id.clone(),
                    count: usage.count,
                    bytes: usage.bytes,
                });
        }
    }
    // Sort each sender list by bytes descending.
    for senders in recipient_senders.values_mut() {
        senders.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.sender_id.cmp(&b.sender_id)));
    }

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
            let senders = recipient_senders
                .remove(peer_id.as_str())
                .unwrap_or_default();
            InboxInfo {
                peer_id: peer_id.clone(),
                queue_depth,
                queue_bytes,
                last_seen_secs,
                delivered_1m: deliveries.map(|d| count_in_window(d, now, 60)).unwrap_or(0),
                delivered_1h: deliveries
                    .map(|d| count_in_window(d, now, 3600))
                    .unwrap_or(0),
                delivered_24h: deliveries
                    .map(|d| count_in_window(d, now, 86400))
                    .unwrap_or(0),
                senders,
            }
        })
        .collect();
    inboxes.sort_by(|a, b| {
        b.queue_depth
            .cmp(&a.queue_depth)
            .then(a.peer_id.cmp(&b.peer_id))
    });

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

    let qos = &state.config.qos;
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
            "qos": {
                "tier1": { "max_envelopes": qos.tier1.max_envelopes, "max_bytes": qos.tier1.max_bytes },
                "tier2": { "max_envelopes": qos.tier2.max_envelopes, "max_bytes": qos.tier2.max_bytes },
                "tier3": { "max_envelopes": qos.tier3.max_envelopes, "max_bytes": qos.tier3.max_bytes },
                "bidirectional_window_secs": qos.bidirectional_window.as_secs(),
            }
        }
    }))
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    Path(recipient_id): Path<String>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Query(auth): Query<WsAuthQuery>,
    State(state): State<RelayState>,
) -> axum::response::Response {
    let token = match auth.token {
        Some(t) => t,
        None => return (StatusCode::UNAUTHORIZED, "missing token").into_response(),
    };
    if crate::crypto::verify_relay_auth_token(&token, &recipient_id).is_err() {
        return (StatusCode::UNAUTHORIZED, "invalid auth token").into_response();
    }
    let ip = addr.ip().to_string();
    ws.on_upgrade(move |socket| handle_ws_connection(socket, recipient_id, ip, state))
        .into_response()
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
        touch_peer(&mut inner, &recipient_id, now);
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
        Ok(StoreDecision::QuotaExceeded { .. }) => (
            StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({"error": "sender quota exceeded for this inbox"})),
        )
            .into_response(),
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
                    Ok(StoreDecision::QuotaExceeded { message_id }) => (
                        BatchStoreResult {
                            message_id: Some(message_id),
                            status: BatchStoreStatus::QuotaExceeded,
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
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    let token = match extract_bearer_token(&headers) {
        Some(t) => t,
        None => return (StatusCode::UNAUTHORIZED, "missing Authorization header").into_response(),
    };
    if crate::crypto::verify_relay_auth_token(&token, &recipient_id).is_err() {
        return (StatusCode::UNAUTHORIZED, "invalid auth token").into_response();
    }

    let mut inner = match lock_with_retry(&state).await {
        Ok(inner) => inner,
        Err(status) => return (status, "relay busy").into_response(),
    };
    let now = Instant::now();
    record_peer_connection(&state.config, &mut inner, &recipient_id, now);
    let (expired_ids, envelopes, latencies, usage_decrements) =
        match inner.queues.get_mut(&recipient_id) {
            Some(queue) => pop_envelopes(queue, query.limit, query.max_size_bytes, now),
            None => return (StatusCode::OK, Json(Vec::<Value>::new())).into_response(),
        };

    apply_usage_decrements(
        &mut inner.sender_inbox_usage,
        &recipient_id,
        &usage_decrements,
    );
    record_deliveries(&mut inner, &recipient_id, &latencies, now);

    for message_id in expired_ids {
        inner.dedup.remove(&message_id);
    }
    prune_dedup(&mut inner.dedup);

    (StatusCode::OK, Json(envelopes)).into_response()
}

fn extract_bearer_token(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(str::to_string)
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

    // Collect (recipient_id, latencies, usage_decrements) before calling record_deliveries
    // to avoid borrowing inner.queues while mutably borrowing inner.inbox_deliveries.
    let mut delivery_records: Vec<(String, Vec<u128>)> = Vec::new();
    let mut all_usage_decrements: Vec<(String, Vec<(Option<String>, usize)>)> = Vec::new();

    for recipient_id in request.recipient_ids {
        record_peer_connection(&state.config, &mut inner, &recipient_id, now);
        let (expired, envelopes, latencies, usage_decrements) =
            match inner.queues.get_mut(&recipient_id) {
                Some(queue) => pop_envelopes(queue, request.limit, request.max_size_bytes, now),
                None => (Vec::new(), Vec::new(), Vec::new(), Vec::new()),
            };
        expired_ids.extend(expired);
        if !latencies.is_empty() {
            delivery_records.push((recipient_id.clone(), latencies));
        }
        if !usage_decrements.is_empty() {
            all_usage_decrements.push((recipient_id.clone(), usage_decrements));
        }
        results.insert(recipient_id, envelopes);
    }

    for (recipient_id, decrements) in all_usage_decrements {
        apply_usage_decrements(&mut inner.sender_inbox_usage, &recipient_id, &decrements);
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

/// Pop envelopes from the queue, optionally filtering by max serialised size.
///
/// Returns `(expired_ids, envelopes, latencies, usage_decrements)`.
/// `usage_decrements` contains `(sender_id, size_bytes)` for every envelope removed
/// from the queue (both expired and delivered), so callers can update `sender_inbox_usage`.
/// Envelopes whose size exceeds `max_size_bytes` are left in the queue unchanged.
fn pop_envelopes(
    queue: &mut VecDeque<StoredEnvelope>,
    limit: Option<usize>,
    max_size_bytes: Option<usize>,
    now: Instant,
) -> (
    Vec<String>,
    Vec<Value>,
    Vec<u128>,
    Vec<(Option<String>, usize)>,
) {
    let (expired_ids, mut usage_decrements) = prune_expired(queue, now);
    let limit = limit.unwrap_or(queue.len());
    let mut envelopes = Vec::new();
    let mut latencies = Vec::new();

    if let Some(max_size) = max_size_bytes {
        // Size-filtered: walk front-to-back, pop items within the size budget,
        // leave oversized items in place for a later (unfiltered) fetch.
        let mut i = 0;
        while envelopes.len() < limit && i < queue.len() {
            if queue[i].size_bytes <= max_size {
                // remove(i) shifts later elements left; next item lands at index i.
                if let Some(item) = queue.remove(i) {
                    latencies.push(now.duration_since(item.stored_at).as_millis());
                    usage_decrements.push((item.sender_id.clone(), item.size_bytes));
                    envelopes.push(item.body);
                }
            } else {
                i += 1;
            }
        }
    } else {
        for _ in 0..limit.min(queue.len()) {
            if let Some(item) = queue.pop_front() {
                latencies.push(now.duration_since(item.stored_at).as_millis());
                usage_decrements.push((item.sender_id.clone(), item.size_bytes));
                envelopes.push(item.body);
            }
        }
    }

    (expired_ids, envelopes, latencies, usage_decrements)
}

/// Decrement `sender_inbox_usage` for all (sender_id, size_bytes) pairs that were
/// removed from `recipient_id`'s inbox (expired or delivered).
fn apply_usage_decrements(
    sender_inbox_usage: &mut HashMap<(String, String), SenderUsage>,
    recipient_id: &str,
    decrements: &[(Option<String>, usize)],
) {
    for (sender_id, bytes) in decrements {
        if let Some(sid) = sender_id {
            let key = (sid.clone(), recipient_id.to_string());
            if let Some(usage) = sender_inbox_usage.get_mut(&key) {
                usage.count = usage.count.saturating_sub(1);
                usage.bytes = usage.bytes.saturating_sub(*bytes);
            }
        }
    }
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

/// Remove expired envelopes and return:
/// - `expired_ids`: message IDs to clean from the dedup map.
/// - `usage_decrements`: `(sender_id, size_bytes)` for each removed envelope,
///   so callers can update `sender_inbox_usage`.
fn prune_expired(
    queue: &mut VecDeque<StoredEnvelope>,
    now: Instant,
) -> (Vec<String>, Vec<(Option<String>, usize)>) {
    let mut expired_ids = Vec::new();
    let mut usage_decrements = Vec::new();
    queue.retain(|item| {
        if item.expires_at <= now {
            expired_ids.push(item.message_id.clone());
            usage_decrements.push((item.sender_id.clone(), item.size_bytes));
            false
        } else {
            true
        }
    });
    (expired_ids, usage_decrements)
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
    /// Sender has exceeded the per-tier quota for this recipient's inbox.
    QuotaExceeded {
        message_id: String,
    },
}

enum StoreError {
    MissingRecipient,
    InvalidPayload(String),
}

/// Return the QoS tier limits for a (sender → recipient) message given the current
/// bidirectionality state.
fn get_tier<'a>(
    config: &'a RelayQosConfig,
    inner: &RelayStateInner,
    sender: &str,
    recipient: &str,
    now: Instant,
) -> &'a TierLimits {
    // Canonical key: lexicographic-min peer first.
    let canonical = if sender <= recipient {
        (sender.to_string(), recipient.to_string())
    } else {
        (recipient.to_string(), sender.to_string())
    };

    // Tier 3: bidirectionality observed and both sides active within the window.
    if inner.bidirectional_pairs.contains(&canonical) {
        let s_to_r = inner
            .directed_last_seen
            .get(&(sender.to_string(), recipient.to_string()));
        let r_to_s = inner
            .directed_last_seen
            .get(&(recipient.to_string(), sender.to_string()));
        if let (Some(&s_ts), Some(&r_ts)) = (s_to_r, r_to_s) {
            if now.duration_since(s_ts) <= config.bidirectional_window
                && now.duration_since(r_ts) <= config.bidirectional_window
            {
                return &config.tier3;
            }
        }
    }

    // Tier 2: recipient has sent to sender at any point in relay history.
    if inner
        .directed_last_seen
        .contains_key(&(recipient.to_string(), sender.to_string()))
    {
        return &config.tier2;
    }

    // Tier 1: no reply ever observed.
    &config.tier1
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
        touch_peer(inner, &recipient_id, now);
    }
    if let Some(sender_id) = sender_id.as_deref() {
        touch_peer(inner, sender_id, now);
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
        // Fan out to all known peers except the sender.
        // Broadcasts bypass per-sender QoS (recipient is "*", not a specific peer).
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
                sender_id: sender_id.clone(),
            };
            let queue = inner
                .queues
                .entry(rid.clone())
                .or_insert_with(VecDeque::new);
            let (evicted_ids, expired_usage) = prune_expired(queue, now);
            apply_usage_decrements(&mut inner.sender_inbox_usage, rid, &expired_usage);
            for eid in evicted_ids {
                inner.dedup.remove(&eid);
            }
            while queue.len() >= config.max_messages
                || queue_bytes(queue) + size_bytes > config.max_bytes
            {
                if let Some(evicted) = queue.pop_front() {
                    inner.dedup.remove(&evicted.message_id);
                    if let Some(ref sid) = evicted.sender_id {
                        let key = (sid.clone(), rid.clone());
                        if let Some(usage) = inner.sender_inbox_usage.get_mut(&key) {
                            usage.count = usage.count.saturating_sub(1);
                            usage.bytes = usage.bytes.saturating_sub(evicted.size_bytes);
                        }
                    }
                }
            }
            queue.push_back(stored);
        }

        inner.total_stored += recipients.len() as u64;

        if let Some(ref sender_id) = sender_id {
            push_timestamped(inner.peer_sends.entry(sender_id.clone()).or_default(), now);
            let recipient_list = if recipients.len() <= 5 {
                recipients
                    .iter()
                    .map(|r| format_peer_id(r))
                    .collect::<Vec<_>>()
                    .join(", ")
            } else {
                format!("{} recipients", recipients.len())
            };
            log_message(
                config,
                format!(
                    "relay: broadcast {} -> [{}] (message_id: {}, total known peers: {})",
                    format_peer_id(sender_id),
                    recipient_list,
                    format_message_id(&message_id),
                    inner.peer_last_seen.len()
                ),
            );
        }

        return Ok(StoreDecision::Broadcast {
            message_id,
            recipients,
        });
    }

    // ── Non-broadcast path ────────────────────────────────────────────────────

    // QoS: update bidirectionality tracking.
    // Record that sender_id sent to recipient_id at this instant.
    if let Some(ref sid) = sender_id {
        inner
            .directed_last_seen
            .insert((sid.clone(), recipient_id.clone()), now);

        // Check if the reverse direction has been seen within the window.
        let reverse_key = (recipient_id.clone(), sid.clone());
        if let Some(&reverse_ts) = inner.directed_last_seen.get(&reverse_key) {
            if now.duration_since(reverse_ts) <= config.qos.bidirectional_window {
                // Both directions active within window → record bidirectionality.
                let canonical = if sid.as_str() <= recipient_id.as_str() {
                    (sid.clone(), recipient_id.clone())
                } else {
                    (recipient_id.clone(), sid.clone())
                };
                inner.bidirectional_pairs.insert(canonical);
            }
        }
    }

    // Get the queue entry, prune expired messages, then apply QoS quota check.
    let entry = inner.queues.entry(recipient_id.clone());
    if matches!(entry, std::collections::hash_map::Entry::Vacant(_)) {
        log_message(
            config,
            format!("relay: inbox created {}", format_peer_id(&recipient_id)),
        );
    }
    let queue = entry.or_insert_with(VecDeque::new);

    // Prune expired envelopes and update usage accounting.
    let (expired_ids_from_prune, expired_usage) = prune_expired(queue, now);
    apply_usage_decrements(&mut inner.sender_inbox_usage, &recipient_id, &expired_usage);
    let mut evicted_ids = expired_ids_from_prune;

    // QoS: check per-sender quota for this inbox (after pruning expired).
    if let Some(ref sid) = sender_id {
        let limits = get_tier(&config.qos, inner, sid, &recipient_id, now);
        let usage = inner
            .sender_inbox_usage
            .get(&(sid.clone(), recipient_id.clone()))
            .map(|u| (u.count, u.bytes))
            .unwrap_or((0, 0));
        if usage.0 >= limits.max_envelopes || usage.1 + size_bytes > limits.max_bytes {
            // Clean up dedup entry we just inserted (we're rejecting the envelope).
            inner.dedup.remove(&message_id);
            return Ok(StoreDecision::QuotaExceeded { message_id });
        }
    }

    // Evict to make room for the new envelope, preferring the sender with the
    // most bytes currently in this inbox (fair eviction).
    let queue = inner.queues.get_mut(&recipient_id).expect("queue exists");
    while queue.len() >= config.max_messages || queue_bytes(queue) + size_bytes > config.max_bytes {
        // Find the sender currently holding the most bytes in this inbox.
        let heaviest_sender: Option<String> = {
            let mut sender_bytes: HashMap<&str, usize> = HashMap::new();
            for item in queue.iter() {
                if let Some(ref sid) = item.sender_id {
                    *sender_bytes.entry(sid.as_str()).or_insert(0) += item.size_bytes;
                }
            }
            sender_bytes
                .into_iter()
                .max_by_key(|(_, b)| *b)
                .map(|(s, _)| s.to_string())
        };

        // Remove the oldest message from that sender (or fall back to front).
        let evicted = if let Some(ref heaviest) = heaviest_sender {
            let pos = queue
                .iter()
                .position(|item| item.sender_id.as_deref() == Some(heaviest.as_str()));
            pos.and_then(|i| queue.remove(i))
        } else {
            queue.pop_front()
        };

        if let Some(ev) = evicted {
            evicted_ids.push(ev.message_id.clone());
            if let Some(ref sid) = ev.sender_id {
                let key = (sid.clone(), recipient_id.clone());
                if let Some(usage) = inner.sender_inbox_usage.get_mut(&key) {
                    usage.count = usage.count.saturating_sub(1);
                    usage.bytes = usage.bytes.saturating_sub(ev.size_bytes);
                }
            }
        } else {
            break; // queue is empty
        }
    }

    // Enqueue the new envelope.
    let stored = StoredEnvelope {
        body: payload,
        size_bytes,
        expires_at,
        message_id: message_id.clone(),
        stored_at: now,
        sender_id: sender_id.clone(),
    };
    inner
        .queues
        .get_mut(&recipient_id)
        .expect("queue exists")
        .push_back(stored);

    // Update sender_inbox_usage for the new envelope.
    if let Some(ref sid) = sender_id {
        let usage = inner
            .sender_inbox_usage
            .entry((sid.clone(), recipient_id.clone()))
            .or_insert(SenderUsage { count: 0, bytes: 0 });
        usage.count += 1;
        usage.bytes += size_bytes;
    }

    inner.total_stored += 1;

    for mid in evicted_ids {
        inner.dedup.remove(&mid);
    }

    if let Some(sender_id) = sender_id {
        push_timestamped(inner.peer_sends.entry(sender_id.clone()).or_default(), now);
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

/// Update `peer_last_seen` and log "peer polled inbox" (rate-limited to once
/// per `peer_log_window`).  Call only from HTTP inbox poll handlers.
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
            format!("relay: peer polled inbox {}", format_peer_id(peer_id)),
        );
    }
}

/// Update `peer_last_seen` silently — no "peer connected" log.  Use when a
/// peer ID is seen as an envelope sender or recipient but has not explicitly
/// connected to fetch its inbox.
fn touch_peer(inner: &mut RelayStateInner, peer_id: &str, now: Instant) {
    inner.peer_last_seen.insert(peer_id.to_string(), now);
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

// ---------------------------------------------------------------------------
// Blob endpoints (Option D Phase 1)
// ---------------------------------------------------------------------------

/// Request body for `POST /blobs`.
#[derive(Deserialize)]
struct UploadBlobRequest {
    /// SHA-256 hex of the **encrypted** chunk used as the storage key.
    chunk_hash: String,
    /// Base64 URL-safe no-pad encoded encrypted chunk bytes.
    data_b64: String,
    /// Optional sender identity for quota tracking.
    sender_id: Option<String>,
}

/// Upload a single encrypted chunk to the relay blob store.
///
/// Returns `201 Created` on success, `409 Conflict` if the chunk already
/// exists, `400 Bad Request` on validation failure, or `413 Payload Too Large`
/// / `429 Too Many Requests` for quota violations.
async fn upload_blob(
    State(state): State<RelayState>,
    Json(req): Json<UploadBlobRequest>,
) -> impl IntoResponse {
    // Validate chunk_hash: must be exactly 64 lowercase hex characters.
    if req.chunk_hash.len() != 64 || !req.chunk_hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return (
            StatusCode::BAD_REQUEST,
            "invalid chunk_hash: expected 64 hex characters",
        )
            .into_response();
    }

    // Decode the base64-encoded encrypted chunk.
    let data = match URL_SAFE_NO_PAD.decode(&req.data_b64) {
        Ok(d) => d,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid base64 in data_b64").into_response(),
    };

    // Self-authenticate: verify declared hash matches content.
    let actual_hash = hex::encode(Sha256::digest(&data));
    if actual_hash != req.chunk_hash {
        return (
            StatusCode::BAD_REQUEST,
            "chunk_hash does not match SHA-256 of data",
        )
            .into_response();
    }

    // Enforce per-chunk size limit before acquiring the lock.
    if data.len() > state.config.blob_max_chunk_bytes {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            "chunk exceeds maximum allowed size",
        )
            .into_response();
    }

    let mut inner = match lock_with_retry(&state).await {
        Ok(inner) => inner,
        Err(status) => return (status, "relay busy").into_response(),
    };

    // Deduplicate: if already stored, return 409 Conflict.
    if inner.blobs.contains_key(&req.chunk_hash) {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"status": "exists", "chunk_hash": req.chunk_hash})),
        )
            .into_response();
    }

    // Enforce per-sender daily upload quota.
    if let Some(ref sender_id) = req.sender_id {
        let today = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            / 86400;
        let entry = inner
            .blob_daily_upload
            .entry(sender_id.clone())
            .or_insert((today, 0));
        // Reset counter when the calendar day rolls over.
        if entry.0 != today {
            *entry = (today, 0);
        }
        if entry.1 + data.len() as u64 > state.config.blob_daily_quota_bytes {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                "daily blob upload quota exceeded",
            )
                .into_response();
        }
        entry.1 += data.len() as u64;
    }

    inner.blobs.insert(
        req.chunk_hash.clone(),
        BlobEntry {
            data,
            uploader_id: req.sender_id,
        },
    );

    (
        StatusCode::CREATED,
        Json(serde_json::json!({"chunk_hash": req.chunk_hash})),
    )
        .into_response()
}

/// Download a single encrypted chunk by its SHA-256 hex hash.
///
/// No authentication required — content is opaque ciphertext, and the hash
/// is self-authenticating.  Returns `200 OK` with raw bytes or `404 Not Found`.
async fn download_blob(
    State(state): State<RelayState>,
    Path(chunk_hash): Path<String>,
) -> impl IntoResponse {
    // Clone the data under the lock, then release before building the response.
    let data = {
        let inner = match lock_with_retry(&state).await {
            Ok(inner) => inner,
            Err(status) => return (status, "relay busy").into_response(),
        };
        match inner.blobs.get(&chunk_hash) {
            Some(entry) => entry.data.clone(),
            None => return (StatusCode::NOT_FOUND, "chunk not found").into_response(),
        }
    };

    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
        data,
    )
        .into_response()
}

/// Delete a stored chunk by its SHA-256 hex hash.
///
/// Returns `204 No Content` on success or `404 Not Found` if absent.
async fn delete_blob(
    State(state): State<RelayState>,
    Path(chunk_hash): Path<String>,
) -> impl IntoResponse {
    let mut inner = match lock_with_retry(&state).await {
        Ok(inner) => inner,
        Err(status) => return (status, "relay busy").into_response(),
    };
    if inner.blobs.remove(&chunk_hash).is_some() {
        StatusCode::NO_CONTENT.into_response()
    } else {
        (StatusCode::NOT_FOUND, "chunk not found").into_response()
    }
}
