//! tenet-web: Web server binary that acts as a full tenet peer.
//!
//! Serves an embedded SPA, provides REST API + WebSocket for messages,
//! connects to relays, and persists state in SQLite.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use axum_extra::extract::Multipart;
use clap::Parser;
use rust_embed::Embed;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::sync::{broadcast, Mutex};

use base64::Engine as _;

use tenet::crypto::{generate_content_key, StoredKeypair, NONCE_SIZE};
use tenet::identity::{resolve_identity, store_relay_for_identity};
use tenet::protocol::{
    build_encrypted_payload, build_envelope_from_payload, build_meta_payload,
    build_plaintext_envelope, decode_meta_payload, decrypt_encrypted_payload, MessageKind,
    MetaMessage,
};
use tenet::storage::{
    db_path, AttachmentRow, FriendRequestRow, MessageAttachmentRow, MessageRow, PeerRow,
    ProfileRow, ReactionRow, Storage,
};

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
const MAX_WS_CONNECTIONS: usize = 8;
const MAX_ATTACHMENT_SIZE: u64 = 10 * 1024 * 1024; // 10 MB per attachment

/// Web server for the Tenet peer-to-peer social network.
///
/// Serves an embedded SPA, provides REST API + WebSocket for messages,
/// connects to relays, and persists state in SQLite.
///
/// Configuration can be set via CLI arguments or environment variables.
/// CLI arguments take precedence over environment variables.
#[derive(Parser, Debug)]
#[command(name = "tenet-web", version, about)]
struct Cli {
    /// HTTP server bind address [env: TENET_WEB_BIND] [default: 127.0.0.1:3000]
    #[arg(long, short = 'b')]
    bind: Option<String>,

    /// Data directory for identity and database [env: TENET_HOME] [default: ~/.tenet]
    #[arg(long, short = 'd')]
    data_dir: Option<PathBuf>,

    /// Relay server URL for fetching and posting messages [env: TENET_RELAY_URL]
    #[arg(long, short = 'r')]
    relay_url: Option<String>,

    /// Identity to use (short ID prefix) [env: TENET_IDENTITY]
    #[arg(long, short = 'i')]
    identity: Option<String>,
}

struct Config {
    bind_addr: String,
    data_dir: PathBuf,
    relay_url: Option<String>,
    identity: Option<String>,
}

impl Config {
    fn from_cli_and_env(cli: Cli) -> Self {
        let data_dir = cli
            .data_dir
            .or_else(|| std::env::var("TENET_HOME").ok().map(PathBuf::from))
            .unwrap_or_else(|| {
                std::env::var("HOME")
                    .map(|h| PathBuf::from(h).join(".tenet"))
                    .unwrap_or_else(|_| PathBuf::from(".tenet"))
            });

        let bind_addr = cli
            .bind
            .or_else(|| std::env::var("TENET_WEB_BIND").ok())
            .unwrap_or_else(|| "127.0.0.1:3000".to_string());

        let relay_url = cli
            .relay_url
            .or_else(|| std::env::var("TENET_RELAY_URL").ok());

        let identity = cli
            .identity
            .or_else(|| std::env::var("TENET_IDENTITY").ok());

        Self {
            bind_addr,
            data_dir,
            relay_url,
            identity,
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
    RelayStatus {
        connected: bool,
        relay_url: Option<String>,
    },
    FriendRequestReceived {
        request_id: i64,
        from_peer_id: String,
        message: Option<String>,
    },
    FriendRequestAccepted {
        request_id: i64,
        from_peer_id: String,
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
    ws_connection_count: Arc<AtomicUsize>,
    relay_connected: Arc<AtomicBool>,
}

type SharedState = Arc<Mutex<AppState>>;

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let config = Config::from_cli_and_env(cli);

    tenet::logging::init();

    tenet::tlog!("tenet-web starting");
    tenet::tlog!("  data directory: {}", config.data_dir.display());

    // Resolve identity (handles migration, creation, and selection)
    let resolved = resolve_identity(&config.data_dir, config.identity.as_deref())
        .expect("failed to resolve identity");

    let keypair = resolved.keypair;
    let storage = resolved.storage;

    if resolved.newly_created {
        tenet::tlog!(
            "  identities: 1 available (newly created), using: {}",
            tenet::logging::peer_id(&keypair.id)
        );
    } else {
        tenet::tlog!(
            "  identities: {} available, using: {}",
            resolved.total_identities,
            tenet::logging::peer_id(&keypair.id)
        );
    }
    tenet::tlog!("  database: {}", db_path(&resolved.identity_dir).display());

    // Resolve relay URL: CLI/env > stored in identity DB > none
    let relay_url = config.relay_url.or(resolved.stored_relay_url);

    // Store relay URL in identity's database if provided via CLI/env
    if let Some(ref url) = relay_url {
        let _ = store_relay_for_identity(&storage, url);
    }

    match &relay_url {
        Some(url) => tenet::tlog!("  relay: {}", url),
        None => tenet::tlog!("  relay: none configured (messages will be local-only)"),
    }

    // Create WebSocket broadcast channel
    let (ws_tx, _) = broadcast::channel(WS_CHANNEL_CAPACITY);

    let ws_connection_count = Arc::new(AtomicUsize::new(0));
    let relay_connected = Arc::new(AtomicBool::new(false));

    let state: SharedState = Arc::new(Mutex::new(AppState {
        storage,
        keypair: keypair.clone(),
        relay_url: relay_url.clone(),
        ws_tx,
        ws_connection_count,
        relay_connected: Arc::clone(&relay_connected),
    }));

    // Start background relay sync task
    if relay_url.is_some() {
        // Attempt an initial connectivity check before starting the server
        let check_state = Arc::clone(&state);
        match sync_once(&check_state).await {
            Ok(()) => {
                relay_connected.store(true, Ordering::Relaxed);
                tenet::tlog!("  relay status: connected");
            }
            Err(e) => {
                tenet::tlog!("  WARNING: relay is unreachable: {}", e);
                tenet::tlog!("  The web UI will show the relay as unavailable.");
                tenet::tlog!("  Background sync will retry with exponential backoff.");
            }
        }

        let sync_state = Arc::clone(&state);
        tokio::spawn(async move {
            relay_sync_loop(sync_state).await;
        });

        // Send online announcement to known peers
        let announce_state = Arc::clone(&state);
        tokio::spawn(async move {
            if let Err(e) = announce_online(announce_state).await {
                tenet::tlog!("failed to announce online status: {}", e);
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
        // Attachments API (Phase 7)
        .route(
            "/api/attachments",
            post(upload_attachment_handler)
                .layer(DefaultBodyLimit::max(MAX_ATTACHMENT_SIZE as usize + 4096)),
        )
        .route(
            "/api/attachments/:content_hash",
            get(download_attachment_handler),
        )
        // Reactions API (Phase 8)
        .route(
            "/api/messages/:message_id/react",
            post(react_handler).delete(unreact_handler),
        )
        .route(
            "/api/messages/:message_id/reactions",
            get(list_reactions_handler),
        )
        // Replies API (Phase 9)
        .route(
            "/api/messages/:message_id/replies",
            get(list_replies_handler),
        )
        .route("/api/messages/:message_id/reply", post(reply_handler))
        // Profiles API (Phase 10)
        .route(
            "/api/profile",
            get(get_own_profile_handler).put(update_own_profile_handler),
        )
        .route("/api/peers/:peer_id/profile", get(get_peer_profile_handler))
        // Friend Requests API
        .route(
            "/api/friend-requests",
            get(list_friend_requests_handler).post(send_friend_request_handler),
        )
        .route(
            "/api/friend-requests/:id/accept",
            post(accept_friend_request_handler),
        )
        .route(
            "/api/friend-requests/:id/ignore",
            post(ignore_friend_request_handler),
        )
        .route(
            "/api/friend-requests/:id/block",
            post(block_friend_request_handler),
        )
        // Conversations API (Phase 5)
        .route("/api/conversations", get(list_conversations_handler))
        .route("/api/conversations/:peer_id", get(get_conversation_handler))
        // WebSocket (Phase 2)
        .route("/api/ws", get(ws_handler))
        // Static fallback
        .fallback(get(static_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&config.bind_addr)
        .await
        .expect("failed to bind");
    tenet::tlog!("tenet-web listening on http://{}", config.bind_addr);

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
    let relay_url = state.relay_url.as_deref().unwrap_or("none");
    let relay_connected = state.relay_connected.load(Ordering::Relaxed);
    let peer_count = state.storage.list_peers().unwrap_or_default().len();
    let message_count = state
        .storage
        .list_messages(None, None, None, 1)
        .map(|m| if m.is_empty() { 0 } else { 1 })
        .unwrap_or(0);

    let body = serde_json::json!({
        "status": "ok",
        "peer_id": state.keypair.id,
        "relay": relay_url,
        "relay_connected": relay_connected,
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

/// Build the JSON representation of a message including its attachments,
/// reaction counts, and reply count.
fn message_to_json(m: &MessageRow, storage: &Storage) -> serde_json::Value {
    let attachments = storage
        .list_message_attachments(&m.message_id)
        .unwrap_or_default();
    let att_json: Vec<serde_json::Value> = attachments
        .iter()
        .map(|a| {
            serde_json::json!({
                "content_hash": a.content_hash,
                "filename": a.filename,
                "position": a.position,
            })
        })
        .collect();

    let (upvotes, downvotes) = storage.count_reactions(&m.message_id).unwrap_or((0, 0));
    let reply_count = storage.count_replies(&m.message_id).unwrap_or(0);

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
        "attachments": att_json,
        "reply_to": m.reply_to,
        "upvotes": upvotes,
        "downvotes": downvotes,
        "reply_count": reply_count,
    })
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
                .map(|m| message_to_json(m, &st.storage))
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
            let json = message_to_json(&m, &st.storage);
            (StatusCode::OK, axum::Json(json)).into_response()
        }
        Ok(None) => api_error(StatusCode::NOT_FOUND, "message not found"),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

// -- Send direct message --

/// Attachment reference included when sending a message.
#[derive(Deserialize)]
struct SendAttachmentRef {
    content_hash: String,
    filename: Option<String>,
}

#[derive(Deserialize)]
struct SendDirectRequest {
    recipient_id: String,
    body: String,
    #[serde(default)]
    attachments: Vec<SendAttachmentRef>,
}

async fn send_direct_handler(
    State(state): State<SharedState>,
    axum::Json(req): axum::Json<SendDirectRequest>,
) -> Response {
    if req.body.trim().is_empty() && req.attachments.is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "body or attachments required");
    }

    let now = now_secs();

    // Short lock: extract data needed for envelope construction
    let (keypair_id, signing_key, recipient_enc_key, relay_url) = {
        let st = state.lock().await;

        let peer = match st.storage.get_peer(&req.recipient_id) {
            Ok(Some(p)) => p,
            Ok(None) => return api_error(StatusCode::NOT_FOUND, "recipient peer not found"),
            Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        };

        let enc_key = match peer.encryption_public_key.as_deref() {
            Some(k) => k.to_string(),
            None => {
                return api_error(
                    StatusCode::BAD_REQUEST,
                    "recipient has no encryption key; cannot send encrypted message",
                )
            }
        };

        (
            st.keypair.id.clone(),
            st.keypair.signing_private_key_hex.clone(),
            enc_key,
            st.relay_url.clone(),
        )
    };
    // Lock released

    // Build encrypted envelope (CPU-only, no lock needed)
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
        keypair_id.clone(),
        req.recipient_id.clone(),
        None,
        None,
        now,
        DEFAULT_TTL_SECONDS,
        MessageKind::Direct,
        None,
        payload,
        &signing_key,
    ) {
        Ok(e) => e,
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("envelope: {e}")),
    };

    let msg_id = envelope.header.message_id.0.clone();

    // Post to relay (blocking I/O, no lock held)
    let relay_delivered = if let Some(ref relay_url) = relay_url {
        match post_to_relay(relay_url, &envelope) {
            Ok(()) => true,
            Err(e) => {
                tenet::tlog!("failed to post direct message to relay: {}", e);
                false
            }
        }
    } else {
        false
    };

    // Short lock: persist and broadcast
    {
        let st = state.lock().await;

        let _ = st.storage.insert_outbox(&tenet::storage::OutboxRow {
            message_id: msg_id.clone(),
            envelope: serde_json::to_string(&envelope).unwrap_or_default(),
            sent_at: now,
            delivered: false,
        });

        let _ = st.storage.insert_message(&MessageRow {
            message_id: msg_id.clone(),
            sender_id: keypair_id.clone(),
            recipient_id: req.recipient_id.clone(),
            message_kind: "direct".to_string(),
            group_id: None,
            body: Some(req.body.clone()),
            timestamp: now,
            received_at: now,
            ttl_seconds: DEFAULT_TTL_SECONDS,
            is_read: true,
            raw_envelope: None,
            reply_to: None,
        });

        link_attachments(&st.storage, &msg_id, &req.attachments);

        let _ = st.ws_tx.send(WsEvent::NewMessage {
            message_id: msg_id.clone(),
            sender_id: keypair_id.clone(),
            message_kind: "direct".to_string(),
            body: Some(req.body.clone()),
            timestamp: now,
        });
    }

    tenet::tlog!(
        "send: direct message to {} (id={}, relay={})",
        tenet::logging::peer_id(&req.recipient_id),
        tenet::logging::msg_id(&msg_id),
        relay_delivered
    );

    let json = serde_json::json!({
        "message_id": msg_id,
        "status": "sent",
        "relay_delivered": relay_delivered,
    });
    (StatusCode::CREATED, axum::Json(json)).into_response()
}

// -- Send public message --

#[derive(Deserialize)]
struct SendPublicRequest {
    body: String,
    #[serde(default)]
    attachments: Vec<SendAttachmentRef>,
}

async fn send_public_handler(
    State(state): State<SharedState>,
    axum::Json(req): axum::Json<SendPublicRequest>,
) -> Response {
    if req.body.trim().is_empty() && req.attachments.is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "body or attachments required");
    }

    let now = now_secs();

    // Short lock: extract identity data
    let (keypair_id, signing_key, relay_url) = {
        let st = state.lock().await;
        (
            st.keypair.id.clone(),
            st.keypair.signing_private_key_hex.clone(),
            st.relay_url.clone(),
        )
    };
    // Lock released

    // Build envelope (CPU-only, no lock needed)
    let mut salt = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut salt);

    let envelope = match build_plaintext_envelope(
        &keypair_id,
        "*",
        None,
        None,
        now,
        DEFAULT_TTL_SECONDS,
        MessageKind::Public,
        None,
        &req.body,
        salt,
        &signing_key,
    ) {
        Ok(e) => e,
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("envelope: {e}")),
    };

