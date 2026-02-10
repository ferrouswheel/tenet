//! Background relay synchronization: fetching messages, processing envelopes,
//! and announcing online status.

use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::protocol::{
    build_envelope_from_payload, build_meta_payload, decode_meta_payload,
    decrypt_encrypted_payload, MessageKind, MetaMessage,
};
use base64::Engine as _;

use crate::relay_transport::{fetch_inbox, post_envelope};
use crate::storage::{AttachmentRow, FriendRequestRow, MessageAttachmentRow, MessageRow, NotificationRow, PeerRow, ProfileRow};
use crate::web_client::config::{
    DEFAULT_TTL_SECONDS, SYNC_INTERVAL_SECS, WEB_HPKE_INFO, WEB_PAYLOAD_AAD,
};
use crate::web_client::state::{SharedState, WsEvent};
use crate::web_client::utils::now_secs;

/// Runs the background relay sync loop with exponential backoff on failure.
pub async fn relay_sync_loop(state: SharedState) {
    let mut consecutive_failures = 0u32;
    const MAX_BACKOFF_SECS: u64 = 300; // 5 minutes

    loop {
        // Calculate interval with exponential backoff on failure
        let interval_secs = if consecutive_failures == 0 {
            SYNC_INTERVAL_SECS
        } else {
            // Exponential backoff: 30s * 2^failures, capped at 5 minutes
            let backoff = SYNC_INTERVAL_SECS * 2u64.pow(consecutive_failures);
            backoff.min(MAX_BACKOFF_SECS)
        };

        tokio::time::sleep(Duration::from_secs(interval_secs)).await;

        match sync_once(&state).await {
            Ok(()) => {
                let was_disconnected = consecutive_failures > 0;
                consecutive_failures = 0;

                let st = state.lock().await;
                let was_connected = st.relay_connected.swap(true, Ordering::Relaxed);
                if !was_connected || was_disconnected {
                    let relay_url = st.relay_url.clone();
                    crate::tlog!(
                        "relay connected: {}",
                        relay_url.as_deref().unwrap_or("unknown")
                    );
                    let _ = st.ws_tx.send(WsEvent::RelayStatus {
                        connected: true,
                        relay_url,
                    });
                }
            }
            Err(e) => {
                consecutive_failures += 1;
                let next_retry_secs =
                    (SYNC_INTERVAL_SECS * 2u64.pow(consecutive_failures)).min(MAX_BACKOFF_SECS);

                let st = state.lock().await;
                let was_connected = st.relay_connected.swap(false, Ordering::Relaxed);
                if was_connected || consecutive_failures == 1 {
                    let relay_url = st.relay_url.clone();
                    crate::tlog!(
                        "relay disconnected: {} (attempt {}, next retry in {}s): {}",
                        relay_url.as_deref().unwrap_or("unknown"),
                        consecutive_failures,
                        next_retry_secs,
                        e
                    );
                    let _ = st.ws_tx.send(WsEvent::RelayStatus {
                        connected: false,
                        relay_url,
                    });
                } else {
                    crate::tlog!(
                        "relay sync error (attempt {}, next retry in {}s): {}",
                        consecutive_failures,
                        next_retry_secs,
                        e
                    );
                }
            }
        }
    }
}

