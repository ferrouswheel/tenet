//! Group management handlers.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::protocol::{build_envelope_from_payload, build_meta_payload, MessageKind, MetaMessage};
use crate::relay_transport::post_envelope;
use crate::web_client::config::DEFAULT_TTL_SECONDS;
use crate::web_client::state::SharedState;
use crate::web_client::utils::{api_error, now_secs};

pub async fn list_groups_handler(State(state): State<SharedState>) -> Response {
    let st = state.lock().await;
    match st.storage.list_groups() {
        Ok(groups) => {
            let json: Vec<serde_json::Value> = groups
                .iter()
                .map(|g| {
                    serde_json::json!({
                        "group_id": g.group_id,
                        "creator_id": g.creator_id,
                        "created_at": g.created_at,
                        "key_version": g.key_version,
                    })
                })
                .collect();
            (StatusCode::OK, axum::Json(serde_json::json!(json))).into_response()
        }
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub async fn get_group_handler(
    State(state): State<SharedState>,
    Path(group_id): Path<String>,
) -> Response {
    let st = state.lock().await;
    match st.storage.get_group(&group_id) {
        Ok(Some(g)) => {
            // Get accepted group members.
            let members = st.storage.list_group_members(&group_id).unwrap_or_default();
            let member_ids: Vec<String> = members.iter().map(|m| m.peer_id.clone()).collect();

            // Get pending invitees (outgoing invites that haven't been accepted yet).
            let pending_invites = st
                .storage
                .list_group_invites(Some("pending"), Some("outgoing"))
                .unwrap_or_default();
            let pending_ids: Vec<String> = pending_invites
                .iter()
                .filter(|inv| inv.group_id == group_id)
                .map(|inv| inv.to_peer_id.clone())
                .collect();

            let json = serde_json::json!({
                "group_id": g.group_id,
                "creator_id": g.creator_id,
                "created_at": g.created_at,
                "key_version": g.key_version,
                "members": member_ids,
                "pending_invites": pending_ids,
            });
            (StatusCode::OK, axum::Json(json)).into_response()
        }
        Ok(None) => api_error(StatusCode::NOT_FOUND, "group not found"),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize)]
pub struct CreateGroupRequest {
    group_id: String,
    member_ids: Vec<String>,
    #[serde(default)]
    message: Option<String>,
}

pub async fn create_group_handler(
    State(state): State<SharedState>,
    axum::Json(req): axum::Json<CreateGroupRequest>,
) -> Response {
    if req.group_id.trim().is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "group_id cannot be empty");
    }

    let now = now_secs();

    // Generate a new symmetric key for the group.
    let mut group_key = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut group_key);

    // Short lock: insert group (creator only as active member), record outgoing invites.
    let (keypair_id, keypair_signing_key, relay_url, invitee_ids, group_name) = {
        let st = state.lock().await;

        let group_row = crate::storage::GroupRow {
            group_id: req.group_id.clone(),
            group_key: group_key.to_vec(),
            creator_id: st.keypair.id.clone(),
            created_at: now,
            key_version: 1,
        };

        // Creator is the only active member initially.
        let creator_member = crate::storage::GroupMemberRow {
            group_id: req.group_id.clone(),
            peer_id: st.keypair.id.clone(),
            joined_at: now,
        };

        if let Err(e) = st
            .storage
            .insert_group_with_members(&group_row, &[creator_member])
        {
            return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
        }

        // Record an outgoing invite for each requested member (skip self).
        let mut invitee_ids: Vec<String> = Vec::new();
        for member_id in &req.member_ids {
            if member_id == &st.keypair.id {
                continue;
            }
            let invite_row = crate::storage::GroupInviteRow {
                id: 0,
                group_id: req.group_id.clone(),
                from_peer_id: st.keypair.id.clone(),
                to_peer_id: member_id.clone(),
                status: "pending".to_string(),
                message: req.message.clone(),
                direction: "outgoing".to_string(),
                created_at: now,
                updated_at: now,
            };
            match st.storage.insert_group_invite(&invite_row) {
                Ok(_) => invitee_ids.push(member_id.clone()),
                Err(e) => {
                    crate::tlog!(
                        "create_group: failed to record invite for {}: {}",
                        crate::logging::peer_id(member_id),
                        e
                    );
                }
            }
        }

        (
            st.keypair.id.clone(),
            st.keypair.signing_private_key_hex.clone(),
            st.relay_url.clone(),
            invitee_ids,
            req.group_id.clone(),
        )
    };
    // Lock released.

    // Send a GroupInvite meta message to each invitee (I/O, no lock held).
    let mut invite_failed: Vec<String> = Vec::new();

    if let Some(ref relay) = relay_url {
        for invitee_id in &invitee_ids {
            let meta_msg = MetaMessage::GroupInvite {
                peer_id: keypair_id.clone(),
                group_id: req.group_id.clone(),
                group_name: Some(group_name.clone()),
                message: req.message.clone(),
            };

            match build_meta_payload(&meta_msg) {
                Ok(payload) => {
                    match build_envelope_from_payload(
                        keypair_id.clone(),
                        invitee_id.clone(),
                        None,
                        None,
                        now,
                        DEFAULT_TTL_SECONDS,
                        MessageKind::Meta,
                        None,
                        None,
                        payload,
                        &keypair_signing_key,
                    ) {
                        Ok(envelope) => {
                            if let Err(e) = post_envelope(relay, &envelope) {
                                crate::tlog!(
                                    "create_group: failed to send invite to {}: {}",
                                    crate::logging::peer_id(invitee_id),
                                    e
                                );
                                invite_failed.push(invitee_id.clone());
                            } else {
                                crate::tlog!(
                                    "create_group: sent group invite for {} to {}",
                                    req.group_id,
                                    crate::logging::peer_id(invitee_id)
                                );
                            }
                        }
                        Err(e) => {
                            crate::tlog!(
                                "create_group: failed to build invite envelope for {}: {}",
                                crate::logging::peer_id(invitee_id),
                                e
                            );
                            invite_failed.push(invitee_id.clone());
                        }
                    }
                }
                Err(e) => {
                    crate::tlog!(
                        "create_group: failed to build invite payload for {}: {}",
                        crate::logging::peer_id(invitee_id),
                        e
                    );
                    invite_failed.push(invitee_id.clone());
                }
            }
        }
    } else if !invitee_ids.is_empty() {
        crate::tlog!("create_group: no relay configured; invites stored locally only");
    }

    crate::tlog!(
        "send: created group {} (invites_sent={}/{})",
        req.group_id,
        invitee_ids.len() - invite_failed.len(),
        invitee_ids.len()
    );

    let mut json = serde_json::json!({
        "group_id": req.group_id,
        "creator_id": keypair_id,
        "created_at": now,
        "key_version": 1,
        "members": [keypair_id],
        "pending_invites": invitee_ids,
    });

    if !invite_failed.is_empty() {
        json.as_object_mut().unwrap().insert(
            "invite_failed".to_string(),
            serde_json::json!(invite_failed),
        );
    }

    (StatusCode::CREATED, axum::Json(json)).into_response()
}

