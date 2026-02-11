//! Protocol-level persistence for received messages.
//!
//! `StorageMessageHandler` implements [`MessageHandler`] and handles all
//! standard protocol-level storage writes: messages, attachments, peer online
//! status, friend request state machine, reactions, and profile updates.
//!
//! It is intentionally storage-agnostic from the caller's point of view: the
//! caller creates a `StorageMessageHandler`, registers it on a
//! [`RelayClient`][crate::client::RelayClient] via `set_handler()`, and calls
//! `sync_inbox()`.  All persistence happens inside the handler callbacks.
//!
//! Application-specific side effects (WebSocket events, push notifications,
//! etc.) belong in a wrapper type that delegates to `StorageMessageHandler`
//! first, then adds its own logic.

use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;

use crate::client::{ClientMessage, MessageHandler};
use crate::crypto::StoredKeypair;
use crate::protocol::{
    build_envelope_from_payload, build_meta_payload, Envelope, MessageKind, MetaMessage,
};
use crate::storage::{
    AttachmentRow, FriendRequestRow, MessageAttachmentRow, MessageRow, NotificationRow, PeerRow,
    ProfileRow, ReactionRow, Storage,
};

/// Default TTL for outgoing envelopes produced by the handler (e.g. auto-accept).
const HANDLER_DEFAULT_TTL_SECONDS: u64 = 3600;

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Handles protocol-level storage writes on behalf of a sync loop.
///
/// Encapsulates persistence for:
/// - `Direct`, `Public`, `FriendGroup`, and `StoreForPeer` messages
/// - Inline attachment data
/// - Peer online status updates
/// - Friend request state machine (including auto-accept on mutual request)
/// - Reaction and profile updates
/// - Notification creation for direct messages, replies, and reactions
pub struct StorageMessageHandler {
    storage: Storage,
    keypair: StoredKeypair,
    my_peer_id: String,
}

impl StorageMessageHandler {
    /// Create a new handler.
    ///
    /// * `storage` — SQLite storage to write into.
    /// * `keypair` — Local keypair; used to sign outgoing envelopes (e.g.
    ///   auto-accept friend requests).
    pub fn new(storage: Storage, keypair: StoredKeypair) -> Self {
        let my_peer_id = keypair.id.clone();
        Self {
            storage,
            keypair,
            my_peer_id,
        }
    }

    /// Borrow the underlying storage.
    pub fn storage(&self) -> &Storage {
        &self.storage
    }

    /// Borrow the underlying storage mutably.
    pub fn storage_mut(&mut self) -> &mut Storage {
        &mut self.storage
    }

    /// Consume the handler and return the underlying storage.
    pub fn into_storage(self) -> Storage {
        self.storage
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    fn message_kind_str(kind: &MessageKind) -> &'static str {
        match kind {
            MessageKind::Public => "public",
            MessageKind::Direct => "direct",
            MessageKind::FriendGroup => "friend_group",
            MessageKind::Meta => "meta",
            MessageKind::StoreForPeer => "store_for_peer",
        }
    }

