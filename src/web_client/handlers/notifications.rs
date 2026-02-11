//! Notification handlers (Phase 11).

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::web_client::state::SharedState;
use crate::web_client::utils::api_error;

#[derive(Deserialize)]
pub struct ListNotificationsQuery {
    unread: Option<bool>,
    limit: Option<u32>,
}

/// GET /api/notifications - List notifications with optional filters.
pub async fn list_notifications_handler(
    State(state): State<SharedState>,
    Query(params): Query<ListNotificationsQuery>,
) -> Response {
    let st = state.lock().await;
    let unread_only = params.unread.unwrap_or(false);
    let limit = params.limit.unwrap_or(50).min(200);

    match st.storage.list_notifications(unread_only, limit) {
        Ok(notifications) => {
            let json: Vec<serde_json::Value> = notifications
                .iter()
                .map(|n| {
                    serde_json::json!({
                        "id": n.id,
                        "type": n.notification_type,
                        "message_id": n.message_id,
                        "sender_id": n.sender_id,
                        "created_at": n.created_at,
                        "seen": n.seen,
                        "read": n.read,
                    })
                })
                .collect();
            (StatusCode::OK, axum::Json(serde_json::json!(json))).into_response()
        }
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// GET /api/notifications/count - Get unseen notification count.
pub async fn count_notifications_handler(State(state): State<SharedState>) -> Response {
    let st = state.lock().await;

    match st.storage.count_unseen_notifications() {
        Ok(count) => {
            let json = serde_json::json!({
                "unseen": count,
            });
            (StatusCode::OK, axum::Json(json)).into_response()
        }
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// POST /api/notifications/:id/read - Mark a notification as read.
pub async fn mark_read_handler(State(state): State<SharedState>, Path(id): Path<i64>) -> Response {
    let st = state.lock().await;

    match st.storage.mark_notification_read(id) {
        Ok(true) => {
            let json = serde_json::json!({
                "status": "ok",
                "id": id,
            });
            (StatusCode::OK, axum::Json(json)).into_response()
        }
        Ok(false) => api_error(StatusCode::NOT_FOUND, "notification not found"),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// POST /api/notifications/seen-all - Mark all notifications as seen.
pub async fn mark_all_seen_handler(State(state): State<SharedState>) -> Response {
    let st = state.lock().await;

    match st.storage.mark_all_notifications_seen() {
        Ok(count) => {
            let json = serde_json::json!({
                "status": "ok",
                "marked_seen": count,
            });
            (StatusCode::OK, axum::Json(json)).into_response()
        }
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// POST /api/notifications/read-all - Mark all notifications as read.
pub async fn mark_all_read_handler(State(state): State<SharedState>) -> Response {
    let st = state.lock().await;

    match st.storage.mark_all_notifications_read() {
        Ok(count) => {
            let json = serde_json::json!({
                "status": "ok",
                "marked_read": count,
            });
            (StatusCode::OK, axum::Json(json)).into_response()
        }
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}
