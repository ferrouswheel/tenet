//! Profile management handlers.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use serde::Deserialize;

use crate::crypto::{generate_content_key, NONCE_SIZE};
use crate::protocol::{
    build_encrypted_payload, build_envelope_from_payload, build_plaintext_envelope, MessageKind,
};
use crate::relay_transport::post_envelope;
use crate::storage::ProfileRow;
use crate::web_client::config::{DEFAULT_TTL_SECONDS, WEB_HPKE_INFO, WEB_PAYLOAD_AAD};
use crate::web_client::state::SharedState;
use crate::web_client::utils::{api_error, now_secs};

pub async fn get_own_profile_handler(State(state): State<SharedState>) -> Response {
    let st = state.lock().await;
    match st.storage.get_profile(&st.keypair.id) {
        Ok(Some(profile)) => {
            let public_fields: serde_json::Value =
                serde_json::from_str(&profile.public_fields).unwrap_or(serde_json::json!({}));
            let friends_fields: serde_json::Value =
                serde_json::from_str(&profile.friends_fields).unwrap_or(serde_json::json!({}));

            let json = serde_json::json!({
                "user_id": profile.user_id,
                "display_name": profile.display_name,
                "bio": profile.bio,
                "avatar_hash": profile.avatar_hash,
                "public_fields": public_fields,
                "friends_fields": friends_fields,
                "updated_at": profile.updated_at,
            });
            (StatusCode::OK, axum::Json(json)).into_response()
        }
        Ok(None) => {
            // Return empty profile
            let json = serde_json::json!({
                "user_id": st.keypair.id,
                "display_name": null,
                "bio": null,
                "avatar_hash": null,
                "public_fields": {},
                "friends_fields": {},
                "updated_at": 0,
            });
            (StatusCode::OK, axum::Json(json)).into_response()
        }
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize)]
pub struct UpdateProfileRequest {
    display_name: Option<String>,
    bio: Option<String>,
    avatar_hash: Option<String>,
    #[serde(default = "default_empty_json")]
    public_fields: serde_json::Value,
    #[serde(default = "default_empty_json")]
    friends_fields: serde_json::Value,
}

fn default_empty_json() -> serde_json::Value {
    serde_json::json!({})
}