    let msg_id = envelope.header.message_id.0.clone();

    // Post to relay (blocking I/O, no lock held)
    let relay_delivered = if let Some(ref relay_url) = relay_url {
        match post_to_relay(relay_url, &envelope) {
            Ok(()) => true,
            Err(e) => {
                tenet::tlog!("failed to post public message to relay: {}", e);
                false
            }
        }
    } else {
        false
    };

    // Short lock: persist and broadcast
    {
        let st = state.lock().await;

        let _ = st.storage.insert_outbox(&tenet::storage::OutboxRow {
            message_id: msg_id.clone(),
            envelope: serde_json::to_string(&envelope).unwrap_or_default(),
            sent_at: now,
            delivered: false,
        });

        let _ = st.storage.insert_message(&MessageRow {
            message_id: msg_id.clone(),
            sender_id: keypair_id.clone(),
            recipient_id: "*".to_string(),
            message_kind: "public".to_string(),
            group_id: None,
            body: Some(req.body.clone()),
            timestamp: now,
            received_at: now,
            ttl_seconds: DEFAULT_TTL_SECONDS,
            is_read: true,
            raw_envelope: None,
            reply_to: None,
        });

        link_attachments(&st.storage, &msg_id, &req.attachments);

        let _ = st.ws_tx.send(WsEvent::NewMessage {
            message_id: msg_id.clone(),
            sender_id: keypair_id,
            message_kind: "public".to_string(),
            body: Some(req.body.clone()),
            timestamp: now,
        });
    }

    tenet::tlog!(
        "send: public message (id={}, relay={})",
        tenet::logging::msg_id(&msg_id),
        relay_delivered
    );

    let json = serde_json::json!({
        "message_id": msg_id,
        "status": "sent",
        "relay_delivered": relay_delivered,
    });
    (StatusCode::CREATED, axum::Json(json)).into_response()
}

// -- Send group message --

#[derive(Deserialize)]
struct SendGroupRequest {
    group_id: String,
    body: String,
    #[serde(default)]
    attachments: Vec<SendAttachmentRef>,
}

