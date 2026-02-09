//! Health check endpoint.

use std::sync::atomic::Ordering;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;

use crate::web_client::state::SharedState;

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
        "relay": relay_url,
        "relay_connected": relay_connected,
        "peers": peer_count,
        "has_messages": message_count > 0,
    });
    (StatusCode::OK, axum::Json(body))
}
