//! Health check endpoint and manual sync trigger.

use std::sync::atomic::Ordering;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;

use crate::web_client::state::{SharedState, WsEvent};
use crate::web_client::sync::sync_once;

pub async fn health_handler(State(state): State<SharedState>) -> impl IntoResponse {
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
        "encryption_public_key": state.keypair.public_key_hex,
        "relay": relay_url,
        "relay_connected": relay_connected,
        "peers": peer_count,
        "has_messages": message_count > 0,
        "build_timestamp": env!("BUILD_TIMESTAMP"),
    });
    (StatusCode::OK, axum::Json(body))
}

/// Trigger an immediate relay sync attempt, updating relay status via WebSocket.
pub async fn sync_now_handler(State(state): State<SharedState>) -> impl IntoResponse {
    match sync_once(&state).await {
        Ok(()) => {
            let st = state.lock().await;
            let was_connected = st.relay_connected.swap(true, Ordering::Relaxed);
            if !was_connected {
                let relay_url = st.relay_url.clone();
                let _ = st.ws_tx.send(WsEvent::RelayStatus {
                    connected: true,
                    relay_url,
                });
            }
            (
                StatusCode::OK,
                axum::Json(serde_json::json!({"status": "ok"})),
            )
        }
        Err(e) => {
            let st = state.lock().await;
            st.relay_connected.store(false, Ordering::Relaxed);
            let relay_url = st.relay_url.clone();
            let _ = st.ws_tx.send(WsEvent::RelayStatus {
                connected: false,
                relay_url,
            });
            (
                StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(serde_json::json!({"status": "error", "message": e})),
            )
        }
    }
}
