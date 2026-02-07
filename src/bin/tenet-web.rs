//! tenet-web: Web server binary that acts as a full tenet peer.
//!
//! Serves an embedded SPA, provides REST API + WebSocket for messages,
//! connects to relays, and persists state in SQLite.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use rust_embed::Embed;
use serde::Deserialize;
use tokio::sync::{broadcast, Mutex};

use tenet::client::{ClientConfig, ClientEncryption, RelayClient};
use tenet::crypto::{generate_content_key, generate_keypair, StoredKeypair, NONCE_SIZE};
use tenet::protocol::{
    build_encrypted_payload, build_envelope_from_payload, build_meta_payload,
    build_plaintext_envelope, decode_meta_payload, MessageKind, MetaMessage,
};
use tenet::storage::{db_path, IdentityRow, MessageRow, PeerRow, RelayRow, Storage};

// ---------------------------------------------------------------------------
// Embedded static assets
// ---------------------------------------------------------------------------

#[derive(Embed)]
#[folder = "web/dist/"]
struct Assets;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

const DEFAULT_TTL_SECONDS: u64 = 3600;
const SYNC_INTERVAL_SECS: u64 = 30;
const WEB_HPKE_INFO: &[u8] = b"tenet-web";
const WEB_PAYLOAD_AAD: &[u8] = b"tenet-web";
const WS_CHANNEL_CAPACITY: usize = 256;

struct Config {
    bind_addr: String,
    data_dir: PathBuf,
    relay_url: Option<String>,
}

impl Config {
    fn from_env() -> Self {
        let data_dir = std::env::var("TENET_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                std::env::var("HOME")
                    .map(|h| PathBuf::from(h).join(".tenet"))
                    .unwrap_or_else(|_| PathBuf::from(".tenet"))
            });

        Self {
            bind_addr: std::env::var("TENET_WEB_BIND")
                .unwrap_or_else(|_| "127.0.0.1:3000".to_string()),
            data_dir,
            relay_url: std::env::var("TENET_RELAY_URL").ok(),
        }
    }
}

// ---------------------------------------------------------------------------
// WebSocket event types
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(dead_code)]
enum WsEvent {
    NewMessage {
        message_id: String,
        sender_id: String,
        message_kind: String,
        body: Option<String>,
        timestamp: u64,
    },
    PeerOnline {
        peer_id: String,
    },
    PeerOffline {
        peer_id: String,
    },
    MessageRead {
        message_id: String,
    },
}

// ---------------------------------------------------------------------------
// Shared application state
// ---------------------------------------------------------------------------

struct AppState {
    storage: Storage,
    keypair: StoredKeypair,
    relay_url: Option<String>,
    ws_tx: broadcast::Sender<WsEvent>,
}

type SharedState = Arc<Mutex<AppState>>;

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    let config = Config::from_env();

    // Ensure data directory exists
    std::fs::create_dir_all(&config.data_dir).expect("failed to create data directory");

    // Open SQLite database
    let db = db_path(&config.data_dir);
    let storage = Storage::open(&db).expect("failed to open database");

    // Run JSON-to-SQLite migration if legacy files exist
    if config.data_dir.join("identity.json").exists() {
        match storage.migrate_from_json(&config.data_dir) {
            Ok(report) => {
                if !report.is_empty() {
                    eprintln!(
                        "migrated legacy data: identity={}, peers={}, inbox={}, outbox={}",
                        report.identity_migrated,
                        report.peers_migrated,
                        report.inbox_migrated,
                        report.outbox_migrated
                    );
                }
            }
            Err(e) => eprintln!("migration warning: {e}"),
        }
    }

    // Load or generate identity
    let keypair = match storage.get_identity().expect("failed to read identity") {
        Some(row) => {
            eprintln!("loaded identity: {}", row.id);
            row.to_stored_keypair()
        }
        None => {
            let kp = generate_keypair();
            let row = IdentityRow::from(&kp);
            storage
                .insert_identity(&row)
                .expect("failed to store identity");
            eprintln!("generated new identity: {}", kp.id);
            kp
        }
    };

    // Store relay URL in database if provided
    if let Some(ref url) = config.relay_url {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let _ = storage.insert_relay(&RelayRow {
            url: url.clone(),
            added_at: now,
            last_sync: None,
            enabled: true,
        });
    }

    // Create WebSocket broadcast channel
    let (ws_tx, _) = broadcast::channel(WS_CHANNEL_CAPACITY);

    let state: SharedState = Arc::new(Mutex::new(AppState {
        storage,
        keypair: keypair.clone(),
        relay_url: config.relay_url.clone(),
        ws_tx,
    }));

    // Start background relay sync task
    if config.relay_url.is_some() {
        let sync_state = Arc::clone(&state);
        tokio::spawn(async move {
            relay_sync_loop(sync_state).await;
        });

        // Send online announcement to known peers
        let announce_state = Arc::clone(&state);
        tokio::spawn(async move {
            if let Err(e) = announce_online(announce_state).await {
                eprintln!("failed to announce online status: {}", e);
            }
        });
    }

    // Build Axum router
    let app = Router::new()
        // Health
        .route("/api/health", get(health_handler))
        // Messages API (Phase 2)
        .route("/api/messages", get(list_messages_handler))
        .route("/api/messages/:message_id", get(get_message_handler))
        .route("/api/messages/direct", post(send_direct_handler))
        .route("/api/messages/public", post(send_public_handler))
        .route("/api/messages/group", post(send_group_handler))
        .route("/api/messages/:message_id/read", post(mark_read_handler))
        // Peers API (Phase 3)
        .route("/api/peers", get(list_peers_handler).post(add_peer_handler))
        .route(
            "/api/peers/:peer_id",
            get(get_peer_handler).delete(delete_peer_handler),
        )
        // Groups API (Phase 4)
        .route(
            "/api/groups",
            get(list_groups_handler).post(create_group_handler),
        )
        .route("/api/groups/:group_id", get(get_group_handler))
        .route(
            "/api/groups/:group_id/members",
            post(add_group_member_handler),
        )
        .route(
            "/api/groups/:group_id/members/:peer_id",
            axum::routing::delete(remove_group_member_handler),
        )
        .route("/api/groups/:group_id/leave", post(leave_group_handler))
        // WebSocket (Phase 2)
        .route("/api/ws", get(ws_handler))
        // Static fallback
        .fallback(get(static_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&config.bind_addr)
        .await
        .expect("failed to bind");
    eprintln!("tenet-web listening on {}", config.bind_addr);

    axum::serve(listener, app).await.expect("server error");
}

