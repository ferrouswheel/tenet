//! Conversation listing handlers.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::web_client::state::SharedState;
use crate::web_client::utils::{api_error, message_to_json};

pub async fn list_conversations_handler(State(state): State<SharedState>) -> Response {
    let st = state.lock().await;
    match st.storage.list_conversations(&st.keypair.id) {
        Ok(conversations) => {
            // Enrich with peer display names
            let json: Vec<serde_json::Value> = conversations
                .iter()
                .map(|c| {
                    let peer = st.storage.get_peer(&c.peer_id).ok().flatten();
                    serde_json::json!({
                        "peer_id": c.peer_id,
                        "display_name": peer.as_ref().and_then(|p| p.display_name.clone()),
                        "last_timestamp": c.last_timestamp,
                        "last_message": c.last_message,
                        "unread_count": c.unread_count,
                    })
                })
                .collect();
            (StatusCode::OK, axum::Json(serde_json::json!(json))).into_response()
        }
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize)]
pub struct ConversationQuery {
    before: Option<u64>,
    limit: Option<u32>,
}

pub async fn get_conversation_handler(
    State(state): State<SharedState>,
    Path(peer_id): Path<String>,
    Query(params): Query<ConversationQuery>,
) -> Response {
    let st = state.lock().await;
    let limit = params.limit.unwrap_or(50).min(200);

    match st
        .storage
        .list_conversation_messages(&st.keypair.id, &peer_id, params.before, limit)
    {
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
