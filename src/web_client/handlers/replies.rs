//! Reply handlers.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::protocol::{build_envelope_from_payload, build_plaintext_envelope, MessageKind};
use crate::relay_transport::post_envelope;
use crate::storage::MessageRow;
use crate::web_client::config::DEFAULT_TTL_SECONDS;
use crate::web_client::state::{SharedState, WsEvent};
use crate::web_client::utils::{
    api_error, link_attachments, message_to_json, now_secs, SendAttachmentRef,
};

#[derive(Deserialize)]
pub struct ReplyRequest {
    body: String,
    #[serde(default)]
    attachments: Vec<SendAttachmentRef>,
}

#[derive(Deserialize)]
pub struct ListRepliesQuery {
    before: Option<u64>,
    limit: Option<u32>,
}

pub async fn list_replies_handler(
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

pub async fn reply_handler(
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
        let payload = match crate::protocol::build_group_message_payload(
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
        match post_envelope(relay_url, &envelope) {
            Ok(()) => true,
            Err(e) => {
                crate::tlog!("failed to post reply to relay: {}", e);
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

        let _ = st.storage.insert_outbox(&crate::storage::OutboxRow {
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