// ---------------------------------------------------------------------------
// API error helper
// ---------------------------------------------------------------------------

fn api_error(status: StatusCode, message: impl Into<String>) -> Response {
    let body = serde_json::json!({ "error": message.into() });
    (status, axum::Json(body)).into_response()
}

// ---------------------------------------------------------------------------
// Health handler
// ---------------------------------------------------------------------------

async fn health_handler(State(state): State<SharedState>) -> impl IntoResponse {
    let state = state.lock().await;
    let relay_status = state.relay_url.as_deref().unwrap_or("none");
    let peer_count = state.storage.list_peers().unwrap_or_default().len();
    let message_count = state
        .storage
        .list_messages(None, None, None, 1)
        .map(|m| if m.is_empty() { 0 } else { 1 })
        .unwrap_or(0);

    let body = serde_json::json!({
        "status": "ok",
        "peer_id": state.keypair.id,
        "relay": relay_status,
        "peers": peer_count,
        "has_messages": message_count > 0,
    });
    (StatusCode::OK, axum::Json(body))
}

// ---------------------------------------------------------------------------
// Messages API
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ListMessagesQuery {
    kind: Option<String>,
    group: Option<String>,
    before: Option<u64>,
    limit: Option<u32>,
}

async fn list_messages_handler(
    State(state): State<SharedState>,
    Query(params): Query<ListMessagesQuery>,
) -> Response {
    let st = state.lock().await;
    let limit = params.limit.unwrap_or(50).min(200);

    match st.storage.list_messages(
        params.kind.as_deref(),
        params.group.as_deref(),
        params.before,
        limit,
    ) {
        Ok(messages) => {
            let json: Vec<serde_json::Value> = messages
                .iter()
                .map(|m| {
                    serde_json::json!({
                        "message_id": m.message_id,
                        "sender_id": m.sender_id,
                        "recipient_id": m.recipient_id,
                        "message_kind": m.message_kind,
                        "group_id": m.group_id,
                        "body": m.body,
                        "timestamp": m.timestamp,
                        "received_at": m.received_at,
                        "is_read": m.is_read,
                    })
                })
                .collect();
            (StatusCode::OK, axum::Json(serde_json::json!(json))).into_response()
        }
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn get_message_handler(
    State(state): State<SharedState>,
    Path(message_id): Path<String>,
) -> Response {
    let st = state.lock().await;
    match st.storage.get_message(&message_id) {
        Ok(Some(m)) => {
            let json = serde_json::json!({
                "message_id": m.message_id,
                "sender_id": m.sender_id,
                "recipient_id": m.recipient_id,
                "message_kind": m.message_kind,
                "group_id": m.group_id,
                "body": m.body,
                "timestamp": m.timestamp,
                "received_at": m.received_at,
                "is_read": m.is_read,
            });
            (StatusCode::OK, axum::Json(json)).into_response()
        }
        Ok(None) => api_error(StatusCode::NOT_FOUND, "message not found"),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

// -- Send direct message --

#[derive(Deserialize)]
struct SendDirectRequest {
    recipient_id: String,
    body: String,
}

async fn send_direct_handler(
    State(state): State<SharedState>,
    axum::Json(req): axum::Json<SendDirectRequest>,
) -> Response {
    if req.body.trim().is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "body cannot be empty");
    }

    let st = state.lock().await;

    // Look up recipient's public key
    let peer = match st.storage.get_peer(&req.recipient_id) {
        Ok(Some(p)) => p,
        Ok(None) => return api_error(StatusCode::NOT_FOUND, "recipient peer not found"),
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };

    let recipient_enc_key = match peer.encryption_public_key.as_deref() {
        Some(k) => k.to_string(),
        None => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "recipient has no encryption key; cannot send encrypted message",
            )
        }
    };

    let now = now_secs();

    // Build encrypted envelope
    let content_key = generate_content_key();
    let mut nonce = [0u8; NONCE_SIZE];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut nonce);

    let payload = match build_encrypted_payload(
        req.body.as_bytes(),
        &recipient_enc_key,
        WEB_PAYLOAD_AAD,
        WEB_HPKE_INFO,
        &content_key,
        &nonce,
        None,
    ) {
        Ok(p) => p,
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("crypto: {e}")),
    };

    let envelope = match build_envelope_from_payload(
        st.keypair.id.clone(),
        req.recipient_id.clone(),
        None,
        None,
        now,
        DEFAULT_TTL_SECONDS,
        MessageKind::Direct,
        None,
        payload,
        &st.keypair.signing_private_key_hex,
    ) {
        Ok(e) => e,
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("envelope: {e}")),
    };

    // Post to relay
    if let Some(ref relay_url) = st.relay_url {
        let url = format!("{}/envelopes", relay_url.trim_end_matches('/'));
        if let Ok(json_val) = serde_json::to_value(&envelope) {
            let _ = ureq::post(&url).send_json(json_val);
        }
    }

    let msg_id = envelope.header.message_id.0.clone();

    // Persist to outbox and messages
    let _ = st.storage.insert_outbox(&tenet::storage::OutboxRow {
        message_id: msg_id.clone(),
        envelope: serde_json::to_string(&envelope).unwrap_or_default(),
        sent_at: now,
        delivered: false,
    });

    let _ = st.storage.insert_message(&MessageRow {
        message_id: msg_id.clone(),
        sender_id: st.keypair.id.clone(),
        recipient_id: req.recipient_id.clone(),
        message_kind: "direct".to_string(),
        group_id: None,
        body: Some(req.body.clone()),
        timestamp: now,
        received_at: now,
        ttl_seconds: DEFAULT_TTL_SECONDS,
        is_read: true,
        raw_envelope: None,
    });

    // Broadcast to WebSocket clients
    let _ = st.ws_tx.send(WsEvent::NewMessage {
        message_id: msg_id.clone(),
        sender_id: st.keypair.id.clone(),
        message_kind: "direct".to_string(),
        body: Some(req.body),
        timestamp: now,
    });

    let json = serde_json::json!({
        "message_id": msg_id,
        "status": "sent",
    });
    (StatusCode::CREATED, axum::Json(json)).into_response()
}