    fn process_friend_request(
        &mut self,
        from_peer_id: &str,
        signing_public_key: &str,
        encryption_public_key: &str,
        message: Option<String>,
        now: u64,
    ) -> Option<Envelope> {
        crate::tlog!(
            "handler: received friend_request from {}",
            crate::logging::peer_id(from_peer_id)
        );

        // If already friends, treat as duplicate and skip.
        if let Ok(Some(peer)) = self.storage.get_peer(from_peer_id) {
            if peer.is_friend {
                crate::tlog!(
                    "handler: friend request from {} is a duplicate (already friends), ignoring",
                    crate::logging::peer_id(from_peer_id)
                );
                return None;
            }
        }

        // Check if we have a pending OUTGOING request to this peer (mutual / race condition).
        let outgoing = self
            .storage
            .find_request_between(&self.my_peer_id, from_peer_id)
            .unwrap_or(None);

        if let Some(ref out_req) = outgoing {
            if out_req.status == "pending" {
                // Auto-accept: mark our outgoing as accepted, add them as a friend,
                // and return a FriendAccept envelope for the caller to send.
                crate::tlog!(
                    "handler: mutual friend request detected with {} — auto-accepting",
                    crate::logging::peer_id(from_peer_id)
                );
                let _ = self
                    .storage
                    .update_friend_request_status(out_req.id, "accepted");

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
                let _ = self.storage.insert_peer(&peer_row);

                return self.build_friend_accept_envelope(from_peer_id, now);
            }
        }

        // Check for an existing incoming request from this peer.
        let existing = self
            .storage
            .find_request_between(from_peer_id, &self.my_peer_id)
            .unwrap_or(None);

        if let Some(ref ex) = existing {
            if ex.status == "blocked" || ex.status == "ignored" {
                crate::tlog!(
                    "handler: friend request from {} is {}, not resurfacing",
                    crate::logging::peer_id(from_peer_id),
                    ex.status
                );
                return None;
            }
            if ex.status == "pending" {
                let _ = self.storage.refresh_friend_request(
                    ex.id,
                    message.as_deref(),
                    signing_public_key,
                    encryption_public_key,
                );
                return None;
            }
            if ex.status == "accepted" {
                crate::tlog!(
                    "handler: friend request from {} already accepted, skipping",
                    crate::logging::peer_id(from_peer_id)
                );
                return None;
            }
        }

        // No existing request — create a new one.
        let fr_row = FriendRequestRow {
            id: 0,
            from_peer_id: from_peer_id.to_string(),
            to_peer_id: self.my_peer_id.clone(),
            status: "pending".to_string(),
            message: message.clone(),
            from_signing_key: signing_public_key.to_string(),
            from_encryption_key: encryption_public_key.to_string(),
            direction: "incoming".to_string(),
            created_at: now,
            updated_at: now,
        };
        match self.storage.insert_friend_request(&fr_row) {
            Ok(request_id) => {
                // Create notification for the new friend request.
                let notif = NotificationRow {
                    id: 0,
                    notification_type: "friend_request".to_string(),
                    message_id: format!("friend_request_{request_id}"),
                    sender_id: from_peer_id.to_string(),
                    created_at: now,
                    seen: false,
                    read: false,
                };
                let _ = self.storage.insert_notification(&notif);
                crate::tlog!(
                    "handler: stored incoming friend request from {} (id={})",
                    crate::logging::peer_id(from_peer_id),
                    request_id
                );
            }
            Err(e) => {
                crate::tlog!(
                    "handler: failed to store friend request from {}: {}",
                    crate::logging::peer_id(from_peer_id),
                    e
                );
            }
        }
        None
    }

    fn process_friend_accept(
        &mut self,
        from_peer_id: &str,
        signing_public_key: String,
        encryption_public_key: String,
        now: u64,
    ) {
        crate::tlog!(
            "handler: received friend_accept from {}",
            crate::logging::peer_id(from_peer_id)
        );

        if let Ok(Some(peer)) = self.storage.get_peer(from_peer_id) {
            if peer.is_friend {
                crate::tlog!(
                    "handler: friend_accept from {} is a duplicate (already friends), ignoring",
                    crate::logging::peer_id(from_peer_id)
                );
                return;
            }
        }

        let requests = self
            .storage
            .list_friend_requests(Some("pending"), Some("outgoing"))
            .unwrap_or_default();
        if let Some(pending) = requests.iter().find(|r| r.to_peer_id == from_peer_id) {
            let req_id = pending.id;
            let _ = self
                .storage
                .update_friend_request_status(req_id, "accepted");
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
            let _ = self.storage.insert_peer(&peer_row);

            // Archive any incoming request from this peer (race condition).
            if let Ok(Some(incoming)) = self
                .storage
                .find_request_between(from_peer_id, &self.my_peer_id)
            {
                if incoming.status == "pending" {
                    let _ = self
                        .storage
                        .update_friend_request_status(incoming.id, "accepted");
                }
            }

            crate::tlog!(
                "handler: friend request accepted by {} (id={})",
                crate::logging::peer_id(from_peer_id),
                req_id
            );
        } else {
            crate::tlog!(
                "handler: received friend_accept from {} but no matching pending request",
                crate::logging::peer_id(from_peer_id)
            );
        }
    }

