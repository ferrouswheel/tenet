//! Peer management handlers.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::crypto::validate_hex_key;
use crate::storage::PeerRow;
use crate::web_client::state::SharedState;
use crate::web_client::utils::{api_error, now_secs};

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

pub async fn list_peers_handler(State(state): State<SharedState>) -> Response {
    let st = state.lock().await;
    match st.storage.list_peers() {
        Ok(peers) => {
            let json: Vec<serde_json::Value> = peers.iter().map(peer_to_json).collect();
            (StatusCode::OK, axum::Json(serde_json::json!(json))).into_response()
        }
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub async fn get_peer_handler(
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
pub struct AddPeerRequest {
    peer_id: String,
    display_name: Option<String>,
    signing_public_key: String,
    encryption_public_key: Option<String>,
}

pub async fn add_peer_handler(
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

pub async fn delete_peer_handler(
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