// -- Send public message --

#[derive(Deserialize)]
struct SendPublicRequest {
    body: String,
}

async fn send_public_handler(
    State(state): State<SharedState>,
    axum::Json(req): axum::Json<SendPublicRequest>,
) -> Response {
    if req.body.trim().is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "body cannot be empty");
    }

    let st = state.lock().await;
    let now = now_secs();

    let mut salt = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut salt);

    let envelope = match build_plaintext_envelope(
        st.keypair.id.as_str(),
        "*",
        None,
        None,
        now,
        DEFAULT_TTL_SECONDS,
        MessageKind::Public,
        None,
        &req.body,
        salt,
        &st.keypair.signing_private_key_hex,
    ) {
        Ok(e) => e,
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("envelope: {e}")),
    };

    // Post to relay
    if let Some(ref relay_url) = st.relay_url {
        let url = format!("{}/envelopes", relay_url.trim_end_matches('/'));
        if let Ok(json_val) = serde_json::to_value(&envelope) {
            let _ = ureq::post(&url).send_json(json_val);
        }
    }

    let msg_id = envelope.header.message_id.0.clone();

    // Persist
    let _ = st.storage.insert_outbox(&tenet::storage::OutboxRow {
        message_id: msg_id.clone(),
        envelope: serde_json::to_string(&envelope).unwrap_or_default(),
        sent_at: now,
        delivered: false,
    });

    let _ = st.storage.insert_message(&MessageRow {
        message_id: msg_id.clone(),
        sender_id: st.keypair.id.clone(),
        recipient_id: "*".to_string(),
        message_kind: "public".to_string(),
        group_id: None,
        body: Some(req.body.clone()),
        timestamp: now,
        received_at: now,
        ttl_seconds: DEFAULT_TTL_SECONDS,
        is_read: true,
        raw_envelope: None,
    });

    // Broadcast to WS
    let _ = st.ws_tx.send(WsEvent::NewMessage {
        message_id: msg_id.clone(),
        sender_id: st.keypair.id.clone(),
        message_kind: "public".to_string(),
        body: Some(req.body),
        timestamp: now,
    });

    let json = serde_json::json!({
        "message_id": msg_id,
        "status": "sent",
    });
    (StatusCode::CREATED, axum::Json(json)).into_response()
}

