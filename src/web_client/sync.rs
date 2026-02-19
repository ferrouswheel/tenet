//! Background relay synchronization: fetching messages, processing envelopes,
//! and announcing online status.
//!
//! Envelope processing is delegated to [`WebClientHandler`], which wraps
//! [`StorageMessageHandler`] and adds WebSocket event broadcasting on top.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use hex;

use futures_util::{SinkExt as _, StreamExt as _};
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;

use crate::client::{ClientConfig, ClientEncryption, ClientMessage, MessageHandler, RelayClient};
use crate::message_handler::StorageMessageHandler;
use crate::protocol::{
    build_envelope_from_payload, build_meta_payload, Envelope, MessageKind, MetaMessage,
};
use crate::relay_transport::post_envelope;
use crate::storage::Storage;
use crate::web_client::config::{
    DEFAULT_MESH_WINDOW_SECS, DEFAULT_TTL_SECONDS, MESH_QUERY_INTERVAL_SECS, SYNC_INTERVAL_SECS,
    WEB_HPKE_INFO, WEB_PAYLOAD_AAD,
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

                {
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

                if let Err(e) = request_missing_profiles(&state).await {
                    crate::tlog!("profile refresh: {}", e);
                }
                if let Err(e) = send_mesh_queries(&state).await {
                    crate::tlog!("mesh queries: {}", e);
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

/// Perform a single relay sync: fetch envelopes, decrypt, and process via [`WebClientHandler`].
pub async fn sync_once(state: &SharedState) -> Result<(), String> {
    // Extract what we need from state under a short lock.
    let (keypair, relay_url, peers, groups, ws_tx, db_path) = {
        let st = state.lock().await;
        let relay_url = st
            .relay_url
            .clone()
            .ok_or_else(|| "no relay configured".to_string())?;
        let peers = st.storage.list_peers().map_err(|e| e.to_string())?;
        let groups = st.storage.list_groups().map_err(|e| e.to_string())?;
        let ws_tx = st.ws_tx.clone();
        let db_path = st.db_path.clone();
        (st.keypair.clone(), relay_url, peers, groups, ws_tx, db_path)
    };

    // Open a dedicated storage connection for the handler.  SQLite WAL mode
    // makes concurrent access from this connection and AppState.storage safe.
    let handler_storage = Storage::open(&db_path).map_err(|e| format!("handler storage: {e}"))?;

    let inner = StorageMessageHandler::new_with_crypto(
        handler_storage,
        keypair.clone(),
        WEB_HPKE_INFO.to_vec(),
        WEB_PAYLOAD_AAD.to_vec(),
    );

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
        relay_client
            .group_manager_mut()
            .add_group_key(group.group_id.clone(), group.group_key.clone());
    }

    relay_client.set_handler(Box::new(WebClientHandler {
        inner,
        ws_tx,
        my_peer_id: keypair.id.clone(),
    }));

    // sync_inbox calls the handler for each envelope during processing.
    let outcome = relay_client.sync_inbox(None).map_err(|e| e.to_string())?;

    if outcome.fetched == 0 {
        return Ok(());
    }

    crate::tlog!("sync: fetched {} envelope(s) from relay", outcome.fetched);

    let now = now_secs();
    {
        let st = state.lock().await;
        let _ = st.storage.update_relay_last_sync(&relay_url, now);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// WebClientHandler: wraps StorageMessageHandler and adds WsEvent broadcasting
// ---------------------------------------------------------------------------

/// A [`MessageHandler`] implementation for the web client.
///
/// Delegates all storage writes to the inner [`StorageMessageHandler`], then
/// broadcasts [`WsEvent`]s over the WebSocket channel so the connected UI
/// receives real-time updates.
struct WebClientHandler {
    inner: StorageMessageHandler,
    ws_tx: tokio::sync::broadcast::Sender<WsEvent>,
    my_peer_id: String,
}

impl MessageHandler for WebClientHandler {
    fn on_message(&mut self, envelope: &Envelope, message: &ClientMessage) -> Vec<Envelope> {
        // Note whether this message is new before delegating (dedup check).
        let was_new = !self
            .inner
            .storage()
            .has_message(&message.message_id)
            .unwrap_or(true);

        let outgoing = self.inner.on_message(envelope, message);

        if !was_new {
            return outgoing; // Already stored — nothing to broadcast.
        }

        // Detect special message types that StorageMessageHandler handles without
        // inserting a MessageRow (profile updates, group key distribution).
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&message.body) {
            match parsed.get("type").and_then(|t| t.as_str()) {
                Some("tenet.profile") => {
                    // Profile was upserted by inner handler — broadcast ProfileUpdated.
                    let user_id = parsed
                        .get("user_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or(&envelope.header.sender_id)
                        .to_string();
                    if let Ok(Some(profile)) = self.inner.storage().get_profile(&user_id) {
                        let _ = self.ws_tx.send(WsEvent::ProfileUpdated {
                            peer_id: profile.user_id.clone(),
                            display_name: profile.display_name.clone(),
                            bio: profile.bio.clone(),
                            avatar_hash: profile.avatar_hash.clone(),
                        });
                    }
                    return outgoing;
                }
                Some("group_key_distribution") => {
                    return outgoing; // No UI event needed.
                }
                _ => {}
            }
        }

        // Regular message — check if it was actually stored (not a dedup at handler level).
        if self
            .inner
            .storage()
            .has_message(&message.message_id)
            .unwrap_or(false)
        {
            let kind_str = message_kind_str(&envelope.header.message_kind);
            crate::tlog!(
                "sync: received {} message from {} (id={})",
                kind_str,
                crate::logging::peer_id(&envelope.header.sender_id),
                crate::logging::msg_id(&message.message_id)
            );

            let body = self
                .inner
                .storage()
                .get_message(&message.message_id)
                .unwrap_or(None)
                .and_then(|r| r.body);

            let _ = self.ws_tx.send(WsEvent::NewMessage {
                message_id: message.message_id.clone(),
                sender_id: envelope.header.sender_id.clone(),
                message_kind: kind_str.to_string(),
                body,
                timestamp: message.timestamp,
                reply_to: envelope.header.reply_to.clone(),
            });

            // Broadcast any notification the handler created for this message.
            self.broadcast_notification_for(&message.message_id);
        }

        outgoing
    }

    fn on_meta(&mut self, meta: &MetaMessage) -> Vec<Envelope> {
        match meta {
            MetaMessage::Online { peer_id, .. } => {
                let outgoing = self.inner.on_meta(meta);
                if let Ok(Some(peer)) = self.inner.storage().get_peer(peer_id) {
                    if peer.online {
                        let _ = self.ws_tx.send(WsEvent::PeerOnline {
                            peer_id: peer_id.clone(),
                        });
                        crate::tlog!(
                            "sync: peer {} is now online",
                            crate::logging::peer_id(peer_id)
                        );
                    }
                }
                outgoing
            }

            MetaMessage::Ack { .. } => self.inner.on_meta(meta),

            MetaMessage::FriendRequest {
                peer_id: from_peer_id,
                message,
                ..
            } => {
                // Capture pre-call state so we can tell what the handler did.
                let pre_outgoing = self
                    .inner
                    .storage()
                    .find_request_between(&self.my_peer_id, from_peer_id)
                    .unwrap_or(None)
                    .filter(|r| r.status == "pending");
                let pre_incoming = self
                    .inner
                    .storage()
                    .find_request_between(from_peer_id, &self.my_peer_id)
                    .unwrap_or(None)
                    .filter(|r| r.status == "pending");

                let outgoing = self.inner.on_meta(meta);

                if !outgoing.is_empty() {
                    // Mutual auto-accept: handler sent a FriendAccept back.
                    if let Some(req) = pre_outgoing {
                        crate::tlog!(
                            "sync: mutual friend request accepted with {}",
                            crate::logging::peer_id(from_peer_id)
                        );
                        let _ = self.ws_tx.send(WsEvent::FriendRequestAccepted {
                            request_id: req.id,
                            from_peer_id: from_peer_id.clone(),
                        });
                    }
                } else if let Ok(Some(req)) = self
                    .inner
                    .storage()
                    .find_request_between(from_peer_id, &self.my_peer_id)
                {
                    if req.status == "pending" {
                        let _ = self.ws_tx.send(WsEvent::FriendRequestReceived {
                            request_id: req.id,
                            from_peer_id: from_peer_id.clone(),
                            message: message.clone(),
                        });
                        // Notification only for newly inserted requests (not refreshes).
                        if pre_incoming.is_none() {
                            self.broadcast_notification_for(&format!("friend_request_{}", req.id));
                        }
                    }
                }

                outgoing
            }

            MetaMessage::FriendAccept {
                peer_id: from_peer_id,
                ..
            } => {
                // Find the pending outgoing request before the handler marks it accepted.
                let pre_pending_id = self
                    .inner
                    .storage()
                    .list_friend_requests(Some("pending"), Some("outgoing"))
                    .unwrap_or_default()
                    .into_iter()
                    .find(|r| r.to_peer_id == *from_peer_id)
                    .map(|r| r.id);

                let outgoing = self.inner.on_meta(meta);

                if let Some(req_id) = pre_pending_id {
                    crate::tlog!(
                        "sync: friend request accepted by {} (id={})",
                        crate::logging::peer_id(from_peer_id),
                        req_id
                    );
                    let _ = self.ws_tx.send(WsEvent::FriendRequestAccepted {
                        request_id: req_id,
                        from_peer_id: from_peer_id.clone(),
                    });
                }

                outgoing
            }

            MetaMessage::GroupInvite {
                peer_id: from_peer_id,
                group_id,
                message,
                ..
            } => {
                let outgoing = self.inner.on_meta(meta);

                // Find the just-stored pending invite and broadcast events.
                if let Ok(Some(invite)) = self.inner.storage().find_group_invite(
                    group_id,
                    from_peer_id,
                    &self.my_peer_id,
                    "incoming",
                ) {
                    if invite.status == "pending" {
                        let _ = self.ws_tx.send(WsEvent::GroupInviteReceived {
                            invite_id: invite.id,
                            group_id: group_id.clone(),
                            from_peer_id: from_peer_id.clone(),
                            message: message.clone(),
                            created_at: invite.created_at,
                        });
                        self.broadcast_notification_for(&format!("group_invite_{}", invite.id));
                    }
                }

                outgoing
            }

            MetaMessage::GroupInviteAccept {
                peer_id: from_peer_id,
                group_id,
            } => {
                let outgoing = self.inner.on_meta(meta);
                let _ = self.ws_tx.send(WsEvent::GroupMemberJoined {
                    group_id: group_id.clone(),
                    peer_id: from_peer_id.clone(),
                });
                outgoing
            }

            MetaMessage::MessageRequest { .. } => self.inner.on_meta(meta),

            MetaMessage::ProfileRequest { .. } => self.inner.on_meta(meta),

            // Mesh protocol: delegate storage + response-building to the inner
            // handler.  No additional WebSocket events needed.
            MetaMessage::MeshAvailable { .. }
            | MetaMessage::MeshRequest { .. }
            | MetaMessage::MeshDelivery { .. } => {
                // When we receive MeshDelivery, any newly stored messages should
                // be broadcast to connected UI clients.  Detect new inserts by
                // checking whether each delivered envelope's ID is already stored
                // before calling the inner handler.
                if let MetaMessage::MeshDelivery {
                    envelopes,
                    sender_keys,
                    ..
                } = meta
                {
                    let mut new_ids: Vec<String> = Vec::new();
                    for env_val in envelopes {
                        if let Ok(env) = serde_json::from_value::<Envelope>(env_val.clone()) {
                            let id = env.header.message_id.0.clone();
                            if !self.inner.storage().has_message(&id).unwrap_or(true) {
                                new_ids.push(id);
                            }
                        }
                    }
                    let outgoing = self.inner.on_meta(meta);
                    for id in new_ids {
                        if let Ok(Some(row)) = self.inner.storage().get_message(&id) {
                            let _ = self.ws_tx.send(WsEvent::NewMessage {
                                message_id: id.clone(),
                                sender_id: row.sender_id.clone(),
                                message_kind: "public".to_string(),
                                body: row.body.clone(),
                                timestamp: row.timestamp,
                                reply_to: row.reply_to.clone(),
                            });
                        }
                    }
                    // Trigger backfill for any sender keys provided.
                    for (sender_id, key_hex) in sender_keys {
                        let key_bytes = match hex::decode(key_hex) {
                            Ok(b) => b,
                            Err(_) => continue,
                        };
                        let derived = crate::crypto::derive_user_id_from_public_key(&key_bytes);
                        if derived == *sender_id {
                            self.inner.try_backfill_signatures(sender_id, key_hex);
                        }
                    }
                    return outgoing;
                }
                self.inner.on_meta(meta)
            }
        }
    }

    fn on_raw_meta(&mut self, envelope: &Envelope, body: &str) -> Vec<Envelope> {
        let outgoing = self.inner.on_raw_meta(envelope, body);
        // Broadcast any reaction notification the handler created.
        self.broadcast_notification_for(&envelope.header.message_id.0);
        outgoing
    }

    fn take_pending_group_keys(&mut self) -> Vec<(String, Vec<u8>)> {
        self.inner.take_pending_group_keys()
    }

    fn on_group_created(
        &mut self,
        group_id: &str,
        group_key: &[u8; 32],
        creator_id: &str,
        members: &[String],
    ) {
        self.inner
            .on_group_created(group_id, group_key, creator_id, members);
    }
}

impl WebClientHandler {
    /// Look up a notification by `message_id` and broadcast it as a [`WsEvent::Notification`].
    /// No-op if no notification with that `message_id` exists.
    fn broadcast_notification_for(&self, message_id: &str) {
        if let Ok(Some(notif)) = self
            .inner
            .storage()
            .get_notification_by_message_id(message_id)
        {
            let _ = self.ws_tx.send(WsEvent::Notification {
                id: notif.id,
                notification_type: notif.notification_type.clone(),
                message_id: notif.message_id.clone(),
                sender_id: notif.sender_id.clone(),
                created_at: notif.created_at,
            });
        }
    }
}

fn message_kind_str(kind: &MessageKind) -> &'static str {
    match kind {
        MessageKind::Public => "public",
        MessageKind::Direct => "direct",
        MessageKind::FriendGroup => "friend_group",
        MessageKind::Meta => "meta",
        MessageKind::StoreForPeer => "store_for_peer",
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

/// Check which known peers are missing profile data (or whose profiles are
/// stale) and send a `ProfileRequest` to each one via the relay.
///
/// Rate limits:
/// - Peers with no profile response yet: at most once per hour.
/// - Peers that have responded (even with an empty profile): at most once per 24 h.
pub async fn request_missing_profiles(state: &SharedState) -> Result<(), String> {
    const MISSING_INTERVAL_SECS: u64 = 3_600; // 1 hour
    const RESPONDED_INTERVAL_SECS: u64 = 86_400; // 24 hours

    let (keypair, relay_url, db_path) = {
        let st = state.lock().await;
        let relay_url = st
            .relay_url
            .clone()
            .ok_or_else(|| "no relay configured".to_string())?;
        (st.keypair.clone(), relay_url, st.db_path.clone())
    };

    let storage = Storage::open(&db_path).map_err(|e| format!("profile refresh storage: {e}"))?;
    let now = now_secs();

    let peers = storage
        .get_peers_needing_profile_request(now, MISSING_INTERVAL_SECS, RESPONDED_INTERVAL_SECS)
        .map_err(|e| e.to_string())?;

    if peers.is_empty() {
        return Ok(());
    }

    crate::tlog!(
        "profile refresh: requesting profiles for {} peer(s)",
        peers.len()
    );

    for peer_id in &peers {
        let meta_msg = MetaMessage::ProfileRequest {
            peer_id: keypair.id.clone(),
            for_peer_id: peer_id.clone(),
        };

        let payload = match build_meta_payload(&meta_msg) {
            Ok(p) => p,
            Err(e) => {
                crate::tlog!("profile refresh: failed to build payload: {}", e);
                continue;
            }
        };

        let envelope = match build_envelope_from_payload(
            keypair.id.clone(),
            peer_id.clone(),
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
            Ok(e) => e,
            Err(e) => {
                crate::tlog!("profile refresh: failed to build envelope: {}", e);
                continue;
            }
        };

        if let Err(e) = post_envelope(&relay_url, &envelope) {
            crate::tlog!(
                "profile refresh: failed to send request to {}: {}",
                crate::logging::peer_id(peer_id),
                e
            );
        }

        if let Err(e) = storage.record_profile_request_sent(peer_id, now) {
            crate::tlog!(
                "profile refresh: failed to record request for {}: {}",
                crate::logging::peer_id(peer_id),
                e
            );
        }
    }

    Ok(())
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

/// Send `MessageRequest` to each known peer that hasn't been queried within
/// `MESH_QUERY_INTERVAL_SECS`.  Rate-limits queries per-peer using
/// `AppState::last_mesh_query_sent`.
pub async fn send_mesh_queries(state: &SharedState) -> Result<(), String> {
    let (keypair, relay_url, peers, now) = {
        let st = state.lock().await;
        let relay_url = st
            .relay_url
            .clone()
            .ok_or_else(|| "no relay configured".to_string())?;
        let peers = st.storage.list_peers().map_err(|e| e.to_string())?;
        let now = now_secs();
        (st.keypair.clone(), relay_url, peers, now)
    };

    let since = now.saturating_sub(DEFAULT_MESH_WINDOW_SECS);

    for peer in &peers {
        // Check and update the per-peer rate-limit.
        {
            let mut st = state.lock().await;
            let last = st
                .last_mesh_query_sent
                .get(&peer.peer_id)
                .copied()
                .unwrap_or(0);
            if now.saturating_sub(last) < MESH_QUERY_INTERVAL_SECS {
                continue;
            }
            st.last_mesh_query_sent.insert(peer.peer_id.clone(), now);
        }

        let meta_msg = MetaMessage::MessageRequest {
            peer_id: keypair.id.clone(),
            since_timestamp: since,
        };
        let Ok(payload) = build_meta_payload(&meta_msg) else {
            continue;
        };
        let Ok(envelope) = build_envelope_from_payload(
            keypair.id.clone(),
            peer.peer_id.clone(),
            None,
            None,
            now,
            DEFAULT_TTL_SECONDS,
            MessageKind::Meta,
            None,
            None,
            payload,
            &keypair.signing_private_key_hex,
        ) else {
            continue;
        };
        if let Err(e) = post_envelope(&relay_url, &envelope) {
            crate::tlog!(
                "mesh query: failed to send to {}: {}",
                crate::logging::peer_id(&peer.peer_id),
                e
            );
        }
    }

    Ok(())
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