async fn send_group_handler(
    State(state): State<SharedState>,
    axum::Json(req): axum::Json<SendGroupRequest>,
) -> Response {
    if req.body.trim().is_empty() && req.attachments.is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "body cannot be empty");
    }

    let now = now_secs();

    // Short lock: extract group key and identity
    let (keypair_id, signing_key, group_key, relay_url) = {
        let st = state.lock().await;

        let group = match st.storage.get_group(&req.group_id) {
            Ok(Some(g)) => g,
            Ok(None) => return api_error(StatusCode::NOT_FOUND, "group not found"),
            Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        };

        let gk: [u8; 32] = match group.group_key.try_into() {
            Ok(k) => k,
            Err(_) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, "invalid group key"),
        };

        (
            st.keypair.id.clone(),
            st.keypair.signing_private_key_hex.clone(),
            gk,
            st.relay_url.clone(),
        )
    };
    // Lock released

    // Build envelope (CPU-only, no lock needed)
    let aad = req.group_id.as_bytes();
    let payload =
        match tenet::protocol::build_group_message_payload(req.body.as_bytes(), &group_key, aad) {
            Ok(p) => p,
            Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("crypto: {e}")),
        };

    let envelope = match build_envelope_from_payload(
        keypair_id.clone(),
        "*".to_string(),
        None,
        None,
        now,
        DEFAULT_TTL_SECONDS,
        MessageKind::FriendGroup,
        Some(req.group_id.clone()),
        payload,
        &signing_key,
    ) {
        Ok(e) => e,
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("envelope: {e}")),
    };

    let msg_id = envelope.header.message_id.0.clone();

    // Post to relay (blocking I/O, no lock held)
    let relay_delivered = if let Some(ref relay_url) = relay_url {
        match post_to_relay(relay_url, &envelope) {
            Ok(()) => true,
            Err(e) => {
                tenet::tlog!("failed to post group message to relay: {}", e);
                false
            }
        }
    } else {
        false
    };

    // Short lock: persist and broadcast
    {
        let st = state.lock().await;

        let _ = st.storage.insert_outbox(&tenet::storage::OutboxRow {
            message_id: msg_id.clone(),
            envelope: serde_json::to_string(&envelope).unwrap_or_default(),
            sent_at: now,
            delivered: false,
        });

        let _ = st.storage.insert_message(&MessageRow {
            message_id: msg_id.clone(),
            sender_id: keypair_id.clone(),
            recipient_id: "*".to_string(),
            message_kind: "friend_group".to_string(),
            group_id: Some(req.group_id.clone()),
            body: Some(req.body.clone()),
            timestamp: now,
            received_at: now,
            ttl_seconds: DEFAULT_TTL_SECONDS,
            is_read: true,
            raw_envelope: None,
            reply_to: None,
        });

        link_attachments(&st.storage, &msg_id, &req.attachments);

        let _ = st.ws_tx.send(WsEvent::NewMessage {
            message_id: msg_id.clone(),
            sender_id: keypair_id,
            message_kind: "friend_group".to_string(),
            body: Some(req.body.clone()),
            timestamp: now,
        });
    }

    tenet::tlog!(
        "send: group message to {} (id={}, relay={})",
        req.group_id,
        tenet::logging::msg_id(&msg_id),
        relay_delivered
    );

    let json = serde_json::json!({
        "message_id": msg_id,
        "status": "sent",
        "relay_delivered": relay_delivered,
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

/// Build the JSON representation of a peer.
fn peer_to_json(p: &PeerRow) -> serde_json::Value {
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
}

async fn list_peers_handler(State(state): State<SharedState>) -> Response {
    let st = state.lock().await;
    match st.storage.list_peers() {
        Ok(peers) => {
            let json: Vec<serde_json::Value> = peers.iter().map(peer_to_json).collect();
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
        Ok(Some(p)) => (StatusCode::OK, axum::Json(peer_to_json(&p))).into_response(),
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

    // Validate signing public key (Ed25519 keys are 32 bytes)
    if let Err(e) = validate_hex_key(&req.signing_public_key, 32, "signing_public_key") {
        return api_error(StatusCode::BAD_REQUEST, e);
    }

    // Validate encryption public key if provided (X25519 keys are 32 bytes)
    if let Some(ref enc_key) = req.encryption_public_key {
        if !enc_key.trim().is_empty() {
            if let Err(e) = validate_hex_key(enc_key, 32, "encryption_public_key") {
                return api_error(StatusCode::BAD_REQUEST, e);
            }
        }
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
        Ok(()) => (StatusCode::CREATED, axum::Json(peer_to_json(&peer_row))).into_response(),
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
// Friend Requests API
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct SendFriendRequestPayload {
    peer_id: String,
    message: Option<String>,
    #[serde(default)]
    force: bool,
}

#[derive(Deserialize)]
struct ListFriendRequestsQuery {
    status: Option<String>,
    direction: Option<String>,
}

async fn send_friend_request_handler(
    State(state): State<SharedState>,
    axum::Json(req): axum::Json<SendFriendRequestPayload>,
) -> Response {
    if req.peer_id.trim().is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "peer_id cannot be empty");
    }

    let (keypair, relay_url, existing) = {
        let st = state.lock().await;
        let peer_id = req.peer_id.trim().to_string();

        // Cannot send friend request to self
        if peer_id == st.keypair.id {
            return api_error(
                StatusCode::BAD_REQUEST,
                "cannot send friend request to yourself",
            );
        }

        // Check if already friends
        if let Ok(Some(peer)) = st.storage.get_peer(&peer_id) {
            if peer.is_friend {
                return api_error(StatusCode::CONFLICT, "already friends with this peer");
            }
        }

        // Check for existing outgoing request to this peer
        let existing = st
            .storage
            .find_request_between(&st.keypair.id, &peer_id)
            .unwrap_or(None);

        if let Some(ref ex) = existing {
            if ex.status == "pending" && !req.force {
                return api_error(StatusCode::CONFLICT, "friend request already pending");
            }
        }

        (st.keypair.clone(), st.relay_url.clone(), existing)
    };

    let peer_id = req.peer_id.trim().to_string();
    let now = now_secs();

    // Either refresh existing or create new outgoing friend request
    let request_id = {
        let st = state.lock().await;
        if let Some(ref ex) = existing {
            // Refresh the existing request (reset to pending, update message)
            if let Err(e) = st.storage.refresh_friend_request(
                ex.id,
                req.message.as_deref(),
                &keypair.signing_public_key_hex,
                &keypair.public_key_hex,
            ) {
                return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
            }
            tenet::tlog!(
                "friend-request: resending to {} (refreshed id={})",
                tenet::logging::peer_id(&peer_id),
                ex.id
            );
            ex.id
        } else {
            let fr_row = FriendRequestRow {
                id: 0,
                from_peer_id: keypair.id.clone(),
                to_peer_id: peer_id.clone(),
                status: "pending".to_string(),
                message: req.message.clone(),
                from_signing_key: keypair.signing_public_key_hex.clone(),
                from_encryption_key: keypair.public_key_hex.clone(),
                direction: "outgoing".to_string(),
                created_at: now,
                updated_at: now,
            };
            match st.storage.insert_friend_request(&fr_row) {
                Ok(id) => id,
                Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
            }
        }
    };

    // Send the friend request as a Meta message via relay
    if let Some(ref relay) = relay_url {
        let meta_msg = MetaMessage::FriendRequest {
            peer_id: keypair.id.clone(),
            signing_public_key: keypair.signing_public_key_hex.clone(),
            encryption_public_key: keypair.public_key_hex.clone(),
            message: req.message.clone(),
        };

        if let Ok(payload) = build_meta_payload(&meta_msg) {
            if let Ok(envelope) = build_envelope_from_payload(
                keypair.id.clone(),
                peer_id.clone(),
                None,
                None,
                now,
                DEFAULT_TTL_SECONDS,
                MessageKind::Meta,
                None,
                payload,
                &keypair.signing_private_key_hex,
            ) {
                if let Err(e) = post_to_relay(relay, &envelope) {
                    tenet::tlog!(
                        "friend-request: failed to send to relay for {}: {}",
                        tenet::logging::peer_id(&peer_id),
                        e
                    );
                } else {
                    tenet::tlog!(
                        "friend-request: sent to {} via relay",
                        tenet::logging::peer_id(&peer_id)
                    );
                }
            }
        }
    } else {
        tenet::tlog!("friend-request: no relay configured, request stored locally only");
    }

    let json = serde_json::json!({
        "id": request_id,
        "from_peer_id": keypair.id,
        "to_peer_id": peer_id,
        "status": "pending",
        "message": req.message,
        "direction": "outgoing",
        "created_at": now,
        "updated_at": now,
    });
    (StatusCode::CREATED, axum::Json(json)).into_response()
}

async fn list_friend_requests_handler(
    State(state): State<SharedState>,
    Query(query): Query<ListFriendRequestsQuery>,
) -> Response {
    let st = state.lock().await;
    match st
        .storage
        .list_friend_requests(query.status.as_deref(), query.direction.as_deref())
    {
        Ok(requests) => {
            let json: Vec<serde_json::Value> = requests
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "id": r.id,
                        "from_peer_id": r.from_peer_id,
                        "to_peer_id": r.to_peer_id,
                        "status": r.status,
                        "message": r.message,
                        "direction": r.direction,
                        "created_at": r.created_at,
                        "updated_at": r.updated_at,
                    })
                })
                .collect();
            (StatusCode::OK, axum::Json(serde_json::json!(json))).into_response()
        }
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn accept_friend_request_handler(
    State(state): State<SharedState>,
    Path(id): Path<i64>,
) -> Response {
    let (keypair, relay_url) = {
        let st = state.lock().await;
        (st.keypair.clone(), st.relay_url.clone())
    };

    let st = state.lock().await;

    // Get the friend request
    let fr = match st.storage.get_friend_request(id) {
        Ok(Some(fr)) => fr,
        Ok(None) => return api_error(StatusCode::NOT_FOUND, "friend request not found"),
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };

    if fr.status != "pending" {
        return api_error(
            StatusCode::BAD_REQUEST,
            format!("friend request is already {}", fr.status),
        );
    }

    if fr.direction != "incoming" {
        return api_error(
            StatusCode::BAD_REQUEST,
            "can only accept incoming friend requests",
        );
    }

    // Update request status
    if let Err(e) = st.storage.update_friend_request_status(id, "accepted") {
        return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }

    // Add the requester as a friend with their keys
    let now = now_secs();
    let peer_row = PeerRow {
        peer_id: fr.from_peer_id.clone(),
        display_name: None,
        signing_public_key: fr.from_signing_key.clone(),
        encryption_public_key: Some(fr.from_encryption_key.clone()),
        added_at: now,
        is_friend: true,
        last_seen_online: Some(now),
        online: false,
    };
    if let Err(e) = st.storage.insert_peer(&peer_row) {
        tenet::tlog!("failed to add peer from friend request: {}", e);
    }

    // Archive any outgoing request we sent to this peer (race condition)
    if let Ok(Some(outgoing)) = st
        .storage
        .find_request_between(&keypair.id, &fr.from_peer_id)
    {
        if outgoing.status == "pending" {
            tenet::tlog!(
                "friend-accept: archiving duplicate outgoing request to {} (id={})",
                tenet::logging::peer_id(&fr.from_peer_id),
                outgoing.id
            );
            let _ = st
                .storage
                .update_friend_request_status(outgoing.id, "accepted");
        }
    }

    // Send FriendAccept meta message via relay
    if let Some(ref relay) = relay_url {
        let meta_msg = MetaMessage::FriendAccept {
            peer_id: keypair.id.clone(),
            signing_public_key: keypair.signing_public_key_hex.clone(),
            encryption_public_key: keypair.public_key_hex.clone(),
        };

        if let Ok(payload) = build_meta_payload(&meta_msg) {
            if let Ok(envelope) = build_envelope_from_payload(
                keypair.id.clone(),
                fr.from_peer_id.clone(),
                None,
                None,
                now,
                DEFAULT_TTL_SECONDS,
                MessageKind::Meta,
                None,
                payload,
                &keypair.signing_private_key_hex,
            ) {
                if let Err(e) = post_to_relay(relay, &envelope) {
                    tenet::tlog!(
                        "friend-accept: failed to send to relay for {}: {}",
                        tenet::logging::peer_id(&fr.from_peer_id),
                        e
                    );
                } else {
                    tenet::tlog!(
                        "friend-accept: sent to {} via relay",
                        tenet::logging::peer_id(&fr.from_peer_id)
                    );
                }
            }
        }
    }

    (
        StatusCode::OK,
        axum::Json(serde_json::json!({"status": "accepted", "id": id})),
    )
        .into_response()
}

async fn ignore_friend_request_handler(
    State(state): State<SharedState>,
    Path(id): Path<i64>,
) -> Response {
    let st = state.lock().await;

    let fr = match st.storage.get_friend_request(id) {
        Ok(Some(fr)) => fr,
        Ok(None) => return api_error(StatusCode::NOT_FOUND, "friend request not found"),
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };

    if fr.status != "pending" {
        return api_error(
            StatusCode::BAD_REQUEST,
            format!("friend request is already {}", fr.status),
        );
    }

    if let Err(e) = st.storage.update_friend_request_status(id, "ignored") {
        return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }

    (
        StatusCode::OK,
        axum::Json(serde_json::json!({"status": "ignored", "id": id})),
    )
        .into_response()
}

async fn block_friend_request_handler(
    State(state): State<SharedState>,
    Path(id): Path<i64>,
) -> Response {
    let st = state.lock().await;

    let fr = match st.storage.get_friend_request(id) {
        Ok(Some(fr)) => fr,
        Ok(None) => return api_error(StatusCode::NOT_FOUND, "friend request not found"),
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };

    if fr.direction != "incoming" {
        return api_error(
            StatusCode::BAD_REQUEST,
            "can only block incoming friend requests",
        );
    }

    if let Err(e) = st.storage.update_friend_request_status(id, "blocked") {
        return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }

    (
        StatusCode::OK,
        axum::Json(serde_json::json!({"status": "blocked", "id": id})),
    )
        .into_response()
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

    let now = now_secs();

    // Generate a new symmetric key for the group
    let mut group_key = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut group_key);

    // Short lock: insert group + members atomically, extract peer data for key distribution
    let (keypair_id, signing_key, relay_url, all_members, member_enc_keys) = {
        let st = state.lock().await;

        let group_row = tenet::storage::GroupRow {
            group_id: req.group_id.clone(),
            group_key: group_key.to_vec(),
            creator_id: st.keypair.id.clone(),
            created_at: now,
            key_version: 1,
        };

        let mut all_members = req.member_ids.clone();
        if !all_members.contains(&st.keypair.id) {
            all_members.push(st.keypair.id.clone());
        }

        let member_rows: Vec<tenet::storage::GroupMemberRow> = all_members
            .iter()
            .map(|member_id| tenet::storage::GroupMemberRow {
                group_id: req.group_id.clone(),
                peer_id: member_id.clone(),
                joined_at: now,
            })
            .collect();

        if let Err(e) = st
            .storage
            .insert_group_with_members(&group_row, &member_rows)
        {
            return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
        }

        // Collect encryption keys for members we need to distribute to
        let mut member_enc_keys: Vec<(String, Option<String>)> = Vec::new();
        for member_id in &req.member_ids {
            if member_id == &st.keypair.id {
                continue;
            }
            let enc_key = st
                .storage
                .get_peer(member_id)
                .ok()
                .flatten()
                .and_then(|p| p.encryption_public_key);
            member_enc_keys.push((member_id.clone(), enc_key));
        }

        (
            st.keypair.id.clone(),
            st.keypair.signing_private_key_hex.clone(),
            st.relay_url.clone(),
            all_members,
            member_enc_keys,
        )
    };
    // Lock released

    // Distribute group key to each member (crypto + I/O, no lock held)
    let mut key_distribution_failed: Vec<String> = Vec::new();

    for (member_id, enc_key) in &member_enc_keys {
        let recipient_enc_key = match enc_key {
            Some(k) => k.clone(),
            None => {
                tenet::tlog!(
                    "peer {} not found or has no encryption key; skipping",
                    tenet::logging::peer_id(member_id)
                );
                key_distribution_failed.push(member_id.clone());
                continue;
            }
        };

        let key_distribution = serde_json::json!({
            "type": "group_key_distribution",
            "group_id": req.group_id,
            "group_key": hex::encode(group_key),
            "key_version": 1,
            "creator_id": keypair_id,
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
            Err(e) => {
                tenet::tlog!(
                    "failed to encrypt key distribution for {}: {}",
                    tenet::logging::peer_id(member_id),
                    e
                );
                key_distribution_failed.push(member_id.clone());
                continue;
            }
        };

        let envelope = match build_envelope_from_payload(
            keypair_id.clone(),
            member_id.clone(),
            None,
            None,
            now,
            DEFAULT_TTL_SECONDS,
            MessageKind::Direct,
            None,
            payload,
            &signing_key,
        ) {
            Ok(e) => e,
            Err(e) => {
                tenet::tlog!(
                    "failed to build envelope for {}: {}",
                    tenet::logging::peer_id(member_id),
                    e
                );
                key_distribution_failed.push(member_id.clone());
                continue;
            }
        };

        if let Some(ref relay_url) = relay_url {
            if let Err(e) = post_to_relay(relay_url, &envelope) {
                tenet::tlog!(
                    "failed to distribute group key to {}: {}",
                    tenet::logging::peer_id(member_id),
                    e
                );
                key_distribution_failed.push(member_id.clone());
            }
        } else {
            key_distribution_failed.push(member_id.clone());
        }
    }

    let mut json = serde_json::json!({
        "group_id": req.group_id,
        "creator_id": keypair_id,
        "created_at": now,
        "key_version": 1,
        "members": all_members,
    });

    if !key_distribution_failed.is_empty() {
        json.as_object_mut().unwrap().insert(
            "key_distribution_failed".to_string(),
            serde_json::json!(key_distribution_failed),
        );
    }

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

    let now = now_secs();

    // Short lock: validate, insert member, extract data for key distribution
    let (keypair_id, signing_key, relay_url, recipient_enc_key, group_key, key_version, creator_id) = {
        let st = state.lock().await;

        let group = match st.storage.get_group(&group_id) {
            Ok(Some(g)) => g,
            Ok(None) => return api_error(StatusCode::NOT_FOUND, "group not found"),
            Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        };

        let member_row = tenet::storage::GroupMemberRow {
            group_id: group_id.clone(),
            peer_id: req.peer_id.clone(),
            joined_at: now,
        };

        if let Err(e) = st.storage.insert_group_member(&member_row) {
            return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
        }

        let peer = match st.storage.get_peer(&req.peer_id) {
            Ok(Some(p)) => p,
            Ok(None) => return api_error(StatusCode::NOT_FOUND, "peer not found"),
            Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        };

        let enc_key = match peer.encryption_public_key.as_deref() {
            Some(k) => k.to_string(),
            None => {
                return api_error(
                    StatusCode::BAD_REQUEST,
                    "peer has no encryption key; cannot distribute group key",
                )
            }
        };

        let gk: [u8; 32] = match group.group_key.try_into() {
            Ok(k) => k,
            Err(_) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, "invalid group key"),
        };

        (
            st.keypair.id.clone(),
            st.keypair.signing_private_key_hex.clone(),
            st.relay_url.clone(),
            enc_key,
            gk,
            group.key_version,
            group.creator_id.clone(),
        )
    };
    // Lock released

    // Build key distribution envelope (crypto, no lock needed)
    let key_distribution = serde_json::json!({
        "type": "group_key_distribution",
        "group_id": group_id,
        "group_key": hex::encode(group_key),
        "key_version": key_version,
        "creator_id": creator_id,
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
        keypair_id,
        req.peer_id.clone(),
        None,
        None,
        now,
        DEFAULT_TTL_SECONDS,
        MessageKind::Direct,
        None,
        payload,
        &signing_key,
    ) {
        Ok(e) => e,
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("envelope: {e}")),
    };

    // Post to relay (blocking I/O, no lock held)
    let relay_delivered = if let Some(ref relay_url) = relay_url {
        match post_to_relay(relay_url, &envelope) {
            Ok(()) => true,
            Err(e) => {
                tenet::tlog!(
                    "failed to distribute group key to {}: {}",
                    tenet::logging::peer_id(&req.peer_id),
                    e
                );
                false
            }
        }
    } else {
        false
    };

    let json = serde_json::json!({
        "status": "added",
        "group_id": group_id,
        "peer_id": req.peer_id,
        "key_delivered": relay_delivered,
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
// Attachments API (Phase 7)
// ---------------------------------------------------------------------------

async fn upload_attachment_handler(
    State(state): State<SharedState>,
    mut multipart: Multipart,
) -> Response {
    let mut file_data: Option<Vec<u8>> = None;
    let mut content_type: Option<String> = None;
    let mut filename: Option<String> = None;

    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        if name == "file" {
            content_type = field
                .content_type()
                .map(|ct| ct.to_string())
                .or_else(|| Some("application/octet-stream".to_string()));
            filename = field.file_name().map(|f| f.to_string());
            match field.bytes().await {
                Ok(bytes) => {
                    if bytes.len() as u64 > MAX_ATTACHMENT_SIZE {
                        return api_error(
                            StatusCode::PAYLOAD_TOO_LARGE,
                            format!(
                                "attachment exceeds maximum size of {} bytes",
                                MAX_ATTACHMENT_SIZE
                            ),
                        );
                    }
                    file_data = Some(bytes.to_vec());
                }
                Err(e) => {
                    return api_error(StatusCode::BAD_REQUEST, format!("failed to read file: {e}"))
                }
            }
        }
    }

    let data = match file_data {
        Some(d) if !d.is_empty() => d,
        _ => return api_error(StatusCode::BAD_REQUEST, "no file provided"),
    };

    let mime = content_type.unwrap_or_else(|| "application/octet-stream".to_string());

    // Compute content hash (SHA256, base64 URL-safe)
    let digest = Sha256::digest(&data);
    let content_hash = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);

    let now = now_secs();
    let size = data.len() as u64;

    let st = state.lock().await;
    let row = AttachmentRow {
        content_hash: content_hash.clone(),
        content_type: mime.clone(),
        size_bytes: size,
        data,
        created_at: now,
    };

    if let Err(e) = st.storage.insert_attachment(&row) {
        return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }

    let json = serde_json::json!({
        "content_hash": content_hash,
        "content_type": mime,
        "size_bytes": size,
        "filename": filename,
    });
    (StatusCode::CREATED, axum::Json(json)).into_response()
}

async fn download_attachment_handler(
    State(state): State<SharedState>,
    Path(content_hash): Path<String>,
) -> Response {
    let st = state.lock().await;
    match st.storage.get_attachment(&content_hash) {
        Ok(Some(att)) => {
            let headers = [
                (header::CONTENT_TYPE, att.content_type.as_str().to_string()),
                (
                    header::CACHE_CONTROL,
                    "public, max-age=31536000, immutable".to_string(),
                ),
            ];
            (StatusCode::OK, headers, att.data).into_response()
        }
        Ok(None) => api_error(StatusCode::NOT_FOUND, "attachment not found"),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Reactions API (Phase 8)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ReactRequest {
    reaction: String,
}

async fn react_handler(
    State(state): State<SharedState>,
    Path(message_id): Path<String>,
    axum::Json(req): axum::Json<ReactRequest>,
) -> Response {
    if req.reaction != "upvote" && req.reaction != "downvote" {
        return api_error(
            StatusCode::BAD_REQUEST,
            "reaction must be 'upvote' or 'downvote'",
        );
    }

    let now = now_secs();

    // Short lock: verify message exists, extract identity data
    let (keypair_id, signing_key, relay_url) = {
        let st = state.lock().await;

        if !st.storage.has_message(&message_id).unwrap_or(false) {
            return api_error(StatusCode::NOT_FOUND, "message not found");
        }

        (
            st.keypair.id.clone(),
            st.keypair.signing_private_key_hex.clone(),
            st.relay_url.clone(),
        )
    };
    // Lock released

    // Build envelope (CPU-only, no lock needed)
    let reaction_payload = serde_json::json!({
        "target_message_id": message_id,
        "reaction": req.reaction,
        "timestamp": now,
    });

    let body_str = serde_json::to_string(&reaction_payload).unwrap_or_default();

    let mut salt = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut salt);

    let envelope = match build_plaintext_envelope(
        &keypair_id,
        "*",
        None,
        None,
        now,
        DEFAULT_TTL_SECONDS,
        MessageKind::Public,
        None,
        &body_str,
        salt,
        &signing_key,
    ) {
        Ok(e) => e,
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("envelope: {e}")),
    };

    // Post to relay (blocking I/O, no lock held)
    if let Some(ref relay_url) = relay_url {
        if let Err(e) = post_to_relay(relay_url, &envelope) {
            tenet::tlog!("failed to post reaction to relay: {}", e);
        }
    }

    let reaction_msg_id = envelope.header.message_id.0.clone();

    // Short lock: persist reaction and count
    let st = state.lock().await;

    let reaction_row = ReactionRow {
        message_id: reaction_msg_id.clone(),
        target_id: message_id.clone(),
        sender_id: keypair_id,
        reaction: req.reaction.clone(),
        timestamp: now,
    };
    if let Err(e) = st.storage.upsert_reaction(&reaction_row) {
        return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }

    let (upvotes, downvotes) = st.storage.count_reactions(&message_id).unwrap_or((0, 0));

    let json = serde_json::json!({
        "status": "ok",
        "target_id": message_id,
        "reaction": req.reaction,
        "upvotes": upvotes,
        "downvotes": downvotes,
    });
    (StatusCode::OK, axum::Json(json)).into_response()
}

async fn unreact_handler(
    State(state): State<SharedState>,
    Path(message_id): Path<String>,
) -> Response {
    let st = state.lock().await;

    match st.storage.delete_reaction(&message_id, &st.keypair.id) {
        Ok(true) => {
            let (upvotes, downvotes) = st.storage.count_reactions(&message_id).unwrap_or((0, 0));
            let json = serde_json::json!({
                "status": "removed",
                "target_id": message_id,
                "upvotes": upvotes,
                "downvotes": downvotes,
            });
            (StatusCode::OK, axum::Json(json)).into_response()
        }
        Ok(false) => api_error(StatusCode::NOT_FOUND, "no reaction to remove"),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn list_reactions_handler(
    State(state): State<SharedState>,
    Path(message_id): Path<String>,
) -> Response {
    let st = state.lock().await;
    let (upvotes, downvotes) = st.storage.count_reactions(&message_id).unwrap_or((0, 0));
    let reactions = st.storage.list_reactions(&message_id).unwrap_or_default();

    let reaction_json: Vec<serde_json::Value> = reactions
        .iter()
        .map(|r| {
            serde_json::json!({
                "sender_id": r.sender_id,
                "reaction": r.reaction,
                "timestamp": r.timestamp,
            })
        })
        .collect();

    // Determine current user's reaction
    let my_reaction = st
        .storage
        .get_reaction(&message_id, &st.keypair.id)
        .ok()
        .flatten()
        .map(|r| r.reaction);

    let json = serde_json::json!({
        "target_id": message_id,
        "upvotes": upvotes,
        "downvotes": downvotes,
        "my_reaction": my_reaction,
        "reactions": reaction_json,
    });
    (StatusCode::OK, axum::Json(json)).into_response()
}

// ---------------------------------------------------------------------------
// Replies API (Phase 9)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ReplyRequest {
    body: String,
    #[serde(default)]
    attachments: Vec<SendAttachmentRef>,
}

#[derive(Deserialize)]
struct ListRepliesQuery {
    before: Option<u64>,
    limit: Option<u32>,
}

async fn list_replies_handler(
    State(state): State<SharedState>,
    Path(message_id): Path<String>,
    Query(params): Query<ListRepliesQuery>,
) -> Response {
    let st = state.lock().await;
    let limit = params.limit.unwrap_or(50).min(200);

    match st.storage.list_replies(&message_id, params.before, limit) {
        Ok(replies) => {
            let json: Vec<serde_json::Value> = replies
                .iter()
                .map(|m| message_to_json(m, &st.storage))
                .collect();
            (StatusCode::OK, axum::Json(serde_json::json!(json))).into_response()
        }
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn reply_handler(
    State(state): State<SharedState>,
    Path(parent_message_id): Path<String>,
    axum::Json(req): axum::Json<ReplyRequest>,
) -> Response {
    if req.body.trim().is_empty() && req.attachments.is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "body or attachments required");
    }

    let now = now_secs();

    // Short lock: extract parent info, identity data, and optional group key
    #[allow(clippy::type_complexity)]
    let (keypair_id, signing_key, relay_url, message_kind, parent_group_id, group_key_opt): (
        String,
        String,
        Option<String>,
        String,
        Option<String>,
        Option<[u8; 32]>,
    ) = {
        let st = state.lock().await;

        let parent = match st.storage.get_message(&parent_message_id) {
            Ok(Some(m)) => m,
            Ok(None) => return api_error(StatusCode::NOT_FOUND, "parent message not found"),
            Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        };

        let mk = parent.message_kind.clone();
        let pgid = parent.group_id.clone();

        let gk = if mk == "friend_group" {
            let group_id = match pgid.as_deref() {
                Some(gid) => gid,
                None => {
                    return api_error(
                        StatusCode::BAD_REQUEST,
                        "parent group message has no group_id",
                    )
                }
            };
            let group = match st.storage.get_group(group_id) {
                Ok(Some(g)) => g,
                Ok(None) => return api_error(StatusCode::NOT_FOUND, "group not found"),
                Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
            };
            let key: [u8; 32] = match group.group_key.try_into() {
                Ok(k) => k,
                Err(_) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, "invalid group key"),
            };
            Some(key)
        } else {
            None
        };

        (
            st.keypair.id.clone(),
            st.keypair.signing_private_key_hex.clone(),
            st.relay_url.clone(),
            mk,
            pgid,
            gk,
        )
    };
    // Lock released

    // Build envelope (CPU-only, no lock needed)
    let envelope = if message_kind == "public" {
        let mut salt = [0u8; 16];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut salt);

        match build_plaintext_envelope(
            &keypair_id,
            "*",
            None,
            None,
            now,
            DEFAULT_TTL_SECONDS,
            MessageKind::Public,
            None,
            &req.body,
            salt,
            &signing_key,
        ) {
            Ok(e) => e,
            Err(e) => {
                return api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("envelope: {e}"))
            }
        }
    } else if message_kind == "friend_group" {
        let group_id = parent_group_id.as_deref().unwrap();
        let group_key = group_key_opt.unwrap();
        let aad = group_id.as_bytes();
        let payload = match tenet::protocol::build_group_message_payload(
            req.body.as_bytes(),
            &group_key,
            aad,
        ) {
            Ok(p) => p,
            Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("crypto: {e}")),
        };
        match build_envelope_from_payload(
            keypair_id.clone(),
            "*".to_string(),
            None,
            None,
            now,
            DEFAULT_TTL_SECONDS,
            MessageKind::FriendGroup,
            Some(group_id.to_string()),
            payload,
            &signing_key,
        ) {
            Ok(e) => e,
            Err(e) => {
                return api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("envelope: {e}"))
            }
        }
    } else {
        return api_error(
            StatusCode::BAD_REQUEST,
            "replies only supported for public and group messages",
        );
    };

    // Post to relay (blocking I/O, no lock held)
    let relay_delivered = if let Some(ref relay_url) = relay_url {
        match post_to_relay(relay_url, &envelope) {
            Ok(()) => true,
            Err(e) => {
                tenet::tlog!("failed to post reply to relay: {}", e);
                false
            }
        }
    } else {
        false
    };

    let msg_id = envelope.header.message_id.0.clone();

    // Short lock: persist and broadcast
    {
        let st = state.lock().await;

        let _ = st.storage.insert_outbox(&tenet::storage::OutboxRow {
            message_id: msg_id.clone(),
            envelope: serde_json::to_string(&envelope).unwrap_or_default(),
            sent_at: now,
            delivered: false,
        });

        let _ = st.storage.insert_message(&MessageRow {
            message_id: msg_id.clone(),
            sender_id: keypair_id.clone(),
            recipient_id: "*".to_string(),
            message_kind: message_kind.clone(),
            group_id: parent_group_id,
            body: Some(req.body.clone()),
            timestamp: now,
            received_at: now,
            ttl_seconds: DEFAULT_TTL_SECONDS,
            is_read: true,
            raw_envelope: None,
            reply_to: Some(parent_message_id.clone()),
        });

        link_attachments(&st.storage, &msg_id, &req.attachments);

        let _ = st.ws_tx.send(WsEvent::NewMessage {
            message_id: msg_id.clone(),
            sender_id: keypair_id,
            message_kind,
            body: Some(req.body),
            timestamp: now,
        });
    }

    let json = serde_json::json!({
        "message_id": msg_id,
        "status": "sent",
        "reply_to": parent_message_id,
        "relay_delivered": relay_delivered,
    });
    (StatusCode::CREATED, axum::Json(json)).into_response()
}