// -- Send group message --

#[derive(Deserialize)]
struct SendGroupRequest {
    group_id: String,
    body: String,
}

async fn send_group_handler(
    State(state): State<SharedState>,
    axum::Json(req): axum::Json<SendGroupRequest>,
) -> Response {
    if req.body.trim().is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "body cannot be empty");
    }

    let st = state.lock().await;
    let now = now_secs();

    // Look up group key from storage
    let group = match st.storage.get_group(&req.group_id) {
        Ok(Some(g)) => g,
        Ok(None) => return api_error(StatusCode::NOT_FOUND, "group not found"),
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };

    let group_key: [u8; 32] = match group.group_key.try_into() {
        Ok(k) => k,
        Err(_) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, "invalid group key"),
    };

    let aad = req.group_id.as_bytes();
    let payload =
        match tenet::protocol::build_group_message_payload(req.body.as_bytes(), &group_key, aad) {
            Ok(p) => p,
            Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("crypto: {e}")),
        };

    let envelope = match build_envelope_from_payload(
        st.keypair.id.clone(),
        "*".to_string(),
        None,
        None,
        now,
        DEFAULT_TTL_SECONDS,
        MessageKind::FriendGroup,
        Some(req.group_id.clone()),
        payload,
        &st.keypair.signing_private_key_hex,
    ) {
        Ok(e) => e,
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("envelope: {e}")),
    };

    // Post to relay
    if let Some(ref relay_url) = st.relay_url {
        let url = format!("{}/envelopes", relay_url.trim_end_matches('/'));
        if let Ok(json_val) = serde_json::to_value(&envelope) {
            let _ = ureq::post(&url).send_json(json_val);
        }
    }

    let msg_id = envelope.header.message_id.0.clone();

    // Persist
    let _ = st.storage.insert_outbox(&tenet::storage::OutboxRow {
        message_id: msg_id.clone(),
        envelope: serde_json::to_string(&envelope).unwrap_or_default(),
        sent_at: now,
        delivered: false,
    });

    let _ = st.storage.insert_message(&MessageRow {
        message_id: msg_id.clone(),
        sender_id: st.keypair.id.clone(),
        recipient_id: "*".to_string(),
        message_kind: "friend_group".to_string(),
        group_id: Some(req.group_id.clone()),
        body: Some(req.body.clone()),
        timestamp: now,
        received_at: now,
        ttl_seconds: DEFAULT_TTL_SECONDS,
        is_read: true,
        raw_envelope: None,
    });

    // Broadcast to WS
    let _ = st.ws_tx.send(WsEvent::NewMessage {
        message_id: msg_id.clone(),
        sender_id: st.keypair.id.clone(),
        message_kind: "friend_group".to_string(),
        body: Some(req.body),
        timestamp: now,
    });

    let json = serde_json::json!({
        "message_id": msg_id,
        "status": "sent",
    });
    (StatusCode::CREATED, axum::Json(json)).into_response()
}

// -- Mark message as read --

