//! Shared application state and WebSocket event types.

use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::Arc;

use tokio::sync::{broadcast, Mutex};

use crate::crypto::StoredKeypair;
use crate::storage::Storage;

/// Events broadcast to connected WebSocket clients.
#[derive(Clone, Debug, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(dead_code)]
pub enum WsEvent {
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

pub struct AppState {
    pub storage: Storage,
    pub keypair: StoredKeypair,
    pub relay_url: Option<String>,
    pub ws_tx: broadcast::Sender<WsEvent>,
    pub ws_connection_count: Arc<AtomicUsize>,
    pub relay_connected: Arc<AtomicBool>,
}

pub type SharedState = Arc<Mutex<AppState>>;