    fn build_friend_accept_envelope(&self, to_peer_id: &str, now: u64) -> Option<Envelope> {
        let meta_msg = MetaMessage::FriendAccept {
            peer_id: self.keypair.id.clone(),
            signing_public_key: self.keypair.signing_public_key_hex.clone(),
            encryption_public_key: self.keypair.public_key_hex.clone(),
        };
        let Ok(payload) = build_meta_payload(&meta_msg) else {
            return None;
        };
        let Ok(env) = build_envelope_from_payload(
            self.keypair.id.clone(),
            to_peer_id.to_string(),
            None,
            None,
            now,
            HANDLER_DEFAULT_TTL_SECONDS,
            MessageKind::Meta,
            None,
            None,
            payload,
            &self.keypair.signing_private_key_hex,
        ) else {
            return None;
        };
        crate::tlog!(
            "handler: built auto-accept envelope for {}",
            crate::logging::peer_id(to_peer_id)
        );
        Some(env)
    }
}

impl MessageHandler for StorageMessageHandler {
    fn on_message(&mut self, envelope: &Envelope, message: &ClientMessage) -> Vec<Envelope> {
        let now = now_secs();

        // Deduplication: skip if already stored.
        if self
            .storage
            .has_message(&message.message_id)
            .unwrap_or(true)
        {
            return Vec::new();
        }

        // Update last_seen_online for the sender.
        let _ = self.storage.update_peer_online(
            &envelope.header.sender_id,
            false,
            envelope.header.timestamp,
        );

        let kind_str = Self::message_kind_str(&envelope.header.message_kind);

        // Check for special message types that should not appear in the timeline.
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&message.body) {
            let msg_type = parsed.get("type").and_then(|t| t.as_str());
            if msg_type == Some("tenet.profile") {
                // Store as a profile update rather than a regular message.
                self.store_profile_from_json(
                    &envelope.header.sender_id,
                    &envelope.header.timestamp,
                    &parsed,
                );
                return Vec::new();
            }
            if msg_type == Some("group_key_distribution") {
                // Group key distribution messages are not stored as timeline messages.
                return Vec::new();
            }
        }

        let row = MessageRow {
            message_id: message.message_id.clone(),
            sender_id: message.sender_id.clone(),
            recipient_id: self.my_peer_id.clone(),
            message_kind: kind_str.to_string(),
            group_id: envelope.header.group_id.clone(),
            body: Some(message.body.clone()),
            timestamp: message.timestamp,
            received_at: now,
            ttl_seconds: envelope.header.ttl_seconds,
            is_read: false,
            raw_envelope: None,
            reply_to: envelope.header.reply_to.clone(),
        };