/// Perform a single relay sync: fetch envelopes, verify, decrypt, and store.
pub async fn sync_once(state: &SharedState) -> Result<(), String> {
    // Extract what we need from state under a short lock
    let (keypair, relay_url, peers) = {
        let st = state.lock().await;
        let relay_url = st
            .relay_url
            .clone()
            .ok_or_else(|| "no relay configured".to_string())?;
        let peers = st.storage.list_peers().map_err(|e| e.to_string())?;
        (st.keypair.clone(), relay_url, peers)
    };

    // Build a peer lookup map for signature verification and decryption
    let peer_map: std::collections::HashMap<String, &crate::storage::PeerRow> =
        peers.iter().map(|p| (p.peer_id.clone(), p)).collect();

    // Fetch envelopes from relay
    let envelopes = fetch_inbox(&relay_url, &keypair.id)?;

    if envelopes.is_empty() {
        return Ok(());
    }

    crate::tlog!("sync: fetched {} envelope(s) from relay", envelopes.len());

    let now = now_secs();
    let mut stored_count = 0u32;

    for envelope in &envelopes {
        // --- Meta messages: process from ALL senders (including unknown) ---
        if envelope.header.message_kind == MessageKind::Meta {
            if let Ok(meta_msg) = decode_meta_payload(&envelope.payload) {
                match meta_msg {
                    MetaMessage::Online { peer_id, timestamp } => {
                        let st = state.lock().await;
                        if let Ok(true) = st.storage.update_peer_online(&peer_id, true, timestamp) {
                            let _ = st.ws_tx.send(WsEvent::PeerOnline {
                                peer_id: peer_id.clone(),
                            });
                            crate::tlog!(
                                "sync: peer {} is now online",
                                crate::logging::peer_id(&peer_id)
                            );
                        }
                    }
                    MetaMessage::Ack {
                        peer_id,
                        online_timestamp,
                    } => {
                        let st = state.lock().await;
                        let _ = st
                            .storage
                            .update_peer_online(&peer_id, true, online_timestamp);
                    }
                    MetaMessage::FriendRequest {
                        peer_id: from_peer_id,
                        signing_public_key,
                        encryption_public_key,
                        message,
                    } => {
                        process_incoming_friend_request(
                            state,
                            &keypair,
                            &relay_url,
                            now,
                            &from_peer_id,
                            &signing_public_key,
                            &encryption_public_key,
                            message,
                        )
                        .await;
                    }
                    MetaMessage::FriendAccept {
                        peer_id: from_peer_id,
                        signing_public_key,
                        encryption_public_key,
                    } => {
                        process_friend_accept(
                            state,
                            &keypair,
                            now,
                            &from_peer_id,
                            signing_public_key,
                            encryption_public_key,
                        )
                        .await;
                    }
                    _ => {}
                }
            } else {
                // If not a standard MetaMessage, check if it's a reaction
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&envelope.payload.body) {
                    if let (Some(target_id), Some(reaction)) = (
                        parsed.get("target_message_id").and_then(|v| v.as_str()),
                        parsed.get("reaction").and_then(|v| v.as_str()),
                    ) {
                        // This is a reaction - store it
                        let st = state.lock().await;
                        let reaction_row = crate::storage::ReactionRow {
                            message_id: envelope.header.message_id.0.clone(),
                            target_id: target_id.to_string(),
                            sender_id: envelope.header.sender_id.clone(),
                            reaction: reaction.to_string(),
                            timestamp: envelope.header.timestamp,
                        };
                        if let Err(e) = st.storage.upsert_reaction(&reaction_row) {
                            crate::tlog!(
                                "sync: failed to store reaction from {}: {}",
                                crate::logging::peer_id(&envelope.header.sender_id),
                                e
                            );
                        } else {
                            crate::tlog!(
                                "sync: received {} reaction from {} for message {}",
                                reaction,
                                crate::logging::peer_id(&envelope.header.sender_id),
                                crate::logging::msg_id(target_id)
                            );

                            // Create notification if reaction is to our own message
                            if let Ok(Some(target_msg)) = st.storage.get_message(target_id) {
                                if target_msg.sender_id == keypair.id {
                                    let notif = NotificationRow {
                                        id: 0,
                                        notification_type: "reaction".to_string(),
                                        message_id: envelope.header.message_id.0.clone(),
                                        sender_id: envelope.header.sender_id.clone(),
                                        created_at: now,
                                        seen: false,
                                        read: false,
                                    };
                                    if let Ok(notif_id) = st.storage.insert_notification(&notif) {
                                        let _ = st.ws_tx.send(WsEvent::Notification {
                                            id: notif_id,
                                            notification_type: "reaction".to_string(),
                                            message_id: envelope.header.message_id.0.clone(),
                                            sender_id: envelope.header.sender_id.clone(),
                                            created_at: now,
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
            }
            continue; // Meta messages are not stored as regular messages
        }

        // --- Non-meta messages: verify signature using known peers ---
        let sender_id = &envelope.header.sender_id;
        let peer = match peer_map.get(sender_id) {
            Some(p) => p,
            None => {
                crate::tlog!(
                    "sync: skipping message from unknown sender {}",
                    crate::logging::peer_id(sender_id)
                );
                continue;
            }
        };

        if let Err(e) = envelope
            .header
            .verify_signature(envelope.version, &peer.signing_public_key)
        {
            crate::tlog!(
                "sync: invalid signature from {}: {:?}",
                crate::logging::peer_id(sender_id),
                e
            );
            continue;
        }

        // Decrypt body for Direct messages, pass through for others
        let body = match envelope.header.message_kind {
            MessageKind::Direct => {
                match decrypt_encrypted_payload(
                    &envelope.payload,
                    &keypair.private_key_hex,
                    WEB_PAYLOAD_AAD,
                    WEB_HPKE_INFO,
                ) {
                    Ok(plaintext) => match String::from_utf8(plaintext) {
                        Ok(s) => Some(s),
                        Err(e) => {
                            crate::tlog!(
                                "sync: utf-8 decode error from {}: {}",
                                crate::logging::peer_id(sender_id),
                                e
                            );
                            continue;
                        }
                    },
                    Err(e) => {
                        crate::tlog!(
                            "sync: decrypt error from {}: {}",
                            crate::logging::peer_id(sender_id),
                            e
                        );
                        continue;
                    }
                }
            }
            _ => Some(envelope.payload.body.clone()),
        };

        let message_id = envelope.header.message_id.0.clone();
        let kind_str = match envelope.header.message_kind {
            MessageKind::Public => "public",
            MessageKind::Direct => "direct",
            MessageKind::FriendGroup => "friend_group",
            MessageKind::Meta => "meta",
            MessageKind::StoreForPeer => "store_for_peer",
        };

        // Check if this is a profile update (body contains "type": "tenet.profile")
        if let Some(ref body_str) = body {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(body_str) {
                if parsed.get("type").and_then(|t| t.as_str()) == Some("tenet.profile") {
                    process_profile_update(state, sender_id, &envelope.header.timestamp, &parsed)
                        .await;
                    continue; // Don't store profile updates as timeline messages
                }

                // Also check for group key distribution messages — don't show in timeline
                if parsed.get("type").and_then(|t| t.as_str()) == Some("group_key_distribution") {
                    continue;
                }
            }
        }

        let st = state.lock().await;
        if st.storage.has_message(&message_id).unwrap_or(true) {
            continue;
        }

        // Update last_seen_online for the sender when we receive a message from them
        if let Ok(Some(_)) = st.storage.get_peer(sender_id) {
            let _ = st
                .storage
                .update_peer_online(sender_id, false, envelope.header.timestamp);
        }

        crate::tlog!(
            "sync: received {} message from {} (id={})",
            kind_str,
            crate::logging::peer_id(sender_id),
            crate::logging::msg_id(&message_id)
        );

        let row = MessageRow {
            message_id: message_id.clone(),
            sender_id: sender_id.clone(),
            recipient_id: keypair.id.clone(),
            message_kind: kind_str.to_string(),
            group_id: envelope.header.group_id.clone(),
            body,
            timestamp: envelope.header.timestamp,
            received_at: now,
            ttl_seconds: DEFAULT_TTL_SECONDS,
            is_read: false,
            raw_envelope: None,
            reply_to: envelope.header.reply_to.clone(),
        };
        if st.storage.insert_message(&row).is_ok() {
            // Store any inline attachment data so it can be served locally
            for (i, att_ref) in envelope.payload.attachments.iter().enumerate() {
                if let Some(ref data_b64) = att_ref.data {
                    if let Ok(data) =
                        base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(data_b64)
                    {
                        let att_row = AttachmentRow {
                            content_hash: att_ref.content_id.0.clone(),
                            content_type: att_ref.content_type.clone(),
                            size_bytes: att_ref.size,
                            data,
                            created_at: now,
                        };
                        let _ = st.storage.insert_attachment(&att_row);
                        let _ = st.storage.insert_message_attachment(&MessageAttachmentRow {
                            message_id: message_id.clone(),
                            content_hash: att_ref.content_id.0.clone(),
                            filename: att_ref.filename.clone(),
                            position: i as u32,
                        });
                    }
                }
            }

            let _ = st.ws_tx.send(WsEvent::NewMessage {
                message_id: message_id.clone(),
                sender_id: sender_id.clone(),
                message_kind: kind_str.to_string(),
                body: row.body.clone(),
                timestamp: envelope.header.timestamp,
                reply_to: row.reply_to.clone(),
            });
            stored_count += 1;

            // Create notifications for direct messages and replies
            let notification_type = if row.reply_to.is_some() {
                // Check if this is a reply to our own message
                if let Some(ref parent_id) = row.reply_to {
                    if let Ok(Some(parent_msg)) = st.storage.get_message(parent_id) {
                        if parent_msg.sender_id == keypair.id {
                            Some("reply")
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else if kind_str == "direct" {
                Some("direct_message")
            } else {
                None
            };

            if let Some(notif_type) = notification_type {
                let notif = NotificationRow {
                    id: 0,
                    notification_type: notif_type.to_string(),
                    message_id: message_id.clone(),
                    sender_id: sender_id.clone(),
                    created_at: now,
                    seen: false,
                    read: false,
                };
                if let Ok(notif_id) = st.storage.insert_notification(&notif) {
                    let _ = st.ws_tx.send(WsEvent::Notification {
                        id: notif_id,
                        notification_type: notif_type.to_string(),
                        message_id: message_id.clone(),
                        sender_id: sender_id.clone(),
                        created_at: now,
                    });
                }
            }
        }
    }

    {
        let st = state.lock().await;
        let _ = st.storage.update_relay_last_sync(&relay_url, now);
    }

    if stored_count > 0 {
        crate::tlog!("sync: stored {} message(s)", stored_count);
    }

    Ok(())
}

/// Process an incoming friend request meta message.
#[allow(clippy::too_many_arguments)]
async fn process_incoming_friend_request(
    state: &SharedState,
    keypair: &crate::crypto::StoredKeypair,
    relay_url: &str,
    now: u64,
    from_peer_id: &str,
    signing_public_key: &str,
    encryption_public_key: &str,
    message: Option<String>,
) {
    crate::tlog!(
        "sync: received friend_request from {}",
        crate::logging::peer_id(from_peer_id)
    );
    let st = state.lock().await;

    // If already friends, treat as duplicate and skip
    if let Ok(Some(peer)) = st.storage.get_peer(from_peer_id) {
        if peer.is_friend {
            crate::tlog!(
                "sync: friend request from {} is a duplicate (already friends), ignoring",
                crate::logging::peer_id(from_peer_id)
            );
            return;
        }
    }

    // Check if we have a pending OUTGOING request to this peer (race condition)
    let outgoing = st
        .storage
        .find_request_between(&keypair.id, from_peer_id)
        .unwrap_or(None);

    if let Some(ref out_req) = outgoing {
        if out_req.status == "pending" {
            // Race condition: both peers sent friend requests to each other.
            // Auto-accept: mark our outgoing request as accepted, add them as a friend,
            // and archive the incoming request as a duplicate.
            crate::tlog!(
                "sync: mutual friend request detected with {} — auto-accepting",
                crate::logging::peer_id(from_peer_id)
            );
            let _ = st
                .storage
                .update_friend_request_status(out_req.id, "accepted");

            // Add peer as friend
            let peer_row = PeerRow {
                peer_id: from_peer_id.to_string(),
                display_name: None,
                signing_public_key: signing_public_key.to_string(),
                encryption_public_key: Some(encryption_public_key.to_string()),
                added_at: now,
                is_friend: true,
                last_seen_online: Some(now),
                online: false,
            };
            let _ = st.storage.insert_peer(&peer_row);

            // Send FriendAccept back so the other peer also completes the handshake
            drop(st);
            {
                let meta_msg = MetaMessage::FriendAccept {
                    peer_id: keypair.id.clone(),
                    signing_public_key: keypair.signing_public_key_hex.clone(),
                    encryption_public_key: keypair.public_key_hex.clone(),
                };
                if let Ok(payload) = build_meta_payload(&meta_msg) {
                    if let Ok(env) = build_envelope_from_payload(
                        keypair.id.clone(),
                        from_peer_id.to_string(),
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
                        if let Err(e) = post_envelope(relay_url, &env) {
                            crate::tlog!(
                                "sync: failed to send auto-accept to {}: {}",
                                crate::logging::peer_id(from_peer_id),
                                e
                            );
                        } else {
                            crate::tlog!(
                                "sync: sent auto-accept to {} via relay",
                                crate::logging::peer_id(from_peer_id)
                            );
                        }
                    }
                }
            }
            let st = state.lock().await;
            let _ = st.ws_tx.send(WsEvent::FriendRequestAccepted {
                request_id: out_req.id,
                from_peer_id: from_peer_id.to_string(),
            });
            return;
        }
    }

    // Check for existing request from this peer
    let existing = st
        .storage
        .find_request_between(from_peer_id, &keypair.id)
        .unwrap_or(None);

    if let Some(ref ex) = existing {
        // If blocked or ignored, silently drop
        if ex.status == "blocked" || ex.status == "ignored" {
            crate::tlog!(
                "sync: friend request from {} is {}, not resurfacing",
                crate::logging::peer_id(from_peer_id),
                ex.status
            );
            return;
        }
        // If pending, refresh the existing record
        if ex.status == "pending" {
            if let Err(e) = st.storage.refresh_friend_request(
                ex.id,
                message.as_deref(),
                signing_public_key,
                encryption_public_key,
            ) {
                crate::tlog!(
                    "sync: failed to refresh friend request from {}: {}",
                    crate::logging::peer_id(from_peer_id),
                    e
                );
            } else {
                crate::tlog!(
                    "sync: refreshed existing friend request from {} (id={})",
                    crate::logging::peer_id(from_peer_id),
                    ex.id
                );
                let _ = st.ws_tx.send(WsEvent::FriendRequestReceived {
                    request_id: ex.id,
                    from_peer_id: from_peer_id.to_string(),
                    message: message.clone(),
                });
            }
            return;
        }
        // If accepted, they are already friends — skip
        if ex.status == "accepted" {
            crate::tlog!(
                "sync: friend request from {} already accepted, skipping",
                crate::logging::peer_id(from_peer_id)
            );
            return;
        }
    }

    // No existing request — create a new one
    let fr_row = FriendRequestRow {
        id: 0,
        from_peer_id: from_peer_id.to_string(),
        to_peer_id: keypair.id.clone(),
        status: "pending".to_string(),
        message: message.clone(),
        from_signing_key: signing_public_key.to_string(),
        from_encryption_key: encryption_public_key.to_string(),
        direction: "incoming".to_string(),
        created_at: now,
        updated_at: now,
    };
    match st.storage.insert_friend_request(&fr_row) {
        Ok(request_id) => {
            let _ = st.ws_tx.send(WsEvent::FriendRequestReceived {
                request_id,
                from_peer_id: from_peer_id.to_string(),
                message: message.clone(),
            });

            // Create notification for friend request
            let notif = NotificationRow {
                id: 0,
                notification_type: "friend_request".to_string(),
                message_id: format!("friend_request_{}", request_id),
                sender_id: from_peer_id.to_string(),
                created_at: now,
                seen: false,
                read: false,
            };
            if let Ok(notif_id) = st.storage.insert_notification(&notif) {
                let _ = st.ws_tx.send(WsEvent::Notification {
                    id: notif_id,
                    notification_type: "friend_request".to_string(),
                    message_id: format!("friend_request_{}", request_id),
                    sender_id: from_peer_id.to_string(),
                    created_at: now,
                });
            }

            crate::tlog!(
                "sync: stored incoming friend request from {} (id={})",
                crate::logging::peer_id(from_peer_id),
                request_id
            );
        }
        Err(e) => {
            crate::tlog!(
                "sync: failed to store friend request from {}: {}",
                crate::logging::peer_id(from_peer_id),
                e
            );
        }
    }
}

/// Process an incoming friend accept meta message.
async fn process_friend_accept(
    state: &SharedState,
    keypair: &crate::crypto::StoredKeypair,
    now: u64,
    from_peer_id: &str,
    signing_public_key: String,
    encryption_public_key: String,
) {
    crate::tlog!(
        "sync: received friend_accept from {}",
        crate::logging::peer_id(from_peer_id)
    );
    let st = state.lock().await;

    // If already friends, treat as duplicate
    if let Ok(Some(peer)) = st.storage.get_peer(from_peer_id) {
        if peer.is_friend {
            crate::tlog!(
                "sync: friend_accept from {} is a duplicate (already friends), ignoring",
                crate::logging::peer_id(from_peer_id)
            );
            return;
        }
    }

    let requests = st
        .storage
        .list_friend_requests(Some("pending"), Some("outgoing"))
        .unwrap_or_default();
    if let Some(pending) = requests.iter().find(|r| r.to_peer_id == from_peer_id) {
        let req_id = pending.id;
        let _ = st.storage.update_friend_request_status(req_id, "accepted");
        let peer_row = PeerRow {
            peer_id: from_peer_id.to_string(),
            display_name: None,
            signing_public_key,
            encryption_public_key: Some(encryption_public_key),
            added_at: now,
            is_friend: true,
            last_seen_online: Some(now),
            online: false,
        };
        let _ = st.storage.insert_peer(&peer_row);

        // Also archive any incoming friend request from this peer (race condition)
        if let Ok(Some(incoming)) = st.storage.find_request_between(from_peer_id, &keypair.id) {
            if incoming.status == "pending" {
                crate::tlog!(
                    "sync: archiving duplicate incoming request from {} (id={})",
                    crate::logging::peer_id(from_peer_id),
                    incoming.id
                );
                let _ = st
                    .storage
                    .update_friend_request_status(incoming.id, "accepted");
            }
        }

        let _ = st.ws_tx.send(WsEvent::FriendRequestAccepted {
            request_id: req_id,
            from_peer_id: from_peer_id.to_string(),
        });
        crate::tlog!(
            "sync: friend request accepted by {} (id={})",
            crate::logging::peer_id(from_peer_id),
            req_id
        );
    } else {
        crate::tlog!(
            "sync: received friend_accept from {} but no matching pending request",
            crate::logging::peer_id(from_peer_id)
        );
    }
}

/// Process a profile update received via relay sync.
async fn process_profile_update(
    state: &SharedState,
    sender_id: &str,
    header_timestamp: &u64,
    parsed: &serde_json::Value,
) {
    let profile_user_id = parsed
        .get("user_id")
        .and_then(|v| v.as_str())
        .unwrap_or(sender_id)
        .to_string();
    let updated_at = parsed
        .get("updated_at")
        .and_then(|v| v.as_u64())
        .unwrap_or(*header_timestamp);

    let profile_row = ProfileRow {
        user_id: profile_user_id.clone(),
        display_name: parsed
            .get("display_name")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        bio: parsed
            .get("bio")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        avatar_hash: parsed
            .get("avatar_hash")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        public_fields: parsed
            .get("public_fields")
            .map(|v| v.to_string())
            .unwrap_or_else(|| "{}".to_string()),
        friends_fields: parsed
            .get("friends_fields")
            .map(|v| v.to_string())
            .unwrap_or_else(|| "{}".to_string()),
        updated_at,
    };

    let st = state.lock().await;
    match st.storage.upsert_profile_if_newer(&profile_row) {
        Ok(true) => {
            crate::tlog!(
                "sync: updated profile for {}",
                crate::logging::peer_id(&profile_user_id)
            );
            // Update peer display name if it changed
            if let Some(ref display_name) = profile_row.display_name {
                if let Ok(Some(mut peer)) = st.storage.get_peer(&profile_user_id) {
                    if peer.display_name.as_deref() != Some(display_name) {
                        peer.display_name = Some(display_name.clone());
                        let _ = st.storage.insert_peer(&peer);
                        crate::tlog!(
                            "sync: updated display name for peer {}",
                            crate::logging::peer_id(&profile_user_id)
                        );
                    }
                }
            }
            let _ = st.ws_tx.send(crate::web_client::state::WsEvent::ProfileUpdated {
                peer_id: profile_user_id.clone(),
                display_name: profile_row.display_name.clone(),
                bio: profile_row.bio.clone(),
                avatar_hash: profile_row.avatar_hash.clone(),
            });
        }
        Ok(false) => {
            crate::tlog!(
                "sync: profile for {} is not newer, skipping",
                crate::logging::peer_id(&profile_user_id)
            );
        }
        Err(e) => {
            crate::tlog!(
                "sync: failed to update profile for {}: {}",
                crate::logging::peer_id(&profile_user_id),
                e
            );
        }
    }
}

/// Send online announcement to all known peers via relay.
pub async fn announce_online(state: SharedState) -> Result<(), String> {
    let (keypair, relay_url, peers) = {
        let st = state.lock().await;
        let relay_url = st
            .relay_url
            .clone()
            .ok_or_else(|| "no relay configured".to_string())?;
        let peers = st.storage.list_peers().map_err(|e| e.to_string())?;
        (st.keypair.clone(), relay_url, peers)
    };

    if peers.is_empty() {
        return Ok(());
    }

    let now = now_secs();
    let meta_msg = MetaMessage::Online {
        peer_id: keypair.id.clone(),
        timestamp: now,
    };

    let payload = build_meta_payload(&meta_msg).map_err(|e| e.to_string())?;

    // Send online announcement to each peer
    for peer in &peers {
        let envelope = build_envelope_from_payload(
            keypair.id.clone(),
            peer.peer_id.clone(),
            None,
            None,
            now,
            DEFAULT_TTL_SECONDS,
            MessageKind::Meta,
            None,
            None,
            payload.clone(),
            &keypair.signing_private_key_hex,
        )
        .map_err(|e| e.to_string())?;

        // Post to relay
        if let Err(e) = post_envelope(&relay_url, &envelope) {
            crate::tlog!(
                "failed to announce online to {}: {}",
                crate::logging::peer_id(&peer.peer_id),
                e
            );
        }
    }

    crate::tlog!("announced online status to {} peers", peers.len());
    Ok(())
}