// ---------------------------------------------------------------------------
// Profiles API (Phase 10)
// ---------------------------------------------------------------------------

async fn get_own_profile_handler(State(state): State<SharedState>) -> Response {
    let st = state.lock().await;
    match st.storage.get_profile(&st.keypair.id) {
        Ok(Some(profile)) => {
            let public_fields: serde_json::Value =
                serde_json::from_str(&profile.public_fields).unwrap_or(serde_json::json!({}));
            let friends_fields: serde_json::Value =
                serde_json::from_str(&profile.friends_fields).unwrap_or(serde_json::json!({}));

            let json = serde_json::json!({
                "user_id": profile.user_id,
                "display_name": profile.display_name,
                "bio": profile.bio,
                "avatar_hash": profile.avatar_hash,
                "public_fields": public_fields,
                "friends_fields": friends_fields,
                "updated_at": profile.updated_at,
            });
            (StatusCode::OK, axum::Json(json)).into_response()
        }
        Ok(None) => {
            // Return empty profile
            let json = serde_json::json!({
                "user_id": st.keypair.id,
                "display_name": null,
                "bio": null,
                "avatar_hash": null,
                "public_fields": {},
                "friends_fields": {},
                "updated_at": 0,
            });
            (StatusCode::OK, axum::Json(json)).into_response()
        }
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize)]
struct UpdateProfileRequest {
    display_name: Option<String>,
    bio: Option<String>,
    avatar_hash: Option<String>,
    #[serde(default = "default_empty_json")]
    public_fields: serde_json::Value,
    #[serde(default = "default_empty_json")]
    friends_fields: serde_json::Value,
}

