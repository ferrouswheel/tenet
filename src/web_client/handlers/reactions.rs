//! Reaction handlers.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::protocol::{build_plaintext_envelope, MessageKind};
use crate::relay_transport::post_envelope;
use crate::storage::ReactionRow;
use crate::web_client::config::DEFAULT_TTL_SECONDS;
use crate::web_client::state::SharedState;
use crate::web_client::utils::{api_error, now_secs};

#[derive(Deserialize)]
pub struct ReactRequest {
    reaction: String,
}

pub async fn react_handler(
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
        MessageKind::Meta,
        None,
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
        if let Err(e) = post_envelope(relay_url, &envelope) {
            crate::tlog!("failed to post reaction to relay: {}", e);
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

pub async fn unreact_handler(
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

pub async fn list_reactions_handler(
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
