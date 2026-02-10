//! Group management handlers.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::crypto::{generate_content_key, NONCE_SIZE};
use crate::protocol::{build_encrypted_payload, build_envelope_from_payload, MessageKind};
use crate::relay_transport::post_envelope;
use crate::web_client::config::{DEFAULT_TTL_SECONDS, WEB_HPKE_INFO, WEB_PAYLOAD_AAD};
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
            // Get group members
            let members = st.storage.list_group_members(&group_id).unwrap_or_default();
            let member_ids: Vec<String> = members.iter().map(|m| m.peer_id.clone()).collect();

            let json = serde_json::json!({
                "group_id": g.group_id,
                "creator_id": g.creator_id,
                "created_at": g.created_at,
                "key_version": g.key_version,
                "members": member_ids,
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
}

pub async fn create_group_handler(
    State(state): State<SharedState>,
    axum::Json(req): axum::Json<CreateGroupRequest>,
) -> Response {
    if req.group_id.trim().is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "group_id cannot be empty");
    }

    let now = now_secs();

    // Generate a new symmetric key for the group
    let mut group_key = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut group_key);

    // Short lock: insert group + members atomically, extract peer data for key distribution
    let (keypair_id, signing_key, relay_url, all_members, member_enc_keys) = {
        let st = state.lock().await;

        let group_row = crate::storage::GroupRow {
            group_id: req.group_id.clone(),
            group_key: group_key.to_vec(),
            creator_id: st.keypair.id.clone(),
            created_at: now,
            key_version: 1,
        };

        let mut all_members = req.member_ids.clone();
        if !all_members.contains(&st.keypair.id) {
            all_members.push(st.keypair.id.clone());
        }

        let member_rows: Vec<crate::storage::GroupMemberRow> = all_members
            .iter()
            .map(|member_id| crate::storage::GroupMemberRow {
                group_id: req.group_id.clone(),
                peer_id: member_id.clone(),
                joined_at: now,
            })
            .collect();

        if let Err(e) = st
            .storage
            .insert_group_with_members(&group_row, &member_rows)
        {
            return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
        }

        // Collect encryption keys for members we need to distribute to
        let mut member_enc_keys: Vec<(String, Option<String>)> = Vec::new();
        for member_id in &req.member_ids {
            if member_id == &st.keypair.id {
                continue;
            }
            let enc_key = st
                .storage
                .get_peer(member_id)
                .ok()
                .flatten()
                .and_then(|p| p.encryption_public_key);
            member_enc_keys.push((member_id.clone(), enc_key));
        }

        (
            st.keypair.id.clone(),
            st.keypair.signing_private_key_hex.clone(),
            st.relay_url.clone(),
            all_members,
            member_enc_keys,
        )
    };
    // Lock released

    // Distribute group key to each member (crypto + I/O, no lock held)
    let mut key_distribution_failed: Vec<String> = Vec::new();

    for (member_id, enc_key) in &member_enc_keys {
        let recipient_enc_key = match enc_key {
            Some(k) => k.clone(),
            None => {
                crate::tlog!(
                    "peer {} not found or has no encryption key; skipping",
                    crate::logging::peer_id(member_id)
                );
                key_distribution_failed.push(member_id.clone());
                continue;
            }
        };

        let key_distribution = serde_json::json!({
            "type": "group_key_distribution",
            "group_id": req.group_id,
            "group_key": hex::encode(group_key),
            "key_version": 1,
            "creator_id": keypair_id,
        });

        let key_dist_bytes = serde_json::to_vec(&key_distribution).unwrap_or_default();

        let content_key = generate_content_key();
        let mut nonce = [0u8; NONCE_SIZE];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut nonce);

        let payload = match build_encrypted_payload(
            &key_dist_bytes,
            &recipient_enc_key,
            WEB_PAYLOAD_AAD,
            WEB_HPKE_INFO,
            &content_key,
            &nonce,
            None,
        ) {
            Ok(p) => p,
            Err(e) => {
                crate::tlog!(
                    "failed to encrypt key distribution for {}: {}",
                    crate::logging::peer_id(member_id),
                    e
                );
                key_distribution_failed.push(member_id.clone());
                continue;
            }
        };

        let envelope = match build_envelope_from_payload(
            keypair_id.clone(),
            member_id.clone(),
            None,
            None,
            now,
            DEFAULT_TTL_SECONDS,
            MessageKind::Direct,
            None,
            None,
            payload,
            &signing_key,
        ) {
            Ok(e) => e,
            Err(e) => {
                crate::tlog!(
                    "failed to build envelope for {}: {}",
                    crate::logging::peer_id(member_id),
                    e
                );
                key_distribution_failed.push(member_id.clone());
                continue;
            }
        };

        if let Some(ref relay_url) = relay_url {
            if let Err(e) = post_envelope(relay_url, &envelope) {
                crate::tlog!(
                    "failed to distribute group key to {}: {}",
                    crate::logging::peer_id(member_id),
                    e
                );
                key_distribution_failed.push(member_id.clone());
            }
        } else {
            key_distribution_failed.push(member_id.clone());
        }
    }

    let mut json = serde_json::json!({
        "group_id": req.group_id,
        "creator_id": keypair_id,
        "created_at": now,
        "key_version": 1,
        "members": all_members,
    });

    if !key_distribution_failed.is_empty() {
        json.as_object_mut().unwrap().insert(
            "key_distribution_failed".to_string(),
            serde_json::json!(key_distribution_failed),
        );
    }

    let keys_sent = member_enc_keys.len() - key_distribution_failed.len();
    crate::tlog!(
        "send: created group {} with {} members (keys_distributed={}/{})",
        req.group_id,
        all_members.len(),
        keys_sent,
        member_enc_keys.len()
    );

    (StatusCode::CREATED, axum::Json(json)).into_response()
}