async fn mark_read_handler(
    State(state): State<SharedState>,
    Path(message_id): Path<String>,
) -> Response {
    let st = state.lock().await;
    match st.storage.mark_message_read(&message_id) {
        Ok(true) => {
            let _ = st.ws_tx.send(WsEvent::MessageRead {
                message_id: message_id.clone(),
            });
            (
                StatusCode::OK,
                axum::Json(serde_json::json!({"status": "ok"})),
            )
                .into_response()
        }
        Ok(false) => api_error(StatusCode::NOT_FOUND, "message not found"),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Peers API
// ---------------------------------------------------------------------------

async fn list_peers_handler(State(state): State<SharedState>) -> Response {
    let st = state.lock().await;
    match st.storage.list_peers() {
        Ok(peers) => {
            let json: Vec<serde_json::Value> = peers
                .iter()
                .map(|p| {
                    serde_json::json!({
                        "peer_id": p.peer_id,
                        "display_name": p.display_name,
                        "signing_public_key": p.signing_public_key,
                        "encryption_public_key": p.encryption_public_key,
                        "added_at": p.added_at,
                        "is_friend": p.is_friend,
                        "last_seen_online": p.last_seen_online,
                        "online": p.online,
                    })
                })
                .collect();
            (StatusCode::OK, axum::Json(serde_json::json!(json))).into_response()
        }
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn get_peer_handler(
    State(state): State<SharedState>,
    Path(peer_id): Path<String>,
) -> Response {
    let st = state.lock().await;
    match st.storage.get_peer(&peer_id) {
        Ok(Some(p)) => {
            let json = serde_json::json!({
                "peer_id": p.peer_id,
                "display_name": p.display_name,
                "signing_public_key": p.signing_public_key,
                "encryption_public_key": p.encryption_public_key,
                "added_at": p.added_at,
                "is_friend": p.is_friend,
                "last_seen_online": p.last_seen_online,
                "online": p.online,
            });
            (StatusCode::OK, axum::Json(json)).into_response()
        }
        Ok(None) => api_error(StatusCode::NOT_FOUND, "peer not found"),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize)]
struct AddPeerRequest {
    peer_id: String,
    display_name: Option<String>,
    signing_public_key: String,
    encryption_public_key: Option<String>,
}

async fn add_peer_handler(
    State(state): State<SharedState>,
    axum::Json(req): axum::Json<AddPeerRequest>,
) -> Response {
    if req.peer_id.trim().is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "peer_id cannot be empty");
    }
    if req.signing_public_key.trim().is_empty() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "signing_public_key cannot be empty",
        );
    }

    let now = now_secs();
    let st = state.lock().await;

    let peer_row = PeerRow {
        peer_id: req.peer_id.clone(),
        display_name: req.display_name.clone(),
        signing_public_key: req.signing_public_key.clone(),
        encryption_public_key: req.encryption_public_key.clone(),
        added_at: now,
        is_friend: true,
        last_seen_online: None,
        online: false,
    };

    match st.storage.insert_peer(&peer_row) {
        Ok(()) => {
            let json = serde_json::json!({
                "peer_id": peer_row.peer_id,
                "display_name": peer_row.display_name,
                "signing_public_key": peer_row.signing_public_key,
                "encryption_public_key": peer_row.encryption_public_key,
                "added_at": peer_row.added_at,
                "is_friend": peer_row.is_friend,
                "last_seen_online": peer_row.last_seen_online,
                "online": peer_row.online,
            });
            (StatusCode::CREATED, axum::Json(json)).into_response()
        }
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn delete_peer_handler(
    State(state): State<SharedState>,
    Path(peer_id): Path<String>,
) -> Response {
    let st = state.lock().await;
    match st.storage.delete_peer(&peer_id) {
        Ok(true) => (
            StatusCode::OK,
            axum::Json(serde_json::json!({"status": "deleted"})),
        )
            .into_response(),
        Ok(false) => api_error(StatusCode::NOT_FOUND, "peer not found"),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Groups API
// ---------------------------------------------------------------------------

async fn list_groups_handler(State(state): State<SharedState>) -> Response {
    let st = state.lock().await;
    match st.storage.list_groups() {
        Ok(groups) => {
            let json: Vec<serde_json::Value> = groups
                .iter()
                .map(|g| {
                    serde_json::json!({
                        "group_id": g.group_id,
                        "creator_id": g.creator_id,
                        "created_at": g.created_at,
                        "key_version": g.key_version,
                    })
                })
                .collect();
            (StatusCode::OK, axum::Json(serde_json::json!(json))).into_response()
        }
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn get_group_handler(
    State(state): State<SharedState>,
    Path(group_id): Path<String>,
) -> Response {
    let st = state.lock().await;
    match st.storage.get_group(&group_id) {
        Ok(Some(g)) => {
            // Get group members
            let members = st.storage.list_group_members(&group_id).unwrap_or_default();
            let member_ids: Vec<String> = members.iter().map(|m| m.peer_id.clone()).collect();

            let json = serde_json::json!({
                "group_id": g.group_id,
                "creator_id": g.creator_id,
                "created_at": g.created_at,
                "key_version": g.key_version,
                "members": member_ids,
            });
            (StatusCode::OK, axum::Json(json)).into_response()
        }
        Ok(None) => api_error(StatusCode::NOT_FOUND, "group not found"),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize)]
struct CreateGroupRequest {
    group_id: String,
    member_ids: Vec<String>,
}

async fn create_group_handler(
    State(state): State<SharedState>,
    axum::Json(req): axum::Json<CreateGroupRequest>,
) -> Response {
    if req.group_id.trim().is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "group_id cannot be empty");
    }

    let st = state.lock().await;
    let now = now_secs();

    // Generate a new symmetric key for the group
    let mut group_key = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut group_key);

    // Store group in database
    let group_row = tenet::storage::GroupRow {
        group_id: req.group_id.clone(),
        group_key: group_key.to_vec(),
        creator_id: st.keypair.id.clone(),
        created_at: now,
        key_version: 1,
    };

    if let Err(e) = st.storage.insert_group(&group_row) {
        return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }

    // Add members to group (including creator)
    let mut all_members = req.member_ids.clone();
    if !all_members.contains(&st.keypair.id) {
        all_members.push(st.keypair.id.clone());
    }

    for member_id in &all_members {
        let member_row = tenet::storage::GroupMemberRow {
            group_id: req.group_id.clone(),
            peer_id: member_id.clone(),
            joined_at: now,
        };
        if let Err(e) = st.storage.insert_group_member(&member_row) {
            eprintln!("failed to add group member {}: {}", member_id, e);
        }
    }

    // Distribute group key to each member (except creator) via encrypted direct messages
    for member_id in &req.member_ids {
        if member_id == &st.keypair.id {
            continue; // Skip creator
        }

        // Look up member's public key
        let peer = match st.storage.get_peer(member_id) {
            Ok(Some(p)) => p,
            Ok(None) => {
                eprintln!("peer not found during key distribution: {}", member_id);
                continue;
            }
            Err(e) => {
                eprintln!("error fetching peer {}: {}", member_id, e);
                continue;
            }
        };

        let recipient_enc_key = match peer.encryption_public_key.as_deref() {
            Some(k) => k.to_string(),
            None => {
                eprintln!("peer {} has no encryption key; skipping", member_id);
                continue;
            }
        };

        // Build a group key distribution message payload
        let key_distribution = serde_json::json!({
            "type": "group_key_distribution",
            "group_id": req.group_id,
            "group_key": hex::encode(group_key),
            "key_version": 1,
            "creator_id": st.keypair.id,
        });

        let key_dist_bytes = serde_json::to_vec(&key_distribution).unwrap_or_default();

        // Encrypt the key distribution message
        let content_key = generate_content_key();
        let mut nonce = [0u8; NONCE_SIZE];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut nonce);

        let payload = match build_encrypted_payload(
            &key_dist_bytes,
            &recipient_enc_key,
            WEB_PAYLOAD_AAD,
            WEB_HPKE_INFO,
            &content_key,
            &nonce,
            None,
        ) {
            Ok(p) => p,
            Err(e) => {
                eprintln!(
                    "failed to encrypt key distribution for {}: {}",
                    member_id, e
                );
                continue;
            }
        };

        let envelope = match build_envelope_from_payload(
            st.keypair.id.clone(),
            member_id.clone(),
            None,
            None,
            now,
            DEFAULT_TTL_SECONDS,
            MessageKind::Direct,
            None,
            payload,
            &st.keypair.signing_private_key_hex,
        ) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("failed to build envelope for {}: {}", member_id, e);
                continue;
            }
        };

        // Post to relay
        if let Some(ref relay_url) = st.relay_url {
            let url = format!("{}/envelopes", relay_url.trim_end_matches('/'));
            if let Ok(json_val) = serde_json::to_value(&envelope) {
                let _ = ureq::post(&url).send_json(json_val);
            }
        }
    }

    let json = serde_json::json!({
        "group_id": req.group_id,
        "creator_id": st.keypair.id,
        "created_at": now,
        "key_version": 1,
        "members": all_members,
    });
    (StatusCode::CREATED, axum::Json(json)).into_response()
}

