//! Background relay synchronization: fetching messages, processing envelopes,
//! and announcing online status.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use futures_util::{SinkExt as _, StreamExt as _};
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;

use crate::client::{ClientConfig, ClientEncryption, RelayClient, SyncEventOutcome};
use crate::crypto::{generate_content_key, NONCE_SIZE};
use crate::protocol::{
    build_encrypted_payload, build_envelope_from_payload, build_meta_payload, MessageKind,
    MetaMessage,
};
use crate::relay_transport::post_envelope;
use crate::web_client::config::{
    DEFAULT_TTL_SECONDS, SYNC_INTERVAL_SECS, WEB_HPKE_INFO, WEB_PAYLOAD_AAD,
};
use crate::web_client::state::{SharedState, WsEvent};
use crate::web_client::utils::now_secs;

/// Runs the background relay sync loop with exponential backoff on failure.
///
/// `notify` is signalled by `relay_ws_listen_loop` whenever the relay pushes a
/// new envelope over its WebSocket connection.  The loop wakes immediately on
/// that signal so messages are delivered in near-real-time; the periodic
/// interval serves as a polling fallback when the WS is unavailable.
pub async fn relay_sync_loop(state: SharedState, notify: Arc<Notify>) {
    let mut consecutive_failures = 0u32;
    const MAX_BACKOFF_SECS: u64 = 300; // 5 minutes

    loop {
        // Calculate interval with exponential backoff on failure
        let interval_secs = if consecutive_failures == 0 {
            SYNC_INTERVAL_SECS
        } else {
            // Exponential backoff: 30s * 2^failures, capped at 5 minutes
            let backoff =
                SYNC_INTERVAL_SECS.saturating_mul(2u64.saturating_pow(consecutive_failures));
            backoff.min(MAX_BACKOFF_SECS)
        };

        // Wake on a WS push notification OR the polling interval, whichever
        // comes first.
        tokio::select! {
            _ = notify.notified() => {}
            _ = tokio::time::sleep(Duration::from_secs(interval_secs)) => {}
        }

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
                let next_retry_secs = SYNC_INTERVAL_SECS
                    .saturating_mul(2u64.saturating_pow(consecutive_failures))
                    .min(MAX_BACKOFF_SECS);

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
    // Extract what we need from state under a short lock.
    let (keypair, relay_url, peers, groups) = {
        let st = state.lock().await;
        let relay_url = st
            .relay_url
            .clone()
            .ok_or_else(|| "no relay configured".to_string())?;
        let peers = st.storage.list_peers().map_err(|e| e.to_string())?;
        let groups = st.storage.list_groups().map_err(|e| e.to_string())?;
        (st.keypair.clone(), relay_url, peers, groups)
    };

    // Build a RelayClient populated with all known peers and groups.
    let config = ClientConfig::new(
        &relay_url,
        DEFAULT_TTL_SECONDS,
        ClientEncryption::Encrypted {
            hpke_info: WEB_HPKE_INFO.to_vec(),
            payload_aad: WEB_PAYLOAD_AAD.to_vec(),
        },
    );
    let mut relay_client = RelayClient::new(keypair.clone(), config);
    for peer in &peers {
        relay_client.add_peer_with_encryption(
            peer.peer_id.clone(),
            peer.signing_public_key.clone(),
            peer.encryption_public_key
                .clone()
                .unwrap_or_else(|| peer.signing_public_key.clone()),
        );
    }
    for group in &groups {
        let mut gm = relay_client.group_manager_mut();
        // Reconstruct a minimal GroupInfo from the stored group row so that
        // FriendGroup messages can be decrypted.
        gm.add_group_key(group.group_id.clone(), group.group_key.clone());
    }

    // Fetch and process all envelopes. The handler is not set here; we use the
    // pull API (outcome.events) so we can perform async storage writes below.
    let outcome = relay_client.sync_inbox(None).map_err(|e| e.to_string())?;

    if outcome.fetched == 0 {
        return Ok(());
    }

    crate::tlog!("sync: fetched {} envelope(s) from relay", outcome.fetched);

    let now = now_secs();
    let mut stored_count = 0u32;

    // Process each event, writing to storage and broadcasting WsEvents.
    for event in &outcome.events {
        match &event.outcome {
            SyncEventOutcome::Meta(meta) => {
                process_meta_event(state, meta, &keypair, &relay_url, now).await;
            }

            SyncEventOutcome::RawMeta { body } => {
                process_raw_meta_event(state, &event.envelope, body, &keypair.id, now).await;
            }

            SyncEventOutcome::Message(message) => {
                let stored =
                    process_message_event(state, &event.envelope, message, &keypair.id, now).await;
                if stored {
                    stored_count += 1;
                }
            }

            SyncEventOutcome::Duplicate
            | SyncEventOutcome::TtlExpired
            | SyncEventOutcome::UnknownSender => {
                // Nothing to do for these outcomes.
            }

            SyncEventOutcome::InvalidSignature { reason } => {
                crate::tlog!(
                    "sync: invalid signature from {}: {}",
                    crate::logging::peer_id(&event.envelope.header.sender_id),
                    reason
                );
            }

            SyncEventOutcome::DecryptFailed { reason } => {
                crate::tlog!(
                    "sync: decrypt error from {}: {}",
                    crate::logging::peer_id(&event.envelope.header.sender_id),
                    reason
                );
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

// ---------------------------------------------------------------------------
// Meta-message dispatch helpers (async, need WsEvent broadcasting)
// ---------------------------------------------------------------------------

async fn process_meta_event(
    state: &SharedState,
    meta: &MetaMessage,
    keypair: &crate::crypto::StoredKeypair,
    relay_url: &str,
    now: u64,
) {
    match meta {
        MetaMessage::Online { peer_id, timestamp } => {
            let st = state.lock().await;
            if let Ok(true) = st.storage.update_peer_online(peer_id, true, *timestamp) {
                let _ = st.ws_tx.send(WsEvent::PeerOnline {
                    peer_id: peer_id.clone(),
                });
                crate::tlog!(
                    "sync: peer {} is now online",
                    crate::logging::peer_id(peer_id)
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
                .update_peer_online(peer_id, true, *online_timestamp);
        }
        MetaMessage::FriendRequest {
            peer_id: from_peer_id,
            signing_public_key,
            encryption_public_key,
            message,
        } => {
            process_incoming_friend_request(
                state,
                keypair,
                relay_url,
                now,
                from_peer_id,
                signing_public_key,
                encryption_public_key,
                message.clone(),
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
                keypair,
                now,
                from_peer_id,
                signing_public_key.clone(),
                encryption_public_key.clone(),
            )
            .await;
        }
        MetaMessage::GroupInvite {
            peer_id: from_peer_id,
            group_id,
            message,
            ..
        } => {
            process_incoming_group_invite(
                state,
                from_peer_id,
                group_id,
                message.clone(),
                now,
            )
            .await;
        }
        MetaMessage::GroupInviteAccept {
            peer_id: from_peer_id,
            group_id,
        } => {
            process_group_invite_accept(
                state,
                keypair,
                relay_url,
                from_peer_id,
                group_id,
                now,
            )
            .await;
        }
        MetaMessage::MessageRequest { .. } => {}
    }
}

async fn process_raw_meta_event(
    state: &SharedState,
    envelope: &crate::protocol::Envelope,
    body: &str,
    my_peer_id: &str,
    now: u64,
) {
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(body) {
        // Reaction
        if let (Some(target_id), Some(reaction)) = (
            parsed.get("target_message_id").and_then(|v| v.as_str()),
            parsed.get("reaction").and_then(|v| v.as_str()),
        ) {
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

                // Notification if reaction is to our own message.
                if let Ok(Some(target_msg)) = st.storage.get_message(target_id) {
                    if target_msg.sender_id == my_peer_id {
                        let notif = crate::storage::NotificationRow {
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

/// Process a successfully decoded message event: store it, handle attachments,
/// create notifications, and broadcast WsEvents.  Returns `true` if stored.
async fn process_message_event(
    state: &SharedState,
    envelope: &crate::protocol::Envelope,
    message: &crate::client::ClientMessage,
    my_peer_id: &str,
    now: u64,
) -> bool {
    let st = state.lock().await;

    // Deduplication at the DB level.
    if st.storage.has_message(&message.message_id).unwrap_or(true) {
        return false;
    }

    let sender_id = &envelope.header.sender_id;
    let kind_str = match envelope.header.message_kind {
        MessageKind::Public => "public",
        MessageKind::Direct => "direct",
        MessageKind::FriendGroup => "friend_group",
        MessageKind::Meta => "meta",
        MessageKind::StoreForPeer => "store_for_peer",
    };

    // Check for special message types that should not appear in the timeline.
    // These must be handled by process_meta_event / process_raw_meta_event above.
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&message.body) {
        let msg_type = parsed.get("type").and_then(|t| t.as_str());
        if msg_type == Some("tenet.profile") {
            drop(st);
            process_profile_update(state, sender_id, &envelope.header.timestamp, &parsed, now)
                .await;
            return false;
        }
        if msg_type == Some("group_key_distribution") {
            drop(st);
            process_group_key_distribution(state, sender_id, &parsed, now).await;
            return false;
        }
    }

    // Update last_seen_online for the sender.
    if let Ok(Some(_)) = st.storage.get_peer(sender_id) {
        let _ = st
            .storage
            .update_peer_online(sender_id, false, envelope.header.timestamp);
    }

    crate::tlog!(
        "sync: received {} message from {} (id={})",
        kind_str,
        crate::logging::peer_id(sender_id),
        crate::logging::msg_id(&message.message_id)
    );

    let row = crate::storage::MessageRow {
        message_id: message.message_id.clone(),
        sender_id: sender_id.clone(),
        recipient_id: my_peer_id.to_string(),
        message_kind: kind_str.to_string(),
        group_id: envelope.header.group_id.clone(),
        body: Some(message.body.clone()),
        timestamp: message.timestamp,
        received_at: now,
        ttl_seconds: DEFAULT_TTL_SECONDS,
        is_read: false,
        raw_envelope: None,
        reply_to: envelope.header.reply_to.clone(),
    };

    if st.storage.insert_message(&row).is_err() {
        return false;
    }

    // Store any inline attachment data.
    for (i, att_ref) in envelope.payload.attachments.iter().enumerate() {
        if let Some(ref data_b64) = att_ref.data {
            if let Ok(data) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(data_b64) {
                let att_row = crate::storage::AttachmentRow {
                    content_hash: att_ref.content_id.0.clone(),
                    content_type: att_ref.content_type.clone(),
                    size_bytes: att_ref.size,
                    data,
                    created_at: now,
                };
                let _ = st.storage.insert_attachment(&att_row);
                let _ =
                    st.storage
                        .insert_message_attachment(&crate::storage::MessageAttachmentRow {
                            message_id: message.message_id.clone(),
                            content_hash: att_ref.content_id.0.clone(),
                            filename: att_ref.filename.clone(),
                            position: i as u32,
                        });
            }
        }
    }

    let _ = st.ws_tx.send(WsEvent::NewMessage {
        message_id: message.message_id.clone(),
        sender_id: sender_id.clone(),
        message_kind: kind_str.to_string(),
        body: row.body.clone(),
        timestamp: message.timestamp,
        reply_to: row.reply_to.clone(),
    });

    // Create notifications for direct messages and replies.
    let notification_type = if row.reply_to.is_some() {
        if let Some(ref parent_id) = row.reply_to {
            if let Ok(Some(parent_msg)) = st.storage.get_message(parent_id) {
                if parent_msg.sender_id == my_peer_id {
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
        let notif = crate::storage::NotificationRow {
            id: 0,
            notification_type: notif_type.to_string(),
            message_id: message.message_id.clone(),
            sender_id: sender_id.clone(),
            created_at: now,
            seen: false,
            read: false,
        };
        if let Ok(notif_id) = st.storage.insert_notification(&notif) {
            let _ = st.ws_tx.send(WsEvent::Notification {
                id: notif_id,
                notification_type: notif_type.to_string(),
                message_id: message.message_id.clone(),
                sender_id: sender_id.clone(),
                created_at: now,
            });
        }
    }

    true
}

// ---------------------------------------------------------------------------
// Friend request / accept helpers (unchanged logic, now called from above)
// ---------------------------------------------------------------------------

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
            let peer_row = crate::storage::PeerRow {
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
    let fr_row = crate::storage::FriendRequestRow {
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
            let notif = crate::storage::NotificationRow {
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
        let peer_row = crate::storage::PeerRow {
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
    now: u64,
) {
    let st = state.lock().await;
    let profile_user_id = parsed
        .get("user_id")
        .and_then(|v| v.as_str())
        .unwrap_or(sender_id)
        .to_string();
    let updated_at = parsed
        .get("updated_at")
        .and_then(|v| v.as_u64())
        .unwrap_or(*header_timestamp);

    let profile_row = crate::storage::ProfileRow {
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

    // Store inline avatar attachment before the profile upsert check.
    // The relay delivers each message only once, so we must extract attachment
    // data on the first (and only) receipt regardless of profile timestamp order.
    // insert_attachment is idempotent: it skips the file write if already on disk
    // and uses OR IGNORE for the DB row, so calling it unconditionally is safe.
    if let Some(hash) = &profile_row.avatar_hash {
        if let Some(data_b64) = parsed.get("avatar_data").and_then(|v| v.as_str()) {
            match base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(data_b64) {
                Ok(data) => {
                    let content_type = parsed
                        .get("avatar_content_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("image/jpeg")
                        .to_string();
                    let att_row = crate::storage::AttachmentRow {
                        content_hash: hash.clone(),
                        content_type,
                        size_bytes: data.len() as u64,
                        data,
                        created_at: now,
                    };
                    if let Err(e) = st.storage.insert_attachment(&att_row) {
                        crate::tlog!(
                            "sync: failed to store avatar for {}: {}",
                            crate::logging::peer_id(&profile_user_id),
                            e
                        );
                    } else {
                        crate::tlog!(
                            "sync: stored avatar for {}",
                            crate::logging::peer_id(&profile_user_id)
                        );
                    }
                }
                Err(e) => {
                    crate::tlog!(
                        "sync: failed to decode avatar_data for {}: {}",
                        crate::logging::peer_id(&profile_user_id),
                        e
                    );
                }
            }
        } else {
            crate::tlog!(
                "sync: profile from {} has avatar_hash but no avatar_data",
                crate::logging::peer_id(&profile_user_id)
            );
        }
    }

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
            let _ = st
                .ws_tx
                .send(crate::web_client::state::WsEvent::ProfileUpdated {
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

// ---------------------------------------------------------------------------
// Relay WebSocket push listener
// ---------------------------------------------------------------------------

/// Connects to the relay's WebSocket endpoint and signals `notify` whenever an
/// envelope arrives so that `relay_sync_loop` wakes up immediately.
///
/// Reconnects with exponential backoff on disconnect or error.  Exits cleanly
/// if no relay URL is configured.
pub async fn relay_ws_listen_loop(state: SharedState, notify: Arc<Notify>, web_ui_port: u16) {
    let mut backoff_secs = 2u64;
    const MAX_BACKOFF_SECS: u64 = 60;

    loop {
        let (relay_url, peer_id, signing_priv) = {
            let st = state.lock().await;
            match st.relay_url.clone() {
                Some(url) => (
                    url,
                    st.keypair.id.clone(),
                    st.keypair.signing_private_key_hex.clone(),
                ),
                None => return, // No relay configured — nothing to do.
            }
        };

        let token = match crate::crypto::make_relay_auth_token(&signing_priv, &peer_id) {
            Ok(t) => t,
            Err(e) => {
                crate::tlog!("relay WS: auth token error: {}", e);
                tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECS);
                continue;
            }
        };
        let ws_url = format!("{}?token={}", relay_http_to_ws(&relay_url, &peer_id), token);

        match tokio_tungstenite::connect_async(&ws_url).await {
            Ok((ws_stream, _response)) => {
                backoff_secs = 2; // reset on successful connect
                crate::tlog!("relay WS connected: {}", ws_url);

                let (mut write, mut read) = ws_stream.split();

                // Announce client identity so the relay can display it in the dashboard.
                let hello = serde_json::json!({
                    "type": "hello",
                    "version": env!("CARGO_PKG_VERSION"),
                    "web_ui_port": web_ui_port,
                });
                if let Ok(text) = serde_json::to_string(&hello) {
                    let _ = write.send(WsMessage::Text(text)).await;
                }
                // write half is kept alive for the duration of the connection.

                while let Some(msg) = read.next().await {
                    match msg {
                        Ok(WsMessage::Text(_)) | Ok(WsMessage::Binary(_)) => {
                            // An envelope was pushed — wake the sync loop.
                            notify.notify_one();
                        }
                        Ok(WsMessage::Close(_)) => break,
                        Err(e) => {
                            crate::tlog!("relay WS error: {}", e);
                            break;
                        }
                        _ => {}
                    }
                }

                crate::tlog!("relay WS disconnected, reconnecting in {}s", backoff_secs);
            }
            Err(e) => {
                crate::tlog!(
                    "relay WS connection failed (retry in {}s): {}",
                    backoff_secs,
                    e
                );
            }
        }

        tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
        backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECS);
    }
}

/// Convert an HTTP(S) relay base URL into a WS(S) URL for `/ws/<peer_id>`.
fn relay_http_to_ws(relay_url: &str, peer_id: &str) -> String {
    let base = if relay_url.starts_with("https://") {
        relay_url.replacen("https://", "wss://", 1)
    } else {
        relay_url.replacen("http://", "ws://", 1)
    };
    format!("{}/ws/{}", base.trim_end_matches('/'), peer_id)
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

// ---------------------------------------------------------------------------
// Group invite helpers
// ---------------------------------------------------------------------------

/// Process an incoming GroupInvite meta message: store the invite, create a
/// notification, and broadcast a WsEvent to the UI.
async fn process_incoming_group_invite(
    state: &SharedState,
    from_peer_id: &str,
    group_id: &str,
    message: Option<String>,
    now: u64,
) {
    crate::tlog!(
        "sync: received group_invite for {} from {}",
        group_id,
        crate::logging::peer_id(from_peer_id)
    );

    let st = state.lock().await;
    let my_id = st.keypair.id.clone();

    // Dedup: if an incoming invite for this (group_id, from, me) already exists, skip.
    let existing = st
        .storage
        .find_group_invite(group_id, from_peer_id, &my_id, "incoming")
        .unwrap_or(None);
    if existing.is_some() {
        crate::tlog!(
            "sync: duplicate group_invite for {} from {}, ignoring",
            group_id,
            crate::logging::peer_id(from_peer_id)
        );
        return;
    }

    let invite_row = crate::storage::GroupInviteRow {
        id: 0,
        group_id: group_id.to_string(),
        from_peer_id: from_peer_id.to_string(),
        to_peer_id: my_id.clone(),
        status: "pending".to_string(),
        message: message.clone(),
        direction: "incoming".to_string(),
        created_at: now,
        updated_at: now,
    };

    match st.storage.insert_group_invite(&invite_row) {
        Ok(invite_id) => {
            let _ = st.ws_tx.send(WsEvent::GroupInviteReceived {
                invite_id,
                group_id: group_id.to_string(),
                from_peer_id: from_peer_id.to_string(),
                message: message.clone(),
                created_at: now,
            });

            let notif = crate::storage::NotificationRow {
                id: 0,
                notification_type: "group_invite".to_string(),
                message_id: format!("group_invite_{invite_id}"),
                sender_id: from_peer_id.to_string(),
                created_at: now,
                seen: false,
                read: false,
            };
            if let Ok(notif_id) = st.storage.insert_notification(&notif) {
                let _ = st.ws_tx.send(WsEvent::Notification {
                    id: notif_id,
                    notification_type: "group_invite".to_string(),
                    message_id: format!("group_invite_{invite_id}"),
                    sender_id: from_peer_id.to_string(),
                    created_at: now,
                });
            }

            crate::tlog!(
                "sync: stored group_invite for {} from {} (id={})",
                group_id,
                crate::logging::peer_id(from_peer_id),
                invite_id
            );
        }
        Err(e) => {
            crate::tlog!(
                "sync: failed to store group_invite for {} from {}: {}",
                group_id,
                crate::logging::peer_id(from_peer_id),
                e
            );
        }
    }
}

/// Process an incoming GroupInviteAccept meta message: mark the outgoing invite
/// accepted, add the peer as an active group member, then distribute the group
/// key to them.
async fn process_group_invite_accept(
    state: &SharedState,
    keypair: &crate::crypto::StoredKeypair,
    relay_url: &str,
    from_peer_id: &str,
    group_id: &str,
    now: u64,
) {
    crate::tlog!(
        "sync: received group_invite_accept for {} from {}",
        group_id,
        crate::logging::peer_id(from_peer_id)
    );

    // Lock: update invite status, add member, collect data needed for key distribution.
    let (group_key, key_version, recipient_enc_key) = {
        let st = state.lock().await;

        // Mark the outgoing invite as accepted.
        if let Ok(Some(invite)) =
            st.storage
                .find_group_invite(group_id, &keypair.id, from_peer_id, "outgoing")
        {
            let _ = st.storage.update_group_invite_status(invite.id, "accepted");
        } else {
            crate::tlog!(
                "sync: no outgoing invite found for {} to {}, ignoring accept",
                group_id,
                crate::logging::peer_id(from_peer_id)
            );
            return;
        }

        let group = match st.storage.get_group(group_id) {
            Ok(Some(g)) => g,
            _ => {
                crate::tlog!(
                    "sync: group {} not found when processing invite accept from {}",
                    group_id,
                    crate::logging::peer_id(from_peer_id)
                );
                return;
            }
        };

        let peer = match st.storage.get_peer(from_peer_id) {
            Ok(Some(p)) => p,
            _ => {
                crate::tlog!(
                    "sync: peer {} not found when processing invite accept",
                    crate::logging::peer_id(from_peer_id)
                );
                return;
            }
        };

        let enc_key = match peer.encryption_public_key {
            Some(k) => k,
            None => {
                crate::tlog!(
                    "sync: peer {} has no encryption key; cannot distribute group key",
                    crate::logging::peer_id(from_peer_id)
                );
                return;
            }
        };

        let gk: [u8; 32] = match group.group_key.try_into() {
            Ok(k) => k,
            Err(_) => {
                crate::tlog!("sync: invalid group key for {}", group_id);
                return;
            }
        };

        // Add accepted peer as an active group member.
        let member = crate::storage::GroupMemberRow {
            group_id: group_id.to_string(),
            peer_id: from_peer_id.to_string(),
            joined_at: now,
        };
        let _ = st.storage.insert_group_member(&member);

        let _ = st.ws_tx.send(WsEvent::GroupMemberJoined {
            group_id: group_id.to_string(),
            peer_id: from_peer_id.to_string(),
        });

        (gk, group.key_version, enc_key)
    };
    // Lock released — now do crypto and I/O.

    let key_distribution = serde_json::json!({
        "type": "group_key_distribution",
        "group_id": group_id,
        "group_key": hex::encode(group_key),
        "key_version": key_version,
        "creator_id": keypair.id,
    });

    let key_dist_bytes = serde_json::to_vec(&key_distribution).unwrap_or_default();

    let content_key = generate_content_key();
    let mut nonce = [0u8; NONCE_SIZE];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut nonce);

    let payload = match build_encrypted_payload(
        &key_dist_bytes,
        &recipient_enc_key,
        crate::web_client::config::WEB_PAYLOAD_AAD,
        crate::web_client::config::WEB_HPKE_INFO,
        &content_key,
        &nonce,
        None,
    ) {
        Ok(p) => p,
        Err(e) => {
            crate::tlog!(
                "sync: failed to encrypt group key for {}: {}",
                crate::logging::peer_id(from_peer_id),
                e
            );
            return;
        }
    };

    let envelope = match build_envelope_from_payload(
        keypair.id.clone(),
        from_peer_id.to_string(),
        None,
        None,
        now,
        DEFAULT_TTL_SECONDS,
        MessageKind::Direct,
        None,
        None,
        payload,
        &keypair.signing_private_key_hex,
    ) {
        Ok(e) => e,
        Err(e) => {
            crate::tlog!(
                "sync: failed to build group key envelope for {}: {}",
                crate::logging::peer_id(from_peer_id),
                e
            );
            return;
        }
    };

    match post_envelope(relay_url, &envelope) {
        Ok(()) => crate::tlog!(
            "sync: distributed group key for {} to {}",
            group_id,
            crate::logging::peer_id(from_peer_id)
        ),
        Err(e) => crate::tlog!(
            "sync: failed to distribute group key for {} to {}: {}",
            group_id,
            crate::logging::peer_id(from_peer_id),
            e
        ),
    }
}

/// Process an incoming group_key_distribution Direct message: validate consent
/// via an accepted invite, then store the group key locally.
async fn process_group_key_distribution(
    state: &SharedState,
    sender_id: &str,
    payload: &serde_json::Value,
    now: u64,
) {
    let group_id = match payload.get("group_id").and_then(|v| v.as_str()) {
        Some(id) => id,
        None => return,
    };
    let group_key_hex = match payload.get("group_key").and_then(|v| v.as_str()) {
        Some(k) => k,
        None => return,
    };
    let key_version = payload
        .get("key_version")
        .and_then(|v| v.as_u64())
        .unwrap_or(1) as u32;
    let creator_id = payload
        .get("creator_id")
        .and_then(|v| v.as_str())
        .unwrap_or(sender_id)
        .to_string();

    let group_key = match hex::decode(group_key_hex) {
        Ok(k) if k.len() == 32 => k,
        _ => {
            crate::tlog!(
                "sync: invalid group_key in key_distribution for {}",
                group_id
            );
            return;
        }
    };

    let st = state.lock().await;

    // Consent check: only store the key if we have an accepted invite for this group.
    let has_accepted_invite = st
        .storage
        .find_group_invite(group_id, sender_id, &st.keypair.id, "incoming")
        .unwrap_or(None)
        .map(|inv| inv.status == "accepted")
        .unwrap_or(false);

    if !has_accepted_invite {
        crate::tlog!(
            "sync: ignoring group_key_distribution for {} from {} — no accepted invite",
            group_id,
            crate::logging::peer_id(sender_id)
        );
        return;
    }

    let group_row = crate::storage::GroupRow {
        group_id: group_id.to_string(),
        group_key,
        creator_id,
        created_at: now,
        key_version,
    };

    if let Err(e) = st.storage.insert_group(&group_row) {
        crate::tlog!(
            "sync: failed to store group key for {}: {}",
            group_id,
            e
        );
        return;
    }

    // Add ourselves as an active member.
    let member = crate::storage::GroupMemberRow {
        group_id: group_id.to_string(),
        peer_id: st.keypair.id.clone(),
        joined_at: now,
    };
    let _ = st.storage.insert_group_member(&member);

    crate::tlog!("sync: stored group key for {}", group_id);
}
