//! Message listing, retrieval, and sending handlers.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::crypto::{generate_content_key, NONCE_SIZE};
use crate::protocol::{
    build_encrypted_payload, build_envelope_from_payload, build_plaintext_envelope, MessageKind,
};
use crate::relay_transport::post_envelope;
use crate::storage::MessageRow;
use crate::web_client::config::{DEFAULT_TTL_SECONDS, WEB_HPKE_INFO, WEB_PAYLOAD_AAD};
use crate::web_client::state::{SharedState, WsEvent};
use crate::web_client::utils::{
    api_error, link_attachments, message_to_json, now_secs, SendAttachmentRef,
};

#[derive(Deserialize)]
pub struct ListMessagesQuery {
    kind: Option<String>,
    group: Option<String>,
    before: Option<u64>,
    limit: Option<u32>,
}

pub async fn list_messages_handler(
    State(state): State<SharedState>,
    Query(params): Query<ListMessagesQuery>,
) -> Response {
    let st = state.lock().await;
    let limit = params.limit.unwrap_or(50).min(200);

    match st.storage.list_messages(
        params.kind.as_deref(),
        params.group.as_deref(),
        params.before,
        limit,
    ) {
        Ok(messages) => {
            let json: Vec<serde_json::Value> = messages
                .iter()
                .map(|m| message_to_json(m, &st.storage))
                .collect();
            (StatusCode::OK, axum::Json(serde_json::json!(json))).into_response()
        }
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub async fn get_message_handler(
    State(state): State<SharedState>,
    Path(message_id): Path<String>,
) -> Response {
    let st = state.lock().await;
    match st.storage.get_message(&message_id) {
        Ok(Some(m)) => {
            let json = message_to_json(&m, &st.storage);
            (StatusCode::OK, axum::Json(json)).into_response()
        }
        Ok(None) => api_error(StatusCode::NOT_FOUND, "message not found"),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

// -- Send direct message --

#[derive(Deserialize)]
pub struct SendDirectRequest {
    recipient_id: String,
    body: String,
    #[serde(default)]
    attachments: Vec<SendAttachmentRef>,
}

pub async fn send_direct_handler(
    State(state): State<SharedState>,
    axum::Json(req): axum::Json<SendDirectRequest>,
) -> Response {
    if req.body.trim().is_empty() && req.attachments.is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "body or attachments required");
    }

    let now = now_secs();

    // Short lock: extract data needed for envelope construction
    let (keypair_id, signing_key, recipient_enc_key, relay_url) = {
        let st = state.lock().await;

        let peer = match st.storage.get_peer(&req.recipient_id) {
            Ok(Some(p)) => p,
            Ok(None) => return api_error(StatusCode::NOT_FOUND, "recipient peer not found"),
            Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        };

        let enc_key = match peer.encryption_public_key.as_deref() {
            Some(k) => k.to_string(),
            None => {
                return api_error(
                    StatusCode::BAD_REQUEST,
                    "recipient has no encryption key; cannot send encrypted message",
                )
            }
        };

        (
            st.keypair.id.clone(),
            st.keypair.signing_private_key_hex.clone(),
            enc_key,
            st.relay_url.clone(),
        )
    };
    // Lock released

    // Build encrypted envelope (CPU-only, no lock needed)
    let content_key = generate_content_key();
    let mut nonce = [0u8; NONCE_SIZE];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut nonce);

    let payload = match build_encrypted_payload(
        req.body.as_bytes(),
        &recipient_enc_key,
        WEB_PAYLOAD_AAD,
        WEB_HPKE_INFO,
        &content_key,
        &nonce,
        None,
    ) {
        Ok(p) => p,
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("crypto: {e}")),
    };

    let envelope = match build_envelope_from_payload(
        keypair_id.clone(),
        req.recipient_id.clone(),
        None,
        None,
        now,
        DEFAULT_TTL_SECONDS,
        MessageKind::Direct,
        None,
        payload,
        &signing_key,
    ) {
        Ok(e) => e,
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("envelope: {e}")),
    };

    let msg_id = envelope.header.message_id.0.clone();

    // Post to relay (blocking I/O, no lock held)
    let relay_delivered = if let Some(ref relay_url) = relay_url {
        match post_envelope(relay_url, &envelope) {
            Ok(()) => true,
            Err(e) => {
                crate::tlog!("failed to post direct message to relay: {}", e);
                false
            }
        }
    } else {
        false
    };

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
            recipient_id: req.recipient_id.clone(),
            message_kind: "direct".to_string(),
            group_id: None,
            body: Some(req.body.clone()),
            timestamp: now,
            received_at: now,
            ttl_seconds: DEFAULT_TTL_SECONDS,
            is_read: true,
            raw_envelope: None,
            reply_to: None,
        });

        link_attachments(&st.storage, &msg_id, &req.attachments);

        let _ = st.ws_tx.send(WsEvent::NewMessage {
            message_id: msg_id.clone(),
            sender_id: keypair_id.clone(),
            message_kind: "direct".to_string(),
            body: Some(req.body.clone()),
            timestamp: now,
        });
    }

    crate::tlog!(
        "send: direct message to {} (id={}, relay={})",
        crate::logging::peer_id(&req.recipient_id),
        crate::logging::msg_id(&msg_id),
        relay_delivered
    );

    let json = serde_json::json!({
        "message_id": msg_id,
        "status": "sent",
        "relay_delivered": relay_delivered,
    });
    (StatusCode::CREATED, axum::Json(json)).into_response()
}

// -- Send public message --

#[derive(Deserialize)]
pub struct SendPublicRequest {
    body: String,
    #[serde(default)]
    attachments: Vec<SendAttachmentRef>,
}

