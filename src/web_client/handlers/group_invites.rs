//! Group invite handlers.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::protocol::{build_envelope_from_payload, build_meta_payload, MessageKind, MetaMessage};
use crate::relay_transport::post_envelope;
use crate::web_client::config::DEFAULT_TTL_SECONDS;
use crate::web_client::state::SharedState;
use crate::web_client::utils::{api_error, now_secs};

#[derive(Deserialize)]
pub struct ListGroupInvitesQuery {
    status: Option<String>,
    direction: Option<String>,
}

pub async fn list_group_invites_handler(
    State(state): State<SharedState>,
    Query(query): Query<ListGroupInvitesQuery>,
) -> Response {
    let st = state.lock().await;
    match st
        .storage
        .list_group_invites(query.status.as_deref(), query.direction.as_deref())
    {
        Ok(invites) => {
            let json: Vec<serde_json::Value> = invites
                .iter()
                .map(|inv| {
                    serde_json::json!({
                        "id": inv.id,
                        "group_id": inv.group_id,
                        "from_peer_id": inv.from_peer_id,
                        "to_peer_id": inv.to_peer_id,
                        "status": inv.status,
                        "message": inv.message,
                        "direction": inv.direction,
                        "created_at": inv.created_at,
                        "updated_at": inv.updated_at,
                    })
                })
                .collect();
            (StatusCode::OK, axum::Json(serde_json::json!(json))).into_response()
        }
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub async fn accept_group_invite_handler(
    State(state): State<SharedState>,
    Path(id): Path<i64>,
) -> Response {
    let now = now_secs();

    // Short lock: fetch invite, update status.
    let (keypair, relay_url, invite) = {
        let st = state.lock().await;

        let invite = match st.storage.get_group_invite(id) {
            Ok(Some(inv)) => inv,
            Ok(None) => return api_error(StatusCode::NOT_FOUND, "group invite not found"),
            Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        };

        if invite.status != "pending" {
            return api_error(
                StatusCode::BAD_REQUEST,
                format!("group invite is already {}", invite.status),
            );
        }

        if invite.direction != "incoming" {
            return api_error(
                StatusCode::BAD_REQUEST,
                "can only accept incoming group invites",
            );
        }

        if let Err(e) = st.storage.update_group_invite_status(id, "accepted") {
            return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
        }

        (st.keypair.clone(), st.relay_url.clone(), invite)
    };
    // Lock released.

    // Send the GroupInviteAccept meta message (I/O, no lock held).
    if let Some(ref relay) = relay_url {
        let meta_msg = MetaMessage::GroupInviteAccept {
            peer_id: keypair.id.clone(),
            group_id: invite.group_id.clone(),
        };

        match build_meta_payload(&meta_msg) {
            Ok(payload) => {
                match build_envelope_from_payload(
                    keypair.id.clone(),
                    invite.from_peer_id.clone(),
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
                    Ok(envelope) => {
                        if let Err(e) = post_envelope(relay, &envelope) {
                            crate::tlog!(
                                "accept_group_invite: failed to send accept to {}: {}",
                                crate::logging::peer_id(&invite.from_peer_id),
                                e
                            );
                        } else {
                            crate::tlog!(
                                "accept_group_invite: sent accept for {} to {}",
                                invite.group_id,
                                crate::logging::peer_id(&invite.from_peer_id)
                            );
                        }
                    }
                    Err(e) => {
                        crate::tlog!(
                            "accept_group_invite: failed to build envelope for {}: {}",
                            crate::logging::peer_id(&invite.from_peer_id),
                            e
                        );
                    }
                }
            }
            Err(e) => {
                crate::tlog!(
                    "accept_group_invite: failed to build payload for {}: {}",
                    crate::logging::peer_id(&invite.from_peer_id),
                    e
                );
            }
        }
    } else {
        crate::tlog!(
            "accept_group_invite: no relay configured; acceptance stored locally only"
        );
    }

    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "status": "accepted",
            "id": id,
            "group_id": invite.group_id,
        })),
    )
        .into_response()
}

pub async fn ignore_group_invite_handler(
    State(state): State<SharedState>,
    Path(id): Path<i64>,
) -> Response {
    let st = state.lock().await;

    let invite = match st.storage.get_group_invite(id) {
        Ok(Some(inv)) => inv,
        Ok(None) => return api_error(StatusCode::NOT_FOUND, "group invite not found"),
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };

    if invite.status != "pending" {
        return api_error(
            StatusCode::BAD_REQUEST,
            format!("group invite is already {}", invite.status),
        );
    }

    if invite.direction != "incoming" {
        return api_error(
            StatusCode::BAD_REQUEST,
            "can only ignore incoming group invites",
        );
    }

    if let Err(e) = st.storage.update_group_invite_status(id, "ignored") {
        return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }

    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "status": "ignored",
            "id": id,
            "group_id": invite.group_id,
        })),
    )
        .into_response()
}
