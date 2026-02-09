//! Shared utility functions for the web client.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::storage::{MessageAttachmentRow, MessageRow, Storage};

/// Attachment reference included when sending a message.
#[derive(serde::Deserialize)]
pub struct SendAttachmentRef {
    pub content_hash: String,
    pub filename: Option<String>,
}

/// Build a standard JSON error response.
pub fn api_error(status: StatusCode, message: impl Into<String>) -> Response {
    let body = serde_json::json!({ "error": message.into() });
    (status, axum::Json(body)).into_response()
}

/// Build the JSON representation of a message including its attachments,
/// reaction counts, and reply count.
pub fn message_to_json(m: &MessageRow, storage: &Storage) -> serde_json::Value {
    let attachments = storage
        .list_message_attachments(&m.message_id)
        .unwrap_or_default();
    let att_json: Vec<serde_json::Value> = attachments
        .iter()
        .map(|a| {
            serde_json::json!({
                "content_hash": a.content_hash,
                "filename": a.filename,
                "position": a.position,
            })
        })
        .collect();

    let (upvotes, downvotes) = storage.count_reactions(&m.message_id).unwrap_or((0, 0));
    let reply_count = storage.count_replies(&m.message_id).unwrap_or(0);

    serde_json::json!({
        "message_id": m.message_id,
        "sender_id": m.sender_id,
        "recipient_id": m.recipient_id,
        "message_kind": m.message_kind,
        "group_id": m.group_id,
        "body": m.body,
        "timestamp": m.timestamp,
        "received_at": m.received_at,
        "is_read": m.is_read,
        "attachments": att_json,
        "reply_to": m.reply_to,
        "upvotes": upvotes,
        "downvotes": downvotes,
        "reply_count": reply_count,
    })
}

/// Link uploaded attachments to a message by inserting into message_attachments.
pub fn link_attachments(storage: &Storage, message_id: &str, attachments: &[SendAttachmentRef]) {
    for (i, att) in attachments.iter().enumerate() {
        let _ = storage.insert_message_attachment(&MessageAttachmentRow {
            message_id: message_id.to_string(),
            content_hash: att.content_hash.clone(),
            filename: att.filename.clone(),
            position: i as u32,
        });
    }
}

/// Current time as seconds since UNIX epoch.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