fn default_empty_json() -> serde_json::Value {
    serde_json::json!({})
}

async fn update_own_profile_handler(
    State(state): State<SharedState>,
    axum::Json(req): axum::Json<UpdateProfileRequest>,
) -> Response {
    let now = now_secs();

    // Short lock: persist profile and extract data for broadcasting
    let (keypair_id, signing_key, relay_url, friend_enc_keys) = {
        let st = state.lock().await;

        let profile = ProfileRow {
            user_id: st.keypair.id.clone(),
            display_name: req.display_name.clone(),
            bio: req.bio.clone(),
            avatar_hash: req.avatar_hash.clone(),
            public_fields: serde_json::to_string(&req.public_fields)
                .unwrap_or_else(|_| "{}".to_string()),
            friends_fields: serde_json::to_string(&req.friends_fields)
                .unwrap_or_else(|_| "{}".to_string()),
            updated_at: now,
        };

        if let Err(e) = st.storage.upsert_profile(&profile) {
            return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
        }

        // Collect friend encryption keys for profile distribution
        let friends: Vec<(String, String)> = st
            .storage
            .list_peers()
            .unwrap_or_default()
            .iter()
            .filter(|p| p.is_friend)
            .filter_map(|p| {
                p.encryption_public_key
                    .as_ref()
                    .map(|k| (p.peer_id.clone(), k.clone()))
            })
            .collect();

        (
            st.keypair.id.clone(),
            st.keypair.signing_private_key_hex.clone(),
            st.relay_url.clone(),
            friends,
        )
    };
    // Lock released

    // Build and send public profile (crypto + I/O, no lock held)
    let public_profile = serde_json::json!({
        "type": "tenet.profile",
        "user_id": keypair_id,
        "display_name": req.display_name,
        "bio": req.bio,
        "avatar_hash": req.avatar_hash,
        "public_fields": req.public_fields,
        "updated_at": now,
    });

    let body_str = serde_json::to_string(&public_profile).unwrap_or_default();
    let mut salt = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut salt);

    if let Ok(envelope) = build_plaintext_envelope(
        &keypair_id,
        "*",
        None,
        None,
        now,
        DEFAULT_TTL_SECONDS,
        MessageKind::Public,
        None,
        &body_str,
        salt,
        &signing_key,
    ) {
        if let Some(ref relay_url) = relay_url {
            if let Err(e) = post_to_relay(relay_url, &envelope) {
                tenet::tlog!("failed to post public profile to relay: {}", e);
            }
        }
    }

    // Send friends-only profile to each friend (crypto + I/O, no lock held)
    let friends_profile = serde_json::json!({
        "type": "tenet.profile",
        "user_id": keypair_id,
        "display_name": req.display_name,
        "bio": req.bio,
        "avatar_hash": req.avatar_hash,
        "public_fields": req.public_fields,
        "friends_fields": req.friends_fields,
        "updated_at": now,
    });
    let friends_body = serde_json::to_string(&friends_profile).unwrap_or_default();

    for (peer_id, enc_key) in &friend_enc_keys {
        let content_key = generate_content_key();
        let mut nonce = [0u8; NONCE_SIZE];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut nonce);

        if let Ok(payload) = build_encrypted_payload(
            friends_body.as_bytes(),
            enc_key,
            WEB_PAYLOAD_AAD,
            WEB_HPKE_INFO,
            &content_key,
            &nonce,
            None,
        ) {
            if let Ok(envelope) = build_envelope_from_payload(
                keypair_id.clone(),
                peer_id.clone(),
                None,
                None,
                now,
                DEFAULT_TTL_SECONDS,
                MessageKind::Direct,
                None,
                payload,
                &signing_key,
            ) {
                if let Some(ref relay_url) = relay_url {
                    if let Err(e) = post_to_relay(relay_url, &envelope) {
                        tenet::tlog!(
                            "failed to send friends profile to {}: {}",
                            tenet::logging::peer_id(peer_id),
                            e
                        );
                    }
                }
            }
        }
    }

    let json = serde_json::json!({
        "user_id": keypair_id,
        "display_name": req.display_name,
        "bio": req.bio,
        "avatar_hash": req.avatar_hash,
        "public_fields": req.public_fields,
        "friends_fields": req.friends_fields,
        "updated_at": now,
    });
    (StatusCode::OK, axum::Json(json)).into_response()
}

