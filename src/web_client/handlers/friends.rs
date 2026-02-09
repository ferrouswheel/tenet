//! Friend request handlers.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::protocol::{build_envelope_from_payload, build_meta_payload, MessageKind, MetaMessage};
use crate::relay_transport::post_envelope;
use crate::storage::{FriendRequestRow, PeerRow};
use crate::web_client::config::DEFAULT_TTL_SECONDS;
use crate::web_client::state::SharedState;
use crate::web_client::utils::{api_error, now_secs};

#[derive(Deserialize)]
pub struct SendFriendRequestPayload {
    peer_id: String,
    message: Option<String>,
    #[serde(default)]
    force: bool,
}

#[derive(Deserialize)]
pub struct ListFriendRequestsQuery {
    status: Option<String>,
    direction: Option<String>,
}

pub async fn send_friend_request_handler(
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
            crate::tlog!(
                "friend-request: resending to {} (refreshed id={})",
                crate::logging::peer_id(&peer_id),
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
                if let Err(e) = post_envelope(relay, &envelope) {
                    crate::tlog!(
                        "friend-request: failed to send to relay for {}: {}",
                        crate::logging::peer_id(&peer_id),
                        e
                    );
                } else {
                    crate::tlog!(
                        "friend-request: sent to {} via relay",
                        crate::logging::peer_id(&peer_id)
                    );
                }
            }
        }
    } else {
        crate::tlog!("friend-request: no relay configured, request stored locally only");
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

pub async fn list_friend_requests_handler(
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

pub async fn accept_friend_request_handler(
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
        crate::tlog!("failed to add peer from friend request: {}", e);
    }

    // Archive any outgoing request we sent to this peer (race condition)
    if let Ok(Some(outgoing)) = st
        .storage
        .find_request_between(&keypair.id, &fr.from_peer_id)
    {
        if outgoing.status == "pending" {
            crate::tlog!(
                "friend-accept: archiving duplicate outgoing request to {} (id={})",
                crate::logging::peer_id(&fr.from_peer_id),
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
                if let Err(e) = post_envelope(relay, &envelope) {
                    crate::tlog!(
                        "friend-accept: failed to send to relay for {}: {}",
                        crate::logging::peer_id(&fr.from_peer_id),
                        e
                    );
                } else {
                    crate::tlog!(
                        "friend-accept: sent to {} via relay",
                        crate::logging::peer_id(&fr.from_peer_id)
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

pub async fn ignore_friend_request_handler(
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

pub async fn block_friend_request_handler(
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