pub async fn send_public_handler(
    State(state): State<SharedState>,
    axum::Json(req): axum::Json<SendPublicRequest>,
) -> Response {
    if req.body.trim().is_empty() && req.attachments.is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "body or attachments required");
    }

    let now = now_secs();

    // Short lock: extract identity data
    let (keypair_id, signing_key, relay_url) = {
        let st = state.lock().await;
        (
            st.keypair.id.clone(),
            st.keypair.signing_private_key_hex.clone(),
            st.relay_url.clone(),
        )
    };
    // Lock released

    // Build envelope (CPU-only, no lock needed)
    let mut salt = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut salt);

    let envelope = match build_plaintext_envelope(
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
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("envelope: {e}")),
    };

    let msg_id = envelope.header.message_id.0.clone();

    // Post to relay (blocking I/O, no lock held)
    let relay_delivered = if let Some(ref relay_url) = relay_url {
        match post_envelope(relay_url, &envelope) {
            Ok(()) => true,
            Err(e) => {
                crate::tlog!("failed to post public message to relay: {}", e);
                false
            }
        }
    } else {
        false
    };

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
            message_kind: "public".to_string(),
            group_id: None,
            body: Some(req.body.clone()),
            timestamp: now,
            received_at: now,
            ttl_seconds: DEFAULT_TTL_SECONDS,
            is_read: true,
            raw_envelope: None,
            reply_to: None,
        });

        link_attachments(&st.storage, &msg_id, &req.attachments);

        let _ = st.ws_tx.send(WsEvent::NewMessage {
            message_id: msg_id.clone(),
            sender_id: keypair_id,
            message_kind: "public".to_string(),
            body: Some(req.body.clone()),
            timestamp: now,
        });
    }

    crate::tlog!(
        "send: public message (id={}, relay={})",
        crate::logging::msg_id(&msg_id),
        relay_delivered
    );

    let json = serde_json::json!({
        "message_id": msg_id,
        "status": "sent",
        "relay_delivered": relay_delivered,
    });
    (StatusCode::CREATED, axum::Json(json)).into_response()
}

// -- Send group message --

#[derive(Deserialize)]
pub struct SendGroupRequest {
    group_id: String,
    body: String,
    #[serde(default)]
    attachments: Vec<SendAttachmentRef>,
}

pub async fn send_group_handler(
    State(state): State<SharedState>,
    axum::Json(req): axum::Json<SendGroupRequest>,
) -> Response {
    if req.body.trim().is_empty() && req.attachments.is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "body cannot be empty");
    }

    let now = now_secs();

    // Short lock: extract group key and identity
    let (keypair_id, signing_key, group_key, relay_url) = {
        let st = state.lock().await;

        let group = match st.storage.get_group(&req.group_id) {
            Ok(Some(g)) => g,
            Ok(None) => return api_error(StatusCode::NOT_FOUND, "group not found"),
            Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        };

        let gk: [u8; 32] = match group.group_key.try_into() {
            Ok(k) => k,
            Err(_) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, "invalid group key"),
        };

        (
            st.keypair.id.clone(),
            st.keypair.signing_private_key_hex.clone(),
            gk,
            st.relay_url.clone(),
        )
    };
    // Lock released

    // Build envelope (CPU-only, no lock needed)
    let aad = req.group_id.as_bytes();
    let payload =
        match crate::protocol::build_group_message_payload(req.body.as_bytes(), &group_key, aad) {
            Ok(p) => p,
            Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("crypto: {e}")),
        };

    let envelope = match build_envelope_from_payload(
        keypair_id.clone(),
        "*".to_string(),
        None,
        None,
        now,
        DEFAULT_TTL_SECONDS,
        MessageKind::FriendGroup,
        Some(req.group_id.clone()),
        payload,
        &signing_key,
    ) {
        Ok(e) => e,
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("envelope: {e}")),
    };

    let msg_id = envelope.header.message_id.0.clone();

    // Post to relay (blocking I/O, no lock held)
    let relay_delivered = if let Some(ref relay_url) = relay_url {
        match post_envelope(relay_url, &envelope) {
            Ok(()) => true,
            Err(e) => {
                crate::tlog!("failed to post group message to relay: {}", e);
                false
            }
        }
    } else {
        false
    };

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
            message_kind: "friend_group".to_string(),
            group_id: Some(req.group_id.clone()),
            body: Some(req.body.clone()),
            timestamp: now,
            received_at: now,
            ttl_seconds: DEFAULT_TTL_SECONDS,
            is_read: true,
            raw_envelope: None,
            reply_to: None,
        });

        link_attachments(&st.storage, &msg_id, &req.attachments);

        let _ = st.ws_tx.send(WsEvent::NewMessage {
            message_id: msg_id.clone(),
            sender_id: keypair_id,
            message_kind: "friend_group".to_string(),
            body: Some(req.body.clone()),
            timestamp: now,
        });
    }

    crate::tlog!(
        "send: group message to {} (id={}, relay={})",
        req.group_id,
        crate::logging::msg_id(&msg_id),
        relay_delivered
    );

    let json = serde_json::json!({
        "message_id": msg_id,
        "status": "sent",
        "relay_delivered": relay_delivered,
    });
    (StatusCode::CREATED, axum::Json(json)).into_response()
}

// -- Mark message as read --

pub async fn mark_read_handler(
    State(state): State<SharedState>,
    Path(message_id): Path<String>,
) -> Response {
    let st = state.lock().await;
    match st.storage.mark_message_read(&message_id) {
        Ok(true) => {
            let _ = st.ws_tx.send(WsEvent::MessageRead {
                message_id: message_id.clone(),
            });
            (
                StatusCode::OK,
                axum::Json(serde_json::json!({"status": "ok"})),
            )
                .into_response()
        }
        Ok(false) => api_error(StatusCode::NOT_FOUND, "message not found"),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}
