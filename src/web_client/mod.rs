//! tenet-web: Web server module that acts as a full tenet peer.
//!
//! Serves an embedded SPA, provides REST API + WebSocket for messages,
//! connects to relays, and persists state in SQLite.

pub mod config;
pub mod handlers;
pub mod router;
pub mod state;
pub mod static_files;
pub mod sync;
pub mod utils;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::sync::Notify;

use clap::Parser;
use tokio::sync::broadcast;

use crate::identity::{resolve_identity, store_relay_for_identity};
use crate::storage::db_path;

use config::{Cli, Config, WS_CHANNEL_CAPACITY};
use state::{AppState, SharedState};

/// Entry point: parse CLI, resolve identity, start server.
pub async fn run() {
    let cli = Cli::parse();
    let config = Config::from_cli_and_env(cli);

    crate::logging::init();

    crate::tlog!("tenet-web starting");
    crate::tlog!("  data directory: {}", config.data_dir.display());

    // Resolve identity (handles migration, creation, and selection)
    let resolved = resolve_identity(&config.data_dir, config.identity.as_deref())
        .expect("failed to resolve identity");

    let keypair = resolved.keypair;
    let storage = resolved.storage;

    if resolved.newly_created {
        crate::tlog!(
            "  identities: 1 available (newly created), using: {}",
            crate::logging::peer_id(&keypair.id)
        );
    } else {
        crate::tlog!(
            "  identities: {} available, using: {}",
            resolved.total_identities,
            crate::logging::peer_id(&keypair.id)
        );
    }
    crate::tlog!("  database: {}", db_path(&resolved.identity_dir).display());

    // Resolve relay URL: CLI/env > stored in identity DB > none
    let relay_url = config.relay_url.or(resolved.stored_relay_url);

    // Store relay URL in identity's database if provided via CLI/env
    if let Some(ref url) = relay_url {
        let _ = store_relay_for_identity(&storage, url);
    }

    match &relay_url {
        Some(url) => crate::tlog!("  relay: {}", url),
        None => crate::tlog!("  relay: none configured (messages will be local-only)"),
    }

    // Create WebSocket broadcast channel
    let (ws_tx, _) = broadcast::channel(WS_CHANNEL_CAPACITY);

    let ws_connection_count = Arc::new(AtomicUsize::new(0));
    let relay_connected = Arc::new(AtomicBool::new(false));

    let state: SharedState = Arc::new(tokio::sync::Mutex::new(AppState {
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
        match sync::sync_once(&check_state).await {
            Ok(()) => {
                relay_connected.store(true, Ordering::Relaxed);
                crate::tlog!("  relay status: connected");
            }
            Err(e) => {
                crate::tlog!("  WARNING: relay is unreachable: {}", e);
                crate::tlog!("  The web UI will show the relay as unavailable.");
                crate::tlog!("  Background sync will retry with exponential backoff.");
            }
        }

        // Shared signal: the WS listener notifies this when an envelope arrives
        // so the sync loop wakes up immediately instead of waiting the full poll
        // interval.
        let notify = Arc::new(Notify::new());

        let sync_state = Arc::clone(&state);
        let sync_notify = Arc::clone(&notify);
        tokio::spawn(async move {
            sync::relay_sync_loop(sync_state, sync_notify).await;
        });

        // Connect to the relay's WebSocket push endpoint so messages are
        // delivered in near-real-time rather than waiting for the next poll.
        let ws_state = Arc::clone(&state);
        let ws_notify = Arc::clone(&notify);
        tokio::spawn(async move {
            sync::relay_ws_listen_loop(ws_state, ws_notify).await;
        });

        // Send online announcement to known peers
        let announce_state = Arc::clone(&state);
        tokio::spawn(async move {
            if let Err(e) = sync::announce_online(announce_state).await {
                crate::tlog!("failed to announce online status: {}", e);
            }
        });
    }

    // Build Axum router
    let app = router::build_router(state);

    let listener = tokio::net::TcpListener::bind(&config.bind_addr)
        .await
        .expect("failed to bind");
    crate::tlog!("tenet-web listening on http://{}", config.bind_addr);

    axum::serve(listener, app).await.expect("server error");
}