#[derive(Deserialize)]
struct AddGroupMemberRequest {
    peer_id: String,
}

async fn add_group_member_handler(
    State(state): State<SharedState>,
    Path(group_id): Path<String>,
    axum::Json(req): axum::Json<AddGroupMemberRequest>,
) -> Response {
    if req.peer_id.trim().is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "peer_id cannot be empty");
    }

    let st = state.lock().await;
    let now = now_secs();

    // Check if group exists
    let group = match st.storage.get_group(&group_id) {
        Ok(Some(g)) => g,
        Ok(None) => return api_error(StatusCode::NOT_FOUND, "group not found"),
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };

    // Add member to database
    let member_row = tenet::storage::GroupMemberRow {
        group_id: group_id.clone(),
        peer_id: req.peer_id.clone(),
        joined_at: now,
    };

    if let Err(e) = st.storage.insert_group_member(&member_row) {
        return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }

    // Send group key to new member via encrypted direct message
    let peer = match st.storage.get_peer(&req.peer_id) {
        Ok(Some(p)) => p,
        Ok(None) => return api_error(StatusCode::NOT_FOUND, "peer not found"),
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };

    let recipient_enc_key = match peer.encryption_public_key.as_deref() {
        Some(k) => k.to_string(),
        None => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "peer has no encryption key; cannot distribute group key",
            )
        }
    };

    let group_key: [u8; 32] = match group.group_key.try_into() {
        Ok(k) => k,
        Err(_) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, "invalid group key"),
    };

    // Build a group key distribution message
    let key_distribution = serde_json::json!({
        "type": "group_key_distribution",
        "group_id": group_id,
        "group_key": hex::encode(group_key),
        "key_version": group.key_version,
        "creator_id": group.creator_id,
    });

    let key_dist_bytes = serde_json::to_vec(&key_distribution).unwrap_or_default();

    let content_key = generate_content_key();
    let mut nonce = [0u8; NONCE_SIZE];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut nonce);

    let payload = match build_encrypted_payload(
        &key_dist_bytes,
        &recipient_enc_key,
        WEB_PAYLOAD_AAD,
        WEB_HPKE_INFO,
        &content_key,
        &nonce,
        None,
    ) {
        Ok(p) => p,
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("crypto: {e}")),
    };

    let envelope = match build_envelope_from_payload(
        st.keypair.id.clone(),
        req.peer_id.clone(),
        None,
        None,
        now,
        DEFAULT_TTL_SECONDS,
        MessageKind::Direct,
        None,
        payload,
        &st.keypair.signing_private_key_hex,
    ) {
        Ok(e) => e,
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("envelope: {e}")),
    };

    // Post to relay
    if let Some(ref relay_url) = st.relay_url {
        let url = format!("{}/envelopes", relay_url.trim_end_matches('/'));
        if let Ok(json_val) = serde_json::to_value(&envelope) {
            let _ = ureq::post(&url).send_json(json_val);
        }
    }

    let json = serde_json::json!({
        "status": "added",
        "group_id": group_id,
        "peer_id": req.peer_id,
    });
    (StatusCode::OK, axum::Json(json)).into_response()
}