#[derive(Deserialize)]
pub struct AddGroupMemberRequest {
    peer_id: String,
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

    // Short lock: validate, insert member, extract data for key distribution
    let (keypair_id, signing_key, relay_url, recipient_enc_key, group_key, key_version, creator_id) = {
        let st = state.lock().await;

        let group = match st.storage.get_group(&group_id) {
            Ok(Some(g)) => g,
            Ok(None) => return api_error(StatusCode::NOT_FOUND, "group not found"),
            Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        };

        let member_row = crate::storage::GroupMemberRow {
            group_id: group_id.clone(),
            peer_id: req.peer_id.clone(),
            joined_at: now,
        };

        if let Err(e) = st.storage.insert_group_member(&member_row) {
            return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
        }

        let peer = match st.storage.get_peer(&req.peer_id) {
            Ok(Some(p)) => p,
            Ok(None) => return api_error(StatusCode::NOT_FOUND, "peer not found"),
            Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        };

        let enc_key = match peer.encryption_public_key.as_deref() {
            Some(k) => k.to_string(),
            None => {
                return api_error(
                    StatusCode::BAD_REQUEST,
                    "peer has no encryption key; cannot distribute group key",
                )
            }
        };

        let gk: [u8; 32] = match group.group_key.try_into() {
            Ok(k) => k,
            Err(_) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, "invalid group key"),
        };

        (
            st.keypair.id.clone(),
            st.keypair.signing_private_key_hex.clone(),
            st.relay_url.clone(),
            enc_key,
            gk,
            group.key_version,
            group.creator_id.clone(),
        )
    };
    // Lock released

    // Build key distribution envelope (crypto, no lock needed)
    let key_distribution = serde_json::json!({
        "type": "group_key_distribution",
        "group_id": group_id,
        "group_key": hex::encode(group_key),
        "key_version": key_version,
        "creator_id": creator_id,
    });

    let key_dist_bytes = serde_json::to_vec(&key_distribution).unwrap_or_default();

    let content_key = generate_content_key();
    let mut nonce = [0u8; NONCE_SIZE];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut nonce);

    let payload = match build_encrypted_payload(
        &key_dist_bytes,
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
        keypair_id,
        req.peer_id.clone(),
        None,
        None,
        now,
        DEFAULT_TTL_SECONDS,
        MessageKind::Direct,
        None,
        None,
        payload,
        &signing_key,
    ) {
        Ok(e) => e,
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("envelope: {e}")),
    };

    // Post to relay (blocking I/O, no lock held)
    let relay_delivered = if let Some(ref relay_url) = relay_url {
        match post_envelope(relay_url, &envelope) {
            Ok(()) => true,
            Err(e) => {
                crate::tlog!(
                    "failed to distribute group key to {}: {}",
                    crate::logging::peer_id(&req.peer_id),
                    e
                );
                false
            }
        }
    } else {
        false
    };

    crate::tlog!(
        "send: group key to {} for {} (relay={})",
        crate::logging::peer_id(&req.peer_id),
        group_id,
        relay_delivered
    );

    let json = serde_json::json!({
        "status": "added",
        "group_id": group_id,
        "peer_id": req.peer_id,
        "key_delivered": relay_delivered,
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
