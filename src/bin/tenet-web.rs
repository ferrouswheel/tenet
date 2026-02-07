//! tenet-web: Web server binary that acts as a full tenet peer.
//!
//! Serves a minimal SPA, connects to relays, and persists state in SQLite.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use rust_embed::Embed;
use tokio::sync::Mutex;

use tenet::client::{ClientConfig, ClientEncryption, RelayClient};
use tenet::crypto::{generate_keypair, StoredKeypair};
use tenet::storage::{db_path, IdentityRow, RelayRow, Storage};

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
// Shared application state
// ---------------------------------------------------------------------------

struct AppState {
    storage: Storage,
    keypair: StoredKeypair,
    relay_url: Option<String>,
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

    let state: SharedState = Arc::new(Mutex::new(AppState {
        storage,
        keypair: keypair.clone(),
        relay_url: config.relay_url.clone(),
    }));

    // Start background relay sync task
    if config.relay_url.is_some() {
        let sync_state = Arc::clone(&state);
        tokio::spawn(async move {
            relay_sync_loop(sync_state).await;
        });
    }

    // Build Axum router
    let app = Router::new()
        .route("/api/health", get(health_handler))
        .fallback(get(static_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&config.bind_addr)
        .await
        .expect("failed to bind");
    eprintln!("tenet-web listening on {}", config.bind_addr);

    axum::serve(listener, app).await.expect("server error");
}

// ---------------------------------------------------------------------------
// Handlers
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

    if outcome.fetched == 0 {
        return Ok(());
    }

    // Store received messages in SQLite
    let st = state.lock().await;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    for msg in &outcome.messages {
        // Check if message already stored
        if st.storage.has_message(&msg.message_id).unwrap_or(true) {
            continue;
        }

        let row = tenet::storage::MessageRow {
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
        let _ = st.storage.insert_message(&row);
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