        if self.storage.insert_message(&row).is_ok() {
            // Store any inline attachment data so it can be served locally.
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
                        let _ = self.storage.insert_attachment(&att_row);
                        let _ = self
                            .storage
                            .insert_message_attachment(&MessageAttachmentRow {
                                message_id: message.message_id.clone(),
                                content_hash: att_ref.content_id.0.clone(),
                                filename: att_ref.filename.clone(),
                                position: i as u32,
                            });
                    }
                }
            }

            // Create notification for direct messages and replies to our own messages.
            let notification_type = if row.reply_to.is_some() {
                if let Some(ref parent_id) = row.reply_to {
                    if let Ok(Some(parent_msg)) = self.storage.get_message(parent_id) {
                        if parent_msg.sender_id == self.my_peer_id {
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
                    message_id: message.message_id.clone(),
                    sender_id: message.sender_id.clone(),
                    created_at: now,
                    seen: false,
                    read: false,
                };
                let _ = self.storage.insert_notification(&notif);
            }
        }
        Vec::new()
    }

    fn on_meta(&mut self, meta: &MetaMessage) -> Vec<Envelope> {
        let now = now_secs();
        let mut outgoing = Vec::new();
        match meta {
            MetaMessage::Online { peer_id, timestamp } => {
                let _ = self.storage.update_peer_online(peer_id, true, *timestamp);
            }
            MetaMessage::Ack {
                peer_id,
                online_timestamp,
            } => {
                let _ = self
                    .storage
                    .update_peer_online(peer_id, true, *online_timestamp);
            }
            MetaMessage::FriendRequest {
                peer_id,
                signing_public_key,
                encryption_public_key,
                message,
            } => {
                if let Some(env) = self.process_friend_request(
                    peer_id,
                    signing_public_key,
                    encryption_public_key,
                    message.clone(),
                    now,
                ) {
                    outgoing.push(env);
                }
            }
            MetaMessage::FriendAccept {
                peer_id,
                signing_public_key,
                encryption_public_key,
            } => {
                self.process_friend_accept(
                    peer_id,
                    signing_public_key.clone(),
                    encryption_public_key.clone(),
                    now,
                );
            }
            MetaMessage::MessageRequest { .. } => {
                // Not handled at the storage layer.
            }
        }
        outgoing
    }

    fn on_raw_meta(&mut self, envelope: &Envelope, body: &str) -> Vec<Envelope> {
        let now = now_secs();
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(body) {
            // Reactions: {"target_message_id": "...", "reaction": "upvote"|"downvote"}
            if let (Some(target_id), Some(reaction)) = (
                parsed.get("target_message_id").and_then(|v| v.as_str()),
                parsed.get("reaction").and_then(|v| v.as_str()),
            ) {
                let reaction_row = ReactionRow {
                    message_id: envelope.header.message_id.0.clone(),
                    target_id: target_id.to_string(),
                    sender_id: envelope.header.sender_id.clone(),
                    reaction: reaction.to_string(),
                    timestamp: envelope.header.timestamp,
                };
                match self.storage.upsert_reaction(&reaction_row) {
                    Err(e) => {
                        crate::tlog!(
                            "handler: failed to store reaction from {}: {}",
                            crate::logging::peer_id(&envelope.header.sender_id),
                            e
                        );
                    }
                    Ok(()) => {
                        crate::tlog!(
                            "handler: received {} reaction from {} for message {}",
                            reaction,
                            crate::logging::peer_id(&envelope.header.sender_id),
                            crate::logging::msg_id(target_id)
                        );

                        // Create a notification if the reaction is to one of our own messages.
                        if let Ok(Some(target_msg)) = self.storage.get_message(target_id) {
                            if target_msg.sender_id == self.my_peer_id {
                                let notif = NotificationRow {
                                    id: 0,
                                    notification_type: "reaction".to_string(),
                                    message_id: envelope.header.message_id.0.clone(),
                                    sender_id: envelope.header.sender_id.clone(),
                                    created_at: now,
                                    seen: false,
                                    read: false,
                                };
                                let _ = self.storage.insert_notification(&notif);
                            }
                        }
                    }
                }
                return Vec::new();
            }

            // Profile updates embedded in raw meta (legacy / forwarded).
            if parsed.get("type").and_then(|t| t.as_str()) == Some("tenet.profile") {
                self.store_profile_from_json(
                    &envelope.header.sender_id,
                    &envelope.header.timestamp,
                    &parsed,
                );
            }
        }
        Vec::new()
    }
}