async fn remove_group_member_handler(
    State(state): State<SharedState>,
    Path((group_id, peer_id)): Path<(String, String)>,
) -> Response {
    let st = state.lock().await;

    match st.storage.remove_group_member(&group_id, &peer_id) {
        Ok(true) => {
            // TODO: Implement key rotation for remaining members
            let json = serde_json::json!({
                "status": "removed",
                "group_id": group_id,
                "peer_id": peer_id,
            });
            (StatusCode::OK, axum::Json(json)).into_response()
        }
        Ok(false) => api_error(StatusCode::NOT_FOUND, "group member not found"),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn leave_group_handler(
    State(state): State<SharedState>,
    Path(group_id): Path<String>,
) -> Response {
    let st = state.lock().await;

    match st.storage.remove_group_member(&group_id, &st.keypair.id) {
        Ok(true) => {
            // TODO: Implement key rotation for remaining members
            let json = serde_json::json!({
                "status": "left",
                "group_id": group_id,
            });
            (StatusCode::OK, axum::Json(json)).into_response()
        }
        Ok(false) => api_error(StatusCode::NOT_FOUND, "not a member of this group"),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

// ---------------------------------------------------------------------------
// WebSocket handler
// ---------------------------------------------------------------------------

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<SharedState>) -> impl IntoResponse {
    ws.on_upgrade(|socket| ws_connection(socket, state))
}

async fn ws_connection(mut socket: WebSocket, state: SharedState) {
    // Subscribe to the broadcast channel
    let mut rx = {
        let st = state.lock().await;
        st.ws_tx.subscribe()
    };

    loop {
        tokio::select! {
            // Forward broadcast events to the WebSocket client
            result = rx.recv() => {
                match result {
                    Ok(event) => {
                        if let Ok(json) = serde_json::to_string(&event) {
                            if socket.send(WsMessage::Text(json)).await.is_err() {
                                break; // client disconnected
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // Client too slow, skip missed events
                        eprintln!("ws client lagged, skipped {n} events");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            // Handle incoming messages from the client (for future use)
            msg = socket.recv() => {
                match msg {
                    Some(Ok(WsMessage::Close(_))) | None => break,
                    Some(Ok(WsMessage::Ping(data))) => {
                        let _ = socket.send(WsMessage::Pong(data)).await;
                    }
                    _ => {} // ignore other client messages for now
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Static asset handler
// ---------------------------------------------------------------------------

async fn static_handler(uri: axum::http::Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    match Assets::get(path) {
        Some(content) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, mime.as_ref())],
                content.data.to_vec(),
            )
                .into_response()
        }
        None => {
            // SPA fallback: serve index.html for unmatched routes
            match Assets::get("index.html") {
                Some(content) => (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "text/html")],
                    content.data.to_vec(),
                )
                    .into_response(),
                None => (StatusCode::NOT_FOUND, "not found").into_response(),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Background relay sync
// ---------------------------------------------------------------------------

async fn relay_sync_loop(state: SharedState) {
    let mut interval = tokio::time::interval(Duration::from_secs(SYNC_INTERVAL_SECS));

    loop {
        interval.tick().await;

        if let Err(e) = sync_once(&state).await {
            eprintln!("relay sync error: {e}");
        }
    }
}

async fn sync_once(state: &SharedState) -> Result<(), String> {
    // Extract what we need from state under a short lock
    let (keypair, relay_url, peers) = {
        let st = state.lock().await;
        let relay_url = st
            .relay_url
            .clone()
            .ok_or_else(|| "no relay configured".to_string())?;
        let peers = st.storage.list_peers().map_err(|e| e.to_string())?;
        (st.keypair.clone(), relay_url, peers)
    };

    // Build a RelayClient for fetching
    let config = ClientConfig::new(
        relay_url.clone(),
        DEFAULT_TTL_SECONDS,
        ClientEncryption::Encrypted {
            hpke_info: WEB_HPKE_INFO.to_vec(),
            payload_aad: WEB_PAYLOAD_AAD.to_vec(),
        },
    );
    let mut client = RelayClient::new(keypair.clone(), config);

    // Register known peers so signature verification works
    for peer in &peers {
        if let Some(ref enc_key) = peer.encryption_public_key {
            client.add_peer_with_encryption(
                peer.peer_id.clone(),
                peer.signing_public_key.clone(),
                enc_key.clone(),
            );
        } else {
            client.add_peer(peer.peer_id.clone(), peer.signing_public_key.clone());
        }
    }

    // Fetch inbox from relay
    let outcome = client.sync_inbox(None).map_err(|e| e.to_string())?;

    // Fetch raw envelopes to check for Meta messages
    let base = relay_url.trim_end_matches('/');
    let inbox_url = format!("{}/inbox/{}", base, keypair.id);
    let envelopes: Vec<tenet::protocol::Envelope> = ureq::get(&inbox_url)
        .call()
        .ok()
        .and_then(|response| response.into_json().ok())
        .unwrap_or_default();

    // Process Meta messages for presence tracking
    let now = now_secs();
    for envelope in &envelopes {
        if envelope.header.message_kind == MessageKind::Meta {
            // Try to decode as Meta message
            if let Ok(meta_msg) = decode_meta_payload(&envelope.payload) {
                match meta_msg {
                    MetaMessage::Online { peer_id, timestamp } => {
                        // Update peer presence
                        let st = state.lock().await;
                        if let Ok(true) = st.storage.update_peer_online(&peer_id, true, timestamp) {
                            // Broadcast peer_online event
                            let _ = st.ws_tx.send(WsEvent::PeerOnline {
                                peer_id: peer_id.clone(),
                            });
                            eprintln!("peer {} is now online", peer_id);
                        }
                    }
                    MetaMessage::Ack {
                        peer_id,
                        online_timestamp,
                    } => {
                        // Update peer last_seen
                        let st = state.lock().await;
                        let _ = st
                            .storage
                            .update_peer_online(&peer_id, true, online_timestamp);
                    }
                    _ => {} // Ignore other meta message types for now
                }
            }
        }
    }

    if outcome.fetched == 0 {
        return Ok(());
    }

    // Store received messages in SQLite and broadcast via WebSocket
    let st = state.lock().await;

    for msg in &outcome.messages {
        if st.storage.has_message(&msg.message_id).unwrap_or(true) {
            continue;
        }

        let row = MessageRow {
            message_id: msg.message_id.clone(),
            sender_id: msg.sender_id.clone(),
            recipient_id: keypair.id.clone(),
            message_kind: "direct".to_string(),
            group_id: None,
            body: Some(msg.body.clone()),
            timestamp: msg.timestamp,
            received_at: now,
            ttl_seconds: DEFAULT_TTL_SECONDS,
            is_read: false,
            raw_envelope: None,
        };
        if st.storage.insert_message(&row).is_ok() {
            // Push to WebSocket clients
            let _ = st.ws_tx.send(WsEvent::NewMessage {
                message_id: msg.message_id.clone(),
                sender_id: msg.sender_id.clone(),
                message_kind: "direct".to_string(),
                body: Some(msg.body.clone()),
                timestamp: msg.timestamp,
            });
        }
    }

    // Update relay last_sync timestamp
    let _ = st.storage.update_relay_last_sync(&relay_url, now);

    if !outcome.messages.is_empty() {
        eprintln!(
            "synced {} messages ({} errors)",
            outcome.messages.len(),
            outcome.errors.len()
        );
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Online announcement
// ---------------------------------------------------------------------------

async fn announce_online(state: SharedState) -> Result<(), String> {
    let (keypair, relay_url, peers) = {
        let st = state.lock().await;
        let relay_url = st
            .relay_url
            .clone()
            .ok_or_else(|| "no relay configured".to_string())?;
        let peers = st.storage.list_peers().map_err(|e| e.to_string())?;
        (st.keypair.clone(), relay_url, peers)
    };

    if peers.is_empty() {
        return Ok(());
    }

    let now = now_secs();
    let meta_msg = MetaMessage::Online {
        peer_id: keypair.id.clone(),
        timestamp: now,
    };

    let payload = build_meta_payload(&meta_msg).map_err(|e| e.to_string())?;

    // Send online announcement to each peer
    for peer in &peers {
        let envelope = build_envelope_from_payload(
            keypair.id.clone(),
            peer.peer_id.clone(),
            None,
            None,
            now,
            DEFAULT_TTL_SECONDS,
            MessageKind::Meta,
            None,
            payload.clone(),
            &keypair.signing_private_key_hex,
        )
        .map_err(|e| e.to_string())?;

        // Post to relay
        let url = format!("{}/envelopes", relay_url.trim_end_matches('/'));
        if let Ok(json_val) = serde_json::to_value(&envelope) {
            let _ = ureq::post(&url).send_json(json_val);
        }
    }

    eprintln!("announced online status to {} peers", peers.len());
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
