//! Peer management handlers.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use hex;
use serde::Deserialize;

use crate::crypto::{derive_user_id_from_public_key, validate_hex_key};
use crate::protocol::{build_envelope_from_payload, build_meta_payload, MessageKind, MetaMessage};
use crate::relay_transport::post_envelope;
use crate::storage::{FriendRequestRow, PeerRow};
use crate::web_client::config::DEFAULT_TTL_SECONDS;
use crate::web_client::state::SharedState;
use crate::web_client::utils::{api_error, message_to_json, now_secs};

/// Build the JSON representation of a peer (without activity aggregates).
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
        "is_blocked": p.is_blocked,
        "is_muted": p.is_muted,
        "blocked_at": p.blocked_at,
        "muted_at": p.muted_at,
    })
}

pub async fn list_peers_handler(State(state): State<SharedState>) -> Response {
    let st = state.lock().await;
    match st.storage.list_peers() {
        Ok(peers) => {
            let json: Vec<serde_json::Value> = peers
                .iter()
                .map(|p| {
                    let mut j = peer_to_json(p);
                    let avatar_hash = st
                        .storage
                        .get_profile(&p.peer_id)
                        .ok()
                        .flatten()
                        .and_then(|prof| prof.avatar_hash);
                    j["avatar_hash"] = serde_json::json!(avatar_hash);

                    // Last-message / last-post aggregates
                    if let Ok(Some((ts, preview))) = st.storage.peer_last_message(&p.peer_id) {
                        j["last_message_at"] = serde_json::json!(ts);
                        j["last_message_preview"] = serde_json::json!(preview);
                    } else {
                        j["last_message_at"] = serde_json::Value::Null;
                        j["last_message_preview"] = serde_json::Value::Null;
                    }
                    if let Ok(Some((ts, preview))) = st.storage.peer_last_post(&p.peer_id) {
                        j["last_post_at"] = serde_json::json!(ts);
                        j["last_post_preview"] = serde_json::json!(preview);
                    } else {
                        j["last_post_at"] = serde_json::Value::Null;
                        j["last_post_preview"] = serde_json::Value::Null;
                    }
                    j
                })
                .collect();
            (StatusCode::OK, axum::Json(serde_json::json!(json))).into_response()
        }
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub async fn get_peer_handler(
    State(state): State<SharedState>,
    Path(peer_id): Path<String>,
) -> Response {
    // Check if peer exists; if not, create a placeholder.
    let (peer_exists, should_request_profile) = {
        let st = state.lock().await;
        match st.storage.get_peer(&peer_id) {
            Ok(Some(ref p)) => {
                let has_profile = st.storage.get_profile(&p.peer_id).ok().flatten().is_some();
                let never_requested = p.last_profile_requested_at.is_none();
                let is_placeholder = p.signing_public_key.is_empty();

                // Request if: placeholder, or never requested, or no profile exists
                (true, is_placeholder || never_requested || !has_profile)
            }
            Ok(None) => {
                // Peer doesn't exist - create a placeholder
                let now = now_secs();
                let placeholder = create_placeholder_peer(&peer_id, now);
                if let Ok(()) = st.storage.insert_peer(&placeholder) {
                    crate::tlog!(
                        "created placeholder peer {} for profile view",
                        crate::logging::peer_id(&peer_id)
                    );
                }
                (true, true) // Created placeholder, need to request profile
            }
            Err(e) => {
                return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
            }
        }
    };

    // Send profile request if needed (do this outside the lock to avoid deadlock).
    if peer_exists && should_request_profile {
        let _ = request_profile_for_peer(&state, &peer_id).await;
    }

    // Return peer data.
    let st = state.lock().await;
    match st.storage.get_peer(&peer_id) {
        Ok(Some(p)) => {
            let mut j = peer_to_json(&p);
            let avatar_hash = st
                .storage
                .get_profile(&p.peer_id)
                .ok()
                .flatten()
                .and_then(|prof| prof.avatar_hash);
            j["avatar_hash"] = serde_json::json!(avatar_hash);

            if let Ok(Some((ts, preview))) = st.storage.peer_last_message(&p.peer_id) {
                j["last_message_at"] = serde_json::json!(ts);
                j["last_message_preview"] = serde_json::json!(preview);
            } else {
                j["last_message_at"] = serde_json::Value::Null;
                j["last_message_preview"] = serde_json::Value::Null;
            }
            if let Ok(Some((ts, preview))) = st.storage.peer_last_post(&p.peer_id) {
                j["last_post_at"] = serde_json::json!(ts);
                j["last_post_preview"] = serde_json::json!(preview);
            } else {
                j["last_post_at"] = serde_json::Value::Null;
                j["last_post_preview"] = serde_json::Value::Null;
            }
            (StatusCode::OK, axum::Json(j)).into_response()
        }
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
        last_profile_requested_at: None,
        last_profile_responded_at: None,
        is_blocked: false,
        is_muted: false,
        blocked_at: None,
        muted_at: None,
    };

    match st.storage.insert_peer(&peer_row) {
        Ok(()) => {
            // Verify the key → peer_id binding and backfill any unverified
            // messages we received from this peer before knowing their key.
            if let Ok(key_bytes) = hex::decode(&req.signing_public_key) {
                let derived = derive_user_id_from_public_key(&key_bytes);
                if derived == req.peer_id {
                    crate::message_handler::StorageMessageHandler::backfill_for_storage(
                        &st.storage,
                        &req.peer_id,
                        &req.signing_public_key,
                    );
                }
            }
            (StatusCode::CREATED, axum::Json(peer_to_json(&peer_row))).into_response()
        }
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

// ---------------------------------------------------------------------------
// Block / Mute actions
// ---------------------------------------------------------------------------

pub async fn block_peer_handler(
    State(state): State<SharedState>,
    Path(peer_id): Path<String>,
) -> Response {
    let st = state.lock().await;
    match st.storage.set_peer_blocked(&peer_id, true) {
        Ok(()) => (
            StatusCode::OK,
            axum::Json(serde_json::json!({"status": "ok"})),
        )
            .into_response(),
        Err(crate::storage::StorageError::NotFound(_)) => {
            api_error(StatusCode::NOT_FOUND, "peer not found")
        }
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub async fn unblock_peer_handler(
    State(state): State<SharedState>,
    Path(peer_id): Path<String>,
) -> Response {
    let st = state.lock().await;
    match st.storage.set_peer_blocked(&peer_id, false) {
        Ok(()) => (
            StatusCode::OK,
            axum::Json(serde_json::json!({"status": "ok"})),
        )
            .into_response(),
        Err(crate::storage::StorageError::NotFound(_)) => {
            api_error(StatusCode::NOT_FOUND, "peer not found")
        }
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub async fn mute_peer_handler(
    State(state): State<SharedState>,
    Path(peer_id): Path<String>,
) -> Response {
    let st = state.lock().await;
    match st.storage.set_peer_muted(&peer_id, true) {
        Ok(()) => (
            StatusCode::OK,
            axum::Json(serde_json::json!({"status": "ok"})),
        )
            .into_response(),
        Err(crate::storage::StorageError::NotFound(_)) => {
            api_error(StatusCode::NOT_FOUND, "peer not found")
        }
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub async fn unmute_peer_handler(
    State(state): State<SharedState>,
    Path(peer_id): Path<String>,
) -> Response {
    let st = state.lock().await;
    match st.storage.set_peer_muted(&peer_id, false) {
        Ok(()) => (
            StatusCode::OK,
            axum::Json(serde_json::json!({"status": "ok"})),
        )
            .into_response(),
        Err(crate::storage::StorageError::NotFound(_)) => {
            api_error(StatusCode::NOT_FOUND, "peer not found")
        }
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Friend-request convenience wrapper
// ---------------------------------------------------------------------------

pub async fn peer_friend_request_handler(
    State(state): State<SharedState>,
    Path(peer_id): Path<String>,
) -> Response {
    let (keypair, relay_url, existing) = {
        let st = state.lock().await;

        if peer_id == st.keypair.id {
            return api_error(
                StatusCode::BAD_REQUEST,
                "cannot send friend request to yourself",
            );
        }

        if let Ok(Some(peer)) = st.storage.get_peer(&peer_id) {
            if peer.is_friend {
                return api_error(StatusCode::CONFLICT, "already friends with this peer");
            }
        } else if st.storage.get_peer(&peer_id).unwrap_or(None).is_none() {
            return api_error(StatusCode::NOT_FOUND, "peer not found");
        }

        let existing = st
            .storage
            .find_request_between(&st.keypair.id, &peer_id)
            .unwrap_or(None);

        if let Some(ref ex) = existing {
            if ex.status == "pending" {
                return api_error(StatusCode::CONFLICT, "friend request already pending");
            }
        }

        (st.keypair.clone(), st.relay_url.clone(), existing)
    };

    let now = now_secs();

    {
        let st = state.lock().await;
        if let Some(ref ex) = existing {
            if let Err(e) = st.storage.refresh_friend_request(
                ex.id,
                None,
                &keypair.signing_public_key_hex,
                &keypair.public_key_hex,
            ) {
                return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
            }
        } else {
            let fr_row = FriendRequestRow {
                id: 0,
                from_peer_id: keypair.id.clone(),
                to_peer_id: peer_id.clone(),
                status: "pending".to_string(),
                message: None,
                from_signing_key: keypair.signing_public_key_hex.clone(),
                from_encryption_key: keypair.public_key_hex.clone(),
                direction: "outgoing".to_string(),
                created_at: now,
                updated_at: now,
            };
            if let Err(e) = st.storage.insert_friend_request(&fr_row) {
                return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
            }
        }
    }

    if let Some(ref relay) = relay_url {
        let meta_msg = MetaMessage::FriendRequest {
            peer_id: keypair.id.clone(),
            signing_public_key: keypair.signing_public_key_hex.clone(),
            encryption_public_key: keypair.public_key_hex.clone(),
            message: None,
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
                None,
                payload,
                &keypair.signing_private_key_hex,
            ) {
                let _ = post_envelope(relay, &envelope);
            }
        }
    }

    (
        StatusCode::OK,
        axum::Json(serde_json::json!({"status": "ok"})),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Profile-request convenience wrapper
// ---------------------------------------------------------------------------

pub async fn peer_request_profile_handler(
    State(state): State<SharedState>,
    Path(peer_id): Path<String>,
) -> Response {
    // Ensure peer exists; create placeholder if needed.
    {
        let st = state.lock().await;
        if st.storage.get_peer(&peer_id).unwrap_or(None).is_none() {
            let now = now_secs();
            let placeholder = create_placeholder_peer(&peer_id, now);
            if let Ok(()) = st.storage.insert_peer(&placeholder) {
                crate::tlog!(
                    "created placeholder peer {} for profile request",
                    crate::logging::peer_id(&peer_id)
                );
            }
        }
    }

    let (keypair, relay_url) = {
        let st = state.lock().await;
        (st.keypair.clone(), st.relay_url.clone())
    };

    let relay = match relay_url {
        Some(r) => r,
        None => return api_error(StatusCode::BAD_REQUEST, "no relay configured"),
    };

    let now = now_secs();
    let meta_msg = MetaMessage::ProfileRequest {
        peer_id: keypair.id.clone(),
        for_peer_id: peer_id.clone(),
    };

    let payload = match build_meta_payload(&meta_msg) {
        Ok(p) => p,
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };

    let envelope = match build_envelope_from_payload(
        keypair.id.clone(),
        peer_id.clone(),
        None,
        None,
        now,
        DEFAULT_TTL_SECONDS,
        MessageKind::Meta,
        None,
        None,
        payload,
        &keypair.signing_private_key_hex,
    ) {
        Ok(e) => e,
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };

    if let Err(e) = post_envelope(&relay, &envelope) {
        return api_error(StatusCode::BAD_GATEWAY, e.to_string());
    }

    let st = state.lock().await;
    let _ = st.storage.record_profile_request_sent(&peer_id, now);

    (
        StatusCode::OK,
        axum::Json(serde_json::json!({"status": "ok"})),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Peer activity (recent posts/messages by this peer)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct PeerActivityQuery {
    limit: Option<u32>,
    before: Option<u64>,
}

pub async fn peer_activity_handler(
    State(state): State<SharedState>,
    Path(peer_id): Path<String>,
    Query(q): Query<PeerActivityQuery>,
) -> Response {
    // Check if peer exists; if not, create a placeholder.
    {
        let st = state.lock().await;
        if st.storage.get_peer(&peer_id).unwrap_or(None).is_none() {
            // Create a placeholder peer so activity can be fetched
            let now = now_secs();
            let placeholder = create_placeholder_peer(&peer_id, now);
            if let Ok(()) = st.storage.insert_peer(&placeholder) {
                crate::tlog!(
                    "created placeholder peer {} for activity view",
                    crate::logging::peer_id(&peer_id)
                );
            }
        }
    }

    let st = state.lock().await;
    let limit = q.limit.unwrap_or(20).min(100);
    match st.storage.list_peer_activity(&peer_id, q.before, limit) {
        Ok(msgs) => {
            let json: Vec<serde_json::Value> = msgs
                .iter()
                .map(|m| message_to_json(m, &st.storage))
                .collect();
            (StatusCode::OK, axum::Json(serde_json::json!(json))).into_response()
        }
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Forget peer (delete peer and all messages)
// ---------------------------------------------------------------------------

pub async fn forget_peer_handler(
    State(state): State<SharedState>,
    Path(peer_id): Path<String>,
) -> Response {
    let st = state.lock().await;

    // Check if peer exists and is not a friend
    match st.storage.get_peer(&peer_id) {
        Ok(Some(peer)) => {
            if peer.is_friend {
                return api_error(
                    StatusCode::BAD_REQUEST,
                    "cannot forget a friend; remove friendship first",
                );
            }
        }
        Ok(None) => {
            return api_error(StatusCode::NOT_FOUND, "peer not found");
        }
        Err(e) => {
            return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
        }
    }

    // Delete peer and all their data
    match st.storage.forget_peer(&peer_id) {
        Ok((peer_deleted, messages_deleted)) => {
            crate::tlog!(
                "forgot peer {} (messages deleted: {})",
                crate::logging::peer_id(&peer_id),
                messages_deleted
            );
            (
                StatusCode::OK,
                axum::Json(serde_json::json!({
                    "peer_deleted": peer_deleted,
                    "messages_deleted": messages_deleted,
                })),
            )
                .into_response()
        }
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Helper: Create placeholder peer
// ---------------------------------------------------------------------------

/// Create a placeholder peer record for a peer we don't have locally yet.
/// The `added_at` timestamp represents when we created this placeholder.
fn create_placeholder_peer(peer_id: &str, now: u64) -> PeerRow {
    PeerRow {
        peer_id: peer_id.to_string(),
        display_name: None,
        signing_public_key: String::new(), // Empty until we learn it
        encryption_public_key: None,
        added_at: now,
        is_friend: false,
        last_seen_online: None,
        online: false,
        last_profile_requested_at: None,
        last_profile_responded_at: None,
        is_blocked: false,
        is_muted: false,
        blocked_at: None,
        muted_at: None,
    }
}

// ---------------------------------------------------------------------------
// Helper: Request profile for a specific peer
// ---------------------------------------------------------------------------

/// Send a ProfileRequest to the specified peer immediately.
/// Called when the user views a peer's profile page.
async fn request_profile_for_peer(state: &SharedState, peer_id: &str) -> Result<(), String> {
    let (keypair, relay_url, db_path) = {
        let st = state.lock().await;
        let relay_url = st
            .relay_url
            .clone()
            .ok_or_else(|| "no relay configured".to_string())?;
        (st.keypair.clone(), relay_url, st.db_path.clone())
    };

    let storage = crate::storage::Storage::open(&db_path)
        .map_err(|e| format!("profile request storage: {e}"))?;
    let now = now_secs();

    let meta_msg = MetaMessage::ProfileRequest {
        peer_id: keypair.id.clone(),
        for_peer_id: peer_id.to_string(),
    };

    let payload = build_meta_payload(&meta_msg).map_err(|e| e.to_string())?;

    let envelope = build_envelope_from_payload(
        keypair.id.clone(),
        peer_id.to_string(),
        None,
        None,
        now,
        DEFAULT_TTL_SECONDS,
        MessageKind::Meta,
        None,
        None,
        payload,
        &keypair.signing_private_key_hex,
    )
    .map_err(|e| e.to_string())?;

    match post_envelope(&relay_url, &envelope) {
        Ok(()) => {
            crate::tlog!(
                "profile request (on-demand): POST successful to relay {} for peer {} (message_id: {})",
                relay_url,
                crate::logging::peer_id(peer_id),
                crate::logging::msg_id(&envelope.header.message_id.0)
            );
        }
        Err(e) => {
            crate::tlog!(
                "profile request (on-demand): POST failed to relay {} for peer {}: {}",
                relay_url,
                crate::logging::peer_id(peer_id),
                e
            );
            return Err(e.to_string());
        }
    }

    if let Err(e) = storage.record_profile_request_sent(peer_id, now) {
        crate::tlog!(
            "profile request (on-demand): failed to record request: {}",
            e
        );
    }

    Ok(())
}