impl StorageMessageHandler {
    fn store_profile_from_json(
        &mut self,
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

        match self.storage.upsert_profile_if_newer(&profile_row) {
            Ok(true) => {
                crate::tlog!(
                    "handler: updated profile for {}",
                    crate::logging::peer_id(&profile_user_id)
                );
                // Mirror display name into the peer row if it changed.
                if let Some(ref display_name) = profile_row.display_name {
                    if let Ok(Some(mut peer)) = self.storage.get_peer(&profile_user_id) {
                        if peer.display_name.as_deref() != Some(display_name) {
                            peer.display_name = Some(display_name.clone());
                            let _ = self.storage.insert_peer(&peer);
                        }
                    }
                }
            }
            Ok(false) => {}
            Err(e) => {
                crate::tlog!(
                    "handler: failed to update profile for {}: {}",
                    crate::logging::peer_id(&profile_user_id),
                    e
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::generate_keypair;
    use crate::protocol::{build_plaintext_envelope, MessageKind, MetaMessage};
    use crate::storage::Storage;

    fn open_memory_storage() -> Storage {
        Storage::open_in_memory(std::path::Path::new("/tmp")).expect("in-memory storage")
    }

    #[test]
    fn handler_stores_direct_message() {
        let alice = generate_keypair();
        let bob = generate_keypair();

        let mut storage = open_memory_storage();
        // Register Alice as a peer in Bob's storage.
        let peer_row = PeerRow {
            peer_id: alice.id.clone(),
            display_name: None,
            signing_public_key: alice.signing_public_key_hex.clone(),
            encryption_public_key: Some(alice.public_key_hex.clone()),
            added_at: 0,
            is_friend: true,
            last_seen_online: None,
            online: false,
        };
        storage.insert_peer(&peer_row).expect("insert peer");

        let mut handler = StorageMessageHandler::new(storage, bob.clone());

        // Build a simple plaintext direct message from Alice to Bob.
        let salt = [0u8; 16];
        let envelope = build_plaintext_envelope(
            &alice.id,
            &bob.id,
            None,
            None,
            1_700_000_000,
            3600,
            MessageKind::Direct,
            None,
            None,
            "hello from alice",
            salt,
            &alice.signing_private_key_hex,
        )
        .expect("build envelope");

        let message = ClientMessage {
            message_id: envelope.header.message_id.0.clone(),
            sender_id: alice.id.clone(),
            timestamp: envelope.header.timestamp,
            body: "hello from alice".to_string(),
        };

        handler.on_message(&envelope, &message);

        let stored = handler
            .storage()
            .get_message(&message.message_id)
            .expect("get message")
            .expect("message exists");
        assert_eq!(stored.sender_id, alice.id);
        assert_eq!(stored.body, Some("hello from alice".to_string()));

        // Check that a direct_message notification was created.
        let notifs = handler
            .storage()
            .list_notifications(false, 100)
            .expect("list notifications");
        assert_eq!(notifs.len(), 1);
        assert_eq!(notifs[0].notification_type, "direct_message");
    }

    #[test]
    fn handler_deduplicates_messages() {
        let alice = generate_keypair();
        let bob = generate_keypair();

        let storage = open_memory_storage();
        let mut handler = StorageMessageHandler::new(storage, bob.clone());

        let salt = [0u8; 16];
        let envelope = build_plaintext_envelope(
            &alice.id,
            &bob.id,
            None,
            None,
            1_700_000_000,
            3600,
            MessageKind::Direct,
            None,
            None,
            "dedup test",
            salt,
            &alice.signing_private_key_hex,
        )
        .expect("build envelope");

        let message = ClientMessage {
            message_id: envelope.header.message_id.0.clone(),
            sender_id: alice.id.clone(),
            timestamp: envelope.header.timestamp,
            body: "dedup test".to_string(),
        };

        handler.on_message(&envelope, &message);
        handler.on_message(&envelope, &message); // second call should be a no-op

        let all_msgs = handler
            .storage()
            .list_messages(None, None, None, 100)
            .expect("list messages");
        assert_eq!(all_msgs.len(), 1, "should only store message once");
    }

    #[test]
    fn handler_updates_peer_online_on_meta() {
        let alice = generate_keypair();
        let bob = generate_keypair();

        let mut storage = open_memory_storage();
        let peer_row = PeerRow {
            peer_id: alice.id.clone(),
            display_name: None,
            signing_public_key: alice.signing_public_key_hex.clone(),
            encryption_public_key: None,
            added_at: 0,
            is_friend: false,
            last_seen_online: None,
            online: false,
        };
        storage.insert_peer(&peer_row).expect("insert peer");

        let mut handler = StorageMessageHandler::new(storage, bob.clone());

        let meta = MetaMessage::Online {
            peer_id: alice.id.clone(),
            timestamp: 1_700_000_100,
        };
        handler.on_meta(&meta);

        let peer = handler
            .storage()
            .get_peer(&alice.id)
            .expect("get peer")
            .expect("peer exists");
        assert!(peer.online);
        assert_eq!(peer.last_seen_online, Some(1_700_000_100));
    }

    #[test]
    fn handler_stores_reaction_and_creates_notification() {
        let alice = generate_keypair();
        let bob = generate_keypair();

        let mut storage = open_memory_storage();
        let peer_row = PeerRow {
            peer_id: alice.id.clone(),
            display_name: None,
            signing_public_key: alice.signing_public_key_hex.clone(),
            encryption_public_key: None,
            added_at: 0,
            is_friend: true,
            last_seen_online: None,
            online: false,
        };
        storage.insert_peer(&peer_row).expect("insert peer");

        // Pre-insert a message authored by Bob.
        let target_msg = MessageRow {
            message_id: "target-msg-001".to_string(),
            sender_id: bob.id.clone(),
            recipient_id: "*".to_string(),
            message_kind: "public".to_string(),
            group_id: None,
            body: Some("a public post".to_string()),
            timestamp: 1_700_000_000,
            received_at: 1_700_000_000,
            ttl_seconds: 3600,
            is_read: false,
            raw_envelope: None,
            reply_to: None,
        };
        storage.insert_message(&target_msg).expect("insert target");

        let mut handler = StorageMessageHandler::new(storage, bob.clone());

        // Build a raw-meta reaction envelope from Alice.
        let reaction_body = r#"{"target_message_id":"target-msg-001","reaction":"upvote"}"#;
        let salt = [0u8; 16];
        let envelope = build_plaintext_envelope(
            &alice.id,
            "*",
            None,
            None,
            1_700_000_200,
            3600,
            MessageKind::Meta,
            None,
            None,
            reaction_body,
            salt,
            &alice.signing_private_key_hex,
        )
        .expect("build reaction envelope");

        handler.on_raw_meta(&envelope, reaction_body);

        // Reaction should be stored.
        let reactions = handler
            .storage()
            .list_reactions("target-msg-001")
            .expect("list reactions");
        assert_eq!(reactions.len(), 1);
        assert_eq!(reactions[0].reaction, "upvote");

        // Notification should be created (Alice reacted to Bob's own message).
        let notifs = handler
            .storage()
            .list_notifications(false, 100)
            .expect("list notifications");
        assert_eq!(notifs.len(), 1);
        assert_eq!(notifs[0].notification_type, "reaction");
    }

    #[test]
    fn sync_event_outcome_message_variant() {
        use crate::client::SyncEventOutcome;
        let msg = ClientMessage {
            message_id: "id".to_string(),
            sender_id: "s".to_string(),
            timestamp: 0,
            body: "body".to_string(),
        };
        let outcome = SyncEventOutcome::Message(msg.clone());
        if let SyncEventOutcome::Message(m) = outcome {
            assert_eq!(m.body, "body");
        } else {
            panic!("expected Message variant");
        }
    }
}