async fn get_peer_profile_handler(
    State(state): State<SharedState>,
    Path(peer_id): Path<String>,
) -> Response {
    let st = state.lock().await;

    // Check if peer is a friend
    let is_friend = st
        .storage
        .get_peer(&peer_id)
        .ok()
        .flatten()
        .map(|p| p.is_friend)
        .unwrap_or(false);

    match st.storage.get_profile(&peer_id) {
        Ok(Some(profile)) => {
            let public_fields: serde_json::Value =
                serde_json::from_str(&profile.public_fields).unwrap_or(serde_json::json!({}));

            let mut json = serde_json::json!({
                "user_id": profile.user_id,
                "display_name": profile.display_name,
                "bio": profile.bio,
                "avatar_hash": profile.avatar_hash,
                "public_fields": public_fields,
                "updated_at": profile.updated_at,
            });

            // If the peer is a friend, include friends-only fields
            if is_friend {
                let friends_fields: serde_json::Value =
                    serde_json::from_str(&profile.friends_fields).unwrap_or(serde_json::json!({}));
                json["friends_fields"] = friends_fields;
            }

            (StatusCode::OK, axum::Json(json)).into_response()
        }
        Ok(None) => {
            let json = serde_json::json!({
                "user_id": peer_id,
                "display_name": null,
                "bio": null,
                "avatar_hash": null,
                "public_fields": {},
                "updated_at": 0,
            });
            (StatusCode::OK, axum::Json(json)).into_response()
        }
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Conversations API (Phase 5)
// ---------------------------------------------------------------------------

async fn list_conversations_handler(State(state): State<SharedState>) -> Response {
    let st = state.lock().await;
    match st.storage.list_conversations(&st.keypair.id) {
        Ok(conversations) => {
            // Enrich with peer display names
            let json: Vec<serde_json::Value> = conversations
                .iter()
                .map(|c| {
                    let peer = st.storage.get_peer(&c.peer_id).ok().flatten();
                    serde_json::json!({
                        "peer_id": c.peer_id,
                        "display_name": peer.as_ref().and_then(|p| p.display_name.clone()),
                        "last_timestamp": c.last_timestamp,
                        "last_message": c.last_message,
                        "unread_count": c.unread_count,
                    })
                })
                .collect();
            (StatusCode::OK, axum::Json(serde_json::json!(json))).into_response()
        }
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize)]
struct ConversationQuery {
    before: Option<u64>,
    limit: Option<u32>,
}

async fn get_conversation_handler(
    State(state): State<SharedState>,
    Path(peer_id): Path<String>,
    Query(params): Query<ConversationQuery>,
) -> Response {
    let st = state.lock().await;
    let limit = params.limit.unwrap_or(50).min(200);

    match st
        .storage
        .list_conversation_messages(&st.keypair.id, &peer_id, params.before, limit)
    {
        Ok(messages) => {
            let json: Vec<serde_json::Value> = messages
                .iter()
                .map(|m| message_to_json(m, &st.storage))
                .collect();
            (StatusCode::OK, axum::Json(serde_json::json!(json))).into_response()
        }
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

// ---------------------------------------------------------------------------
// WebSocket handler
// ---------------------------------------------------------------------------

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<SharedState>) -> Response {
    // Check connection limit before upgrading
    let ws_count = {
        let st = state.lock().await;
        st.ws_connection_count.clone()
    };

    let current = ws_count.load(Ordering::Relaxed);
    if current >= MAX_WS_CONNECTIONS {
        return api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            format!(
                "too many WebSocket connections (max {})",
                MAX_WS_CONNECTIONS
            ),
        );
    }

    ws.on_upgrade(|socket| ws_connection(socket, state))
        .into_response()
}

async fn ws_connection(mut socket: WebSocket, state: SharedState) {
    // Subscribe to the broadcast channel and increment connection count
    let (mut rx, ws_count) = {
        let st = state.lock().await;
        let count = st.ws_connection_count.clone();
        count.fetch_add(1, Ordering::Relaxed);
        (st.ws_tx.subscribe(), count)
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
                        tenet::tlog!("ws client lagged, skipped {n} events");
                        // Notify client so it can refresh
                        let lag_msg = serde_json::json!({
                            "type": "events_missed",
                            "count": n,
                        });
                        if let Ok(json) = serde_json::to_string(&lag_msg) {
                            if socket.send(WsMessage::Text(json)).await.is_err() {
                                break;
                            }
                        }
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

    // Decrement connection count on disconnect
    ws_count.fetch_sub(1, Ordering::Relaxed);
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
    let mut consecutive_failures = 0u32;
    const MAX_BACKOFF_SECS: u64 = 300; // 5 minutes

    loop {
        // Calculate interval with exponential backoff on failure
        let interval_secs = if consecutive_failures == 0 {
            SYNC_INTERVAL_SECS
        } else {
            // Exponential backoff: 30s * 2^failures, capped at 5 minutes
            let backoff = SYNC_INTERVAL_SECS * 2u64.pow(consecutive_failures);
            backoff.min(MAX_BACKOFF_SECS)
        };

        tokio::time::sleep(Duration::from_secs(interval_secs)).await;

        match sync_once(&state).await {
            Ok(()) => {
                let was_disconnected = consecutive_failures > 0;
                consecutive_failures = 0;

                let st = state.lock().await;
                let was_connected = st.relay_connected.swap(true, Ordering::Relaxed);
                if !was_connected || was_disconnected {
                    let relay_url = st.relay_url.clone();
                    tenet::tlog!(
                        "relay connected: {}",
                        relay_url.as_deref().unwrap_or("unknown")
                    );
                    let _ = st.ws_tx.send(WsEvent::RelayStatus {
                        connected: true,
                        relay_url,
                    });
                }
            }
            Err(e) => {
                consecutive_failures += 1;
                let next_retry_secs =
                    (SYNC_INTERVAL_SECS * 2u64.pow(consecutive_failures)).min(MAX_BACKOFF_SECS);

                let st = state.lock().await;
                let was_connected = st.relay_connected.swap(false, Ordering::Relaxed);
                if was_connected || consecutive_failures == 1 {
                    let relay_url = st.relay_url.clone();
                    tenet::tlog!(
                        "relay disconnected: {} (attempt {}, next retry in {}s): {}",
                        relay_url.as_deref().unwrap_or("unknown"),
                        consecutive_failures,
                        next_retry_secs,
                        e
                    );
                    let _ = st.ws_tx.send(WsEvent::RelayStatus {
                        connected: false,
                        relay_url,
                    });
                } else {
                    tenet::tlog!(
                        "relay sync error (attempt {}, next retry in {}s): {}",
                        consecutive_failures,
                        next_retry_secs,
                        e
                    );
                }
            }
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

    // Build a peer lookup map for signature verification and decryption
    let peer_map: std::collections::HashMap<String, &PeerRow> =
        peers.iter().map(|p| (p.peer_id.clone(), p)).collect();

    // Fetch raw envelopes directly from the relay inbox (single fetch).
    // The relay drains messages on fetch, so we must process everything here
    // including meta messages from unknown senders (friend requests).
    let base = relay_url.trim_end_matches('/');
    let inbox_url = format!("{}/inbox/{}", base, keypair.id);
    let envelopes: Vec<tenet::protocol::Envelope> = ureq::get(&inbox_url)
        .call()
        .map_err(|e| format!("relay fetch failed: {e}"))?
        .into_json()
        .map_err(|e| format!("deserialize inbox: {e}"))?;

    if envelopes.is_empty() {
        return Ok(());
    }

    tenet::tlog!("sync: fetched {} envelope(s) from relay", envelopes.len());

    let now = now_secs();
    let mut stored_count = 0u32;

    for envelope in &envelopes {
        // --- Meta messages: process from ALL senders (including unknown) ---
        if envelope.header.message_kind == MessageKind::Meta {
            if let Ok(meta_msg) = decode_meta_payload(&envelope.payload) {
                match meta_msg {
                    MetaMessage::Online { peer_id, timestamp } => {
                        let st = state.lock().await;
                        if let Ok(true) = st.storage.update_peer_online(&peer_id, true, timestamp) {
                            let _ = st.ws_tx.send(WsEvent::PeerOnline {
                                peer_id: peer_id.clone(),
                            });
                            tenet::tlog!(
                                "sync: peer {} is now online",
                                tenet::logging::peer_id(&peer_id)
                            );
                        }
                    }
                    MetaMessage::Ack {
                        peer_id,
                        online_timestamp,
                    } => {
                        let st = state.lock().await;
                        let _ = st
                            .storage
                            .update_peer_online(&peer_id, true, online_timestamp);
                    }
                    MetaMessage::FriendRequest {
                        peer_id: from_peer_id,
                        signing_public_key,
                        encryption_public_key,
                        message,
                    } => {
                        tenet::tlog!(
                            "sync: received friend_request from {}",
                            tenet::logging::peer_id(&from_peer_id)
                        );
                        let st = state.lock().await;

                        // If already friends, treat as duplicate and skip
                        if let Ok(Some(peer)) = st.storage.get_peer(&from_peer_id) {
                            if peer.is_friend {
                                tenet::tlog!(
                                    "sync: friend request from {} is a duplicate (already friends), ignoring",
                                    tenet::logging::peer_id(&from_peer_id)
                                );
                                continue;
                            }
                        }

                        // Check if we have a pending OUTGOING request to this peer (race condition)
                        let outgoing = st
                            .storage
                            .find_request_between(&keypair.id, &from_peer_id)
                            .unwrap_or(None);

                        if let Some(ref out_req) = outgoing {
                            if out_req.status == "pending" {
                                // Race condition: both peers sent friend requests to each other.
                                // Auto-accept: mark our outgoing request as accepted, add them as a friend,
                                // and archive the incoming request as a duplicate.
                                tenet::tlog!(
                                    "sync: mutual friend request detected with {} — auto-accepting",
                                    tenet::logging::peer_id(&from_peer_id)
                                );
                                let _ = st
                                    .storage
                                    .update_friend_request_status(out_req.id, "accepted");

                                // Add peer as friend
                                let peer_row = PeerRow {
                                    peer_id: from_peer_id.clone(),
                                    display_name: None,
                                    signing_public_key: signing_public_key.clone(),
                                    encryption_public_key: Some(encryption_public_key.clone()),
                                    added_at: now,
                                    is_friend: true,
                                    last_seen_online: Some(now),
                                    online: false,
                                };
                                let _ = st.storage.insert_peer(&peer_row);

                                // Send FriendAccept back so the other peer also completes the handshake
                                drop(st);
                                {
                                    let meta_msg = MetaMessage::FriendAccept {
                                        peer_id: keypair.id.clone(),
                                        signing_public_key: keypair.signing_public_key_hex.clone(),
                                        encryption_public_key: keypair.public_key_hex.clone(),
                                    };
                                    if let Ok(payload) = build_meta_payload(&meta_msg) {
                                        if let Ok(env) = build_envelope_from_payload(
                                            keypair.id.clone(),
                                            from_peer_id.clone(),
                                            None,
                                            None,
                                            now,
                                            DEFAULT_TTL_SECONDS,
                                            MessageKind::Meta,
                                            None,
                                            payload,
                                            &keypair.signing_private_key_hex,
                                        ) {
                                            if let Err(e) = post_to_relay(&relay_url, &env) {
                                                tenet::tlog!(
                                                    "sync: failed to send auto-accept to {}: {}",
                                                    tenet::logging::peer_id(&from_peer_id),
                                                    e
                                                );
                                            } else {
                                                tenet::tlog!(
                                                    "sync: sent auto-accept to {} via relay",
                                                    tenet::logging::peer_id(&from_peer_id)
                                                );
                                            }
                                        }
                                    }
                                }
                                let st = state.lock().await;
                                let _ = st.ws_tx.send(WsEvent::FriendRequestAccepted {
                                    request_id: out_req.id,
                                    from_peer_id: from_peer_id.clone(),
                                });
                                continue;
                            }
                        }

                        // Check for existing request from this peer
                        let existing = st
                            .storage
                            .find_request_between(&from_peer_id, &keypair.id)
                            .unwrap_or(None);

                        if let Some(ref ex) = existing {
                            // If blocked or ignored, silently drop
                            if ex.status == "blocked" || ex.status == "ignored" {
                                tenet::tlog!(
                                    "sync: friend request from {} is {}, not resurfacing",
                                    tenet::logging::peer_id(&from_peer_id),
                                    ex.status
                                );
                                continue;
                            }
                            // If pending, refresh the existing record
                            if ex.status == "pending" {
                                if let Err(e) = st.storage.refresh_friend_request(
                                    ex.id,
                                    message.as_deref(),
                                    &signing_public_key,
                                    &encryption_public_key,
                                ) {
                                    tenet::tlog!(
                                        "sync: failed to refresh friend request from {}: {}",
                                        tenet::logging::peer_id(&from_peer_id),
                                        e
                                    );
                                } else {
                                    tenet::tlog!(
                                        "sync: refreshed existing friend request from {} (id={})",
                                        tenet::logging::peer_id(&from_peer_id),
                                        ex.id
                                    );
                                    let _ = st.ws_tx.send(WsEvent::FriendRequestReceived {
                                        request_id: ex.id,
                                        from_peer_id: from_peer_id.clone(),
                                        message: message.clone(),
                                    });
                                }
                                continue;
                            }
                            // If accepted, they are already friends — skip
                            if ex.status == "accepted" {
                                tenet::tlog!(
                                    "sync: friend request from {} already accepted, skipping",
                                    tenet::logging::peer_id(&from_peer_id)
                                );
                                continue;
                            }
                        }

                        // No existing request — create a new one
                        let fr_row = FriendRequestRow {
                            id: 0,
                            from_peer_id: from_peer_id.clone(),
                            to_peer_id: keypair.id.clone(),
                            status: "pending".to_string(),
                            message: message.clone(),
                            from_signing_key: signing_public_key,
                            from_encryption_key: encryption_public_key,
                            direction: "incoming".to_string(),
                            created_at: now,
                            updated_at: now,
                        };
                        match st.storage.insert_friend_request(&fr_row) {
                            Ok(request_id) => {
                                let _ = st.ws_tx.send(WsEvent::FriendRequestReceived {
                                    request_id,
                                    from_peer_id: from_peer_id.clone(),
                                    message: message.clone(),
                                });
                                tenet::tlog!(
                                    "sync: stored incoming friend request from {} (id={})",
                                    tenet::logging::peer_id(&from_peer_id),
                                    request_id
                                );
                            }
                            Err(e) => {
                                tenet::tlog!(
                                    "sync: failed to store friend request from {}: {}",
                                    tenet::logging::peer_id(&from_peer_id),
                                    e
                                );
                            }
                        }
                    }
                    MetaMessage::FriendAccept {
                        peer_id: from_peer_id,
                        signing_public_key,
                        encryption_public_key,
                    } => {
                        tenet::tlog!(
                            "sync: received friend_accept from {}",
                            tenet::logging::peer_id(&from_peer_id)
                        );
                        let st = state.lock().await;

                        // If already friends, treat as duplicate
                        if let Ok(Some(peer)) = st.storage.get_peer(&from_peer_id) {
                            if peer.is_friend {
                                tenet::tlog!(
                                    "sync: friend_accept from {} is a duplicate (already friends), ignoring",
                                    tenet::logging::peer_id(&from_peer_id)
                                );
                                continue;
                            }
                        }

                        let requests = st
                            .storage
                            .list_friend_requests(Some("pending"), Some("outgoing"))
                            .unwrap_or_default();
                        if let Some(pending) =
                            requests.iter().find(|r| r.to_peer_id == from_peer_id)
                        {
                            let req_id = pending.id;
                            let _ = st.storage.update_friend_request_status(req_id, "accepted");
                            let peer_row = PeerRow {
                                peer_id: from_peer_id.clone(),
                                display_name: None,
                                signing_public_key,
                                encryption_public_key: Some(encryption_public_key),
                                added_at: now,
                                is_friend: true,
                                last_seen_online: Some(now),
                                online: false,
                            };
                            let _ = st.storage.insert_peer(&peer_row);

                            // Also archive any incoming friend request from this peer (race condition)
                            if let Ok(Some(incoming)) =
                                st.storage.find_request_between(&from_peer_id, &keypair.id)
                            {
                                if incoming.status == "pending" {
                                    tenet::tlog!(
                                        "sync: archiving duplicate incoming request from {} (id={})",
                                        tenet::logging::peer_id(&from_peer_id), incoming.id
                                    );
                                    let _ = st
                                        .storage
                                        .update_friend_request_status(incoming.id, "accepted");
                                }
                            }

                            let _ = st.ws_tx.send(WsEvent::FriendRequestAccepted {
                                request_id: req_id,
                                from_peer_id: from_peer_id.clone(),
                            });
                            tenet::tlog!(
                                "sync: friend request accepted by {} (id={})",
                                tenet::logging::peer_id(&from_peer_id),
                                req_id
                            );
                        } else {
                            tenet::tlog!(
                                "sync: received friend_accept from {} but no matching pending request",
                                tenet::logging::peer_id(&from_peer_id)
                            );
                        }
                    }
                    _ => {}
                }
            }
            continue; // Meta messages are not stored as regular messages
        }

        // --- Non-meta messages: verify signature using known peers ---
        let sender_id = &envelope.header.sender_id;
        let peer = match peer_map.get(sender_id) {
            Some(p) => p,
            None => {
                tenet::tlog!(
                    "sync: skipping message from unknown sender {}",
                    tenet::logging::peer_id(sender_id)
                );
                continue;
            }
        };

        if let Err(e) = envelope
            .header
            .verify_signature(envelope.version, &peer.signing_public_key)
        {
            tenet::tlog!(
                "sync: invalid signature from {}: {:?}",
                tenet::logging::peer_id(sender_id),
                e
            );
            continue;
        }

        // Decrypt body for Direct messages, pass through for others
        let body = match envelope.header.message_kind {
            MessageKind::Direct => {
                match decrypt_encrypted_payload(
                    &envelope.payload,
                    &keypair.private_key_hex,
                    WEB_PAYLOAD_AAD,
                    WEB_HPKE_INFO,
                ) {
                    Ok(plaintext) => match String::from_utf8(plaintext) {
                        Ok(s) => Some(s),
                        Err(e) => {
                            tenet::tlog!(
                                "sync: utf-8 decode error from {}: {}",
                                tenet::logging::peer_id(sender_id),
                                e
                            );
                            continue;
                        }
                    },
                    Err(e) => {
                        tenet::tlog!(
                            "sync: decrypt error from {}: {}",
                            tenet::logging::peer_id(sender_id),
                            e
                        );
                        continue;
                    }
                }
            }
            _ => Some(envelope.payload.body.clone()),
        };

        let message_id = envelope.header.message_id.0.clone();
        let kind_str = match envelope.header.message_kind {
            MessageKind::Public => "public",
            MessageKind::Direct => "direct",
            MessageKind::FriendGroup => "friend_group",
            MessageKind::Meta => "meta",
            MessageKind::StoreForPeer => "store_for_peer",
        };

        // Check if this is a profile update (body contains "type": "tenet.profile")
        if let Some(ref body_str) = body {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(body_str) {
                if parsed.get("type").and_then(|t| t.as_str()) == Some("tenet.profile") {
                    let profile_user_id = parsed
                        .get("user_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or(sender_id)
                        .to_string();
                    let updated_at = parsed
                        .get("updated_at")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(envelope.header.timestamp);

                    let profile_row = ProfileRow {
                        user_id: profile_user_id.clone(),
                        display_name: parsed
                            .get("display_name")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string()),
                        bio: parsed
                            .get("bio")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string()),
                        avatar_hash: parsed
                            .get("avatar_hash")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string()),
                        public_fields: parsed
                            .get("public_fields")
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "{}".to_string()),
                        friends_fields: parsed
                            .get("friends_fields")
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "{}".to_string()),
                        updated_at,
                    };

                    let st = state.lock().await;
                    match st.storage.upsert_profile_if_newer(&profile_row) {
                        Ok(true) => {
                            tenet::tlog!(
                                "sync: updated profile for {}",
                                tenet::logging::peer_id(&profile_user_id)
                            );
                            // Update peer display name if it changed
                            if let Some(ref display_name) = profile_row.display_name {
                                if let Ok(Some(mut peer)) = st.storage.get_peer(&profile_user_id) {
                                    if peer.display_name.as_deref() != Some(display_name) {
                                        peer.display_name = Some(display_name.clone());
                                        let _ = st.storage.insert_peer(&peer);
                                        tenet::tlog!(
                                            "sync: updated display name for peer {}",
                                            tenet::logging::peer_id(&profile_user_id)
                                        );
                                    }
                                }
                            }
                        }
                        Ok(false) => {
                            tenet::tlog!(
                                "sync: profile for {} is not newer, skipping",
                                tenet::logging::peer_id(&profile_user_id)
                            );
                        }
                        Err(e) => {
                            tenet::tlog!(
                                "sync: failed to update profile for {}: {}",
                                tenet::logging::peer_id(&profile_user_id),
                                e
                            );
                        }
                    }
                    continue; // Don't store profile updates as timeline messages
                }

                // Also check for group key distribution messages — don't show in timeline
                if parsed.get("type").and_then(|t| t.as_str()) == Some("group_key_distribution") {
                    // Handle group key distribution (existing logic would go here)
                    // For now, just skip showing in timeline
                    continue;
                }
            }
        }

        let st = state.lock().await;
        if st.storage.has_message(&message_id).unwrap_or(true) {
            continue;
        }

        // Update last_seen_online for the sender when we receive a message from them
        if let Ok(Some(_)) = st.storage.get_peer(sender_id) {
            let _ = st
                .storage
                .update_peer_online(sender_id, false, envelope.header.timestamp);
        }

        tenet::tlog!(
            "sync: received {} message from {} (id={})",
            kind_str,
            tenet::logging::peer_id(sender_id),
            tenet::logging::msg_id(&message_id)
        );

        let row = MessageRow {
            message_id: message_id.clone(),
            sender_id: sender_id.clone(),
            recipient_id: keypair.id.clone(),
            message_kind: kind_str.to_string(),
            group_id: envelope.header.group_id.clone(),
            body,
            timestamp: envelope.header.timestamp,
            received_at: now,
            ttl_seconds: DEFAULT_TTL_SECONDS,
            is_read: false,
            raw_envelope: None,
            reply_to: None,
        };
        if st.storage.insert_message(&row).is_ok() {
            let _ = st.ws_tx.send(WsEvent::NewMessage {
                message_id: message_id.clone(),
                sender_id: sender_id.clone(),
                message_kind: kind_str.to_string(),
                body: row.body.clone(),
                timestamp: envelope.header.timestamp,
            });
            stored_count += 1;
        }
    }

    {
        let st = state.lock().await;
        let _ = st.storage.update_relay_last_sync(&relay_url, now);
    }

    if stored_count > 0 {
        tenet::tlog!("sync: stored {} message(s)", stored_count);
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
        if let Err(e) = post_to_relay(&relay_url, &envelope) {
            tenet::tlog!(
                "failed to announce online to {}: {}",
                tenet::logging::peer_id(&peer.peer_id),
                e
            );
        }
    }

    tenet::tlog!("announced online status to {} peers", peers.len());
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Post an envelope to the relay. Returns Ok(()) on success, or an error
/// string describing what went wrong.
fn post_to_relay(relay_url: &str, envelope: &tenet::protocol::Envelope) -> Result<(), String> {
    let url = format!("{}/envelopes", relay_url.trim_end_matches('/'));
    let json_val =
        serde_json::to_value(envelope).map_err(|e| format!("failed to serialize envelope: {e}"))?;
    ureq::post(&url)
        .send_json(json_val)
        .map_err(|e| format!("relay POST failed: {e}"))?;
    Ok(())
}

/// Validate a hex-encoded key has the expected byte length.
fn validate_hex_key(hex: &str, expected_len: usize, field_name: &str) -> Result<(), String> {
    let bytes = hex::decode(hex).map_err(|_| format!("{field_name} is not valid hex"))?;
    if bytes.len() != expected_len {
        return Err(format!(
            "{field_name} must be {expected_len} bytes, got {}",
            bytes.len()
        ));
    }
    Ok(())
}

/// Link uploaded attachments to a message by inserting into message_attachments.
fn link_attachments(storage: &Storage, message_id: &str, attachments: &[SendAttachmentRef]) {
    for (i, att) in attachments.iter().enumerate() {
        let _ = storage.insert_message_attachment(&MessageAttachmentRow {
            message_id: message_id.to_string(),
            content_hash: att.content_hash.clone(),
            filename: att.filename.clone(),
            position: i as u32,
        });
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