#[derive(Deserialize)]
pub struct AddGroupMemberRequest {
    peer_id: String,
    #[serde(default)]
    message: Option<String>,
}

pub async fn add_group_member_handler(
    State(state): State<SharedState>,
    Path(group_id): Path<String>,
    axum::Json(req): axum::Json<AddGroupMemberRequest>,
) -> Response {
    if req.peer_id.trim().is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "peer_id cannot be empty");
    }

    let now = now_secs();

    // Short lock: validate group exists, record outgoing invite.
    let (keypair_id, keypair_signing_key, relay_url) = {
        let st = state.lock().await;

        if st.storage.get_group(&group_id).ok().flatten().is_none() {
            return api_error(StatusCode::NOT_FOUND, "group not found");
        }

        // Check peer exists.
        if st
            .storage
            .get_peer(&req.peer_id)
            .ok()
            .flatten()
            .is_none()
        {
            return api_error(StatusCode::NOT_FOUND, "peer not found");
        }

        let invite_row = crate::storage::GroupInviteRow {
            id: 0,
            group_id: group_id.clone(),
            from_peer_id: st.keypair.id.clone(),
            to_peer_id: req.peer_id.clone(),
            status: "pending".to_string(),
            message: req.message.clone(),
            direction: "outgoing".to_string(),
            created_at: now,
            updated_at: now,
        };
        if let Err(e) = st.storage.insert_group_invite(&invite_row) {
            return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
        }

        (
            st.keypair.id.clone(),
            st.keypair.signing_private_key_hex.clone(),
            st.relay_url.clone(),
        )
    };
    // Lock released.

    // Send the GroupInvite meta message (I/O, no lock held).
    let relay_sent = if let Some(ref relay) = relay_url {
        let meta_msg = MetaMessage::GroupInvite {
            peer_id: keypair_id.clone(),
            group_id: group_id.clone(),
            group_name: Some(group_id.clone()),
            message: req.message.clone(),
        };

        match build_meta_payload(&meta_msg) {
            Ok(payload) => match build_envelope_from_payload(
                keypair_id.clone(),
                req.peer_id.clone(),
                None,
                None,
                now,
                DEFAULT_TTL_SECONDS,
                MessageKind::Meta,
                None,
                None,
                payload,
                &keypair_signing_key,
            ) {
                Ok(envelope) => match post_envelope(relay, &envelope) {
                    Ok(()) => {
                        crate::tlog!(
                            "send: group invite for {} to {} via relay",
                            group_id,
                            crate::logging::peer_id(&req.peer_id)
                        );
                        true
                    }
                    Err(e) => {
                        crate::tlog!(
                            "add_group_member: failed to post invite to relay for {}: {}",
                            crate::logging::peer_id(&req.peer_id),
                            e
                        );
                        false
                    }
                },
                Err(e) => {
                    crate::tlog!(
                        "add_group_member: failed to build envelope for {}: {}",
                        crate::logging::peer_id(&req.peer_id),
                        e
                    );
                    false
                }
            },
            Err(e) => {
                crate::tlog!(
                    "add_group_member: failed to build payload for {}: {}",
                    crate::logging::peer_id(&req.peer_id),
                    e
                );
                false
            }
        }
    } else {
        crate::tlog!("add_group_member: no relay configured; invite stored locally only");
        false
    };

    let json = serde_json::json!({
        "status": "invited",
        "group_id": group_id,
        "peer_id": req.peer_id,
        "invite_sent": relay_sent,
    });
    (StatusCode::OK, axum::Json(json)).into_response()
}

pub async fn remove_group_member_handler(
    State(state): State<SharedState>,
    Path((group_id, peer_id)): Path<(String, String)>,
) -> Response {
    let st = state.lock().await;

    match st.storage.remove_group_member(&group_id, &peer_id) {
        Ok(true) => {
            // TODO: Implement key rotation for remaining members
            let json = serde_json::json!({
                "status": "removed",
                "group_id": group_id,
                "peer_id": peer_id,
            });
            (StatusCode::OK, axum::Json(json)).into_response()
        }
        Ok(false) => api_error(StatusCode::NOT_FOUND, "group member not found"),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub async fn leave_group_handler(
    State(state): State<SharedState>,
    Path(group_id): Path<String>,
) -> Response {
    let st = state.lock().await;

    match st.storage.remove_group_member(&group_id, &st.keypair.id) {
        Ok(true) => {
            // TODO: Implement key rotation for remaining members
            let json = serde_json::json!({
                "status": "left",
                "group_id": group_id,
            });
            (StatusCode::OK, axum::Json(json)).into_response()
        }
        Ok(false) => api_error(StatusCode::NOT_FOUND, "not a member of this group"),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}