pub async fn update_own_profile_handler(
    State(state): State<SharedState>,
    axum::Json(req): axum::Json<UpdateProfileRequest>,
) -> Response {
    let now = now_secs();

    // Short lock: persist profile and extract data for broadcasting
    let (keypair_id, signing_key, relay_url, friend_enc_keys, avatar_inline) = {
        let st = state.lock().await;

        let profile = ProfileRow {
            user_id: st.keypair.id.clone(),
            display_name: req.display_name.clone(),
            bio: req.bio.clone(),
            avatar_hash: req.avatar_hash.clone(),
            public_fields: serde_json::to_string(&req.public_fields)
                .unwrap_or_else(|_| "{}".to_string()),
            friends_fields: serde_json::to_string(&req.friends_fields)
                .unwrap_or_else(|_| "{}".to_string()),
            updated_at: now,
        };

        if let Err(e) = st.storage.upsert_profile(&profile) {
            return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
        }

        // Collect friend encryption keys for profile distribution
        let friends: Vec<(String, String)> = st
            .storage
            .list_peers()
            .unwrap_or_default()
            .iter()
            .filter(|p| p.is_friend)
            .filter_map(|p| {
                p.encryption_public_key
                    .as_ref()
                    .map(|k| (p.peer_id.clone(), k.clone()))
            })
            .collect();

        // Fetch avatar attachment bytes so recipients can store them locally.
        // The relay allows up to 5 MB per message; profile photos from phones are
        // typically 1-3 MB so no size filter is applied here.
        let avatar_inline: Option<(String, String)> =
            req.avatar_hash.as_ref().and_then(|hash| {
                st.storage
                    .get_attachment(hash)
                    .ok()
                    .flatten()
                    .map(|att| {
                        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
                            .encode(&att.data);
                        (b64, att.content_type)
                    })
            });

        (
            st.keypair.id.clone(),
            st.keypair.signing_private_key_hex.clone(),
            st.relay_url.clone(),
            friends,
            avatar_inline,
        )
    };
    // Lock released

    // Build and send public profile (crypto + I/O, no lock held)
    let public_profile = serde_json::json!({
        "type": "tenet.profile",
        "user_id": keypair_id,
        "display_name": req.display_name,
        "bio": req.bio,
        "avatar_hash": req.avatar_hash,
        "avatar_data": avatar_inline.as_ref().map(|(d, _)| d),
        "avatar_content_type": avatar_inline.as_ref().map(|(_, ct)| ct),
        "public_fields": req.public_fields,
        "updated_at": now,
    });

    let body_str = serde_json::to_string(&public_profile).unwrap_or_default();
    let mut salt = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut salt);

    let public_profile_delivered = if let Ok(envelope) = build_plaintext_envelope(
        &keypair_id,
        "*",
        None,
        None,
        now,
        DEFAULT_TTL_SECONDS,
        MessageKind::Public,
        None,
        None,
        &body_str,
        salt,
        &signing_key,
    ) {
        if let Some(ref relay_url) = relay_url {
            match post_envelope(relay_url, &envelope) {
                Ok(()) => true,
                Err(e) => {
                    crate::tlog!("failed to post public profile to relay: {}", e);
                    false
                }
            }
        } else {
            false
        }
    } else {
        false
    };

    // Send friends-only profile to each friend (crypto + I/O, no lock held)
    let friends_profile = serde_json::json!({
        "type": "tenet.profile",
        "user_id": keypair_id,
        "display_name": req.display_name,
        "bio": req.bio,
        "avatar_hash": req.avatar_hash,
        "avatar_data": avatar_inline.as_ref().map(|(d, _)| d),
        "avatar_content_type": avatar_inline.as_ref().map(|(_, ct)| ct),
        "public_fields": req.public_fields,
        "friends_fields": req.friends_fields,
        "updated_at": now,
    });
    let friends_body = serde_json::to_string(&friends_profile).unwrap_or_default();
    let mut friends_delivered = 0usize;

    for (peer_id, enc_key) in &friend_enc_keys {
        let content_key = generate_content_key();
        let mut nonce = [0u8; NONCE_SIZE];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut nonce);

        if let Ok(payload) = build_encrypted_payload(
            friends_body.as_bytes(),
            enc_key,
            WEB_PAYLOAD_AAD,
            WEB_HPKE_INFO,
            &content_key,
            &nonce,
            None,
        ) {
            if let Ok(envelope) = build_envelope_from_payload(
                keypair_id.clone(),
                peer_id.clone(),
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
                if let Some(ref relay_url) = relay_url {
                    if let Err(e) = post_envelope(relay_url, &envelope) {
                        crate::tlog!(
                            "failed to send friends profile to {}: {}",
                            crate::logging::peer_id(peer_id),
                            e
                        );
                    }
                }
            }
        }
        friends_delivered += 1;
    }

    crate::tlog!(
        "send: profile update (public_relay={}, friends={}/{})",
        public_profile_delivered,
        friends_delivered,
        friend_enc_keys.len()
    );

    let json = serde_json::json!({
        "user_id": keypair_id,
        "display_name": req.display_name,
        "bio": req.bio,
        "avatar_hash": req.avatar_hash,
        "public_fields": req.public_fields,
        "friends_fields": req.friends_fields,
        "updated_at": now,
    });
    (StatusCode::OK, axum::Json(json)).into_response()
}

pub async fn get_peer_profile_handler(
    State(state): State<SharedState>,
    Path(peer_id): Path<String>,
) -> Response {
    let st = state.lock().await;

    // Check if peer is a friend
    let is_friend = st
        .storage
        .get_peer(&peer_id)
        .ok()
        .flatten()
        .map(|p| p.is_friend)
        .unwrap_or(false);

    match st.storage.get_profile(&peer_id) {
        Ok(Some(profile)) => {
            let public_fields: serde_json::Value =
                serde_json::from_str(&profile.public_fields).unwrap_or(serde_json::json!({}));

            let mut json = serde_json::json!({
                "user_id": profile.user_id,
                "display_name": profile.display_name,
                "bio": profile.bio,
                "avatar_hash": profile.avatar_hash,
                "public_fields": public_fields,
                "updated_at": profile.updated_at,
            });

            // If the peer is a friend, include friends-only fields
            if is_friend {
                let friends_fields: serde_json::Value =
                    serde_json::from_str(&profile.friends_fields).unwrap_or(serde_json::json!({}));
                json["friends_fields"] = friends_fields;
            }

            (StatusCode::OK, axum::Json(json)).into_response()
        }
        Ok(None) => {
            let json = serde_json::json!({
                "user_id": peer_id,
                "display_name": null,
                "bio": null,
                "avatar_hash": null,
                "public_fields": {},
                "updated_at": 0,
            });
            (StatusCode::OK, axum::Json(json)).into_response()
        }
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}
