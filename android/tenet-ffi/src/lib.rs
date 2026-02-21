// Tenet FFI — UniFFI-based Rust/Kotlin bridge for the Android client.
//
// Design constraints:
//  * All public functions are synchronous.  Kotlin callers dispatch to
//    Dispatchers.IO so the main thread is never blocked.
//  * TenetClient wraps the mutable state behind a std::sync::Mutex, making
//    concurrent Kotlin coroutine calls safe.
//  * Sync opens a *second* Storage connection (SQLite WAL mode allows this)
//    to avoid holding the main lock across network I/O.

mod types;

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use rand::RngCore;
use sha2::{Digest, Sha256};

use tenet::client::{ClientConfig, ClientEncryption, RelayClient};
use tenet::crypto::StoredKeypair;
use tenet::identity::{resolve_identity, store_relay_for_identity};
use tenet::message_handler::StorageMessageHandler;
use tenet::protocol::{
    build_envelope_from_payload, build_group_message_payload, build_plaintext_envelope,
    MessageKind,
};
use tenet::relay_transport::post_envelope;
use tenet::storage::{
    AttachmentRow, FriendRequestRow, GroupInviteRow, GroupMemberRow, GroupRow, NotificationRow,
    PeerRow, ProfileRow, ReactionRow, Storage,
};

pub use types::{
    FfiConversation, FfiFriendRequest, FfiGroup, FfiGroupInvite, FfiGroupMember, FfiMessage,
    FfiNotification, FfiPeer, FfiProfile, FfiReactionSummary, FfiSyncResult,
};

// HPKE binding strings — must match the web client constants so messages
// created by the web client can be decrypted by the Android client and
// vice-versa.
const FFI_HPKE_INFO: &[u8] = b"tenet-web-v1";
const FFI_PAYLOAD_AAD: &[u8] = b"tenet-payload-v1";
const DEFAULT_TTL_SECONDS: u64 = 86_400; // 24 hours

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum TenetError {
    Init(String),
    Sync(String),
    Send(String),
    Storage(String),
    NotFound(String),
    InvalidKey(String),
}

impl std::fmt::Display for TenetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TenetError::Init(msg) => write!(f, "init error: {msg}"),
            TenetError::Sync(msg) => write!(f, "sync error: {msg}"),
            TenetError::Send(msg) => write!(f, "send error: {msg}"),
            TenetError::Storage(msg) => write!(f, "storage error: {msg}"),
            TenetError::NotFound(msg) => write!(f, "not found: {msg}"),
            TenetError::InvalidKey(msg) => write!(f, "invalid key: {msg}"),
        }
    }
}

impl std::error::Error for TenetError {}

// ---------------------------------------------------------------------------
// Internal state
// ---------------------------------------------------------------------------

struct TenetClientInner {
    keypair: StoredKeypair,
    relay_url: String,
    storage: Storage,
    db_path: PathBuf,
}

// ---------------------------------------------------------------------------
// Public TenetClient — the UniFFI interface object
// ---------------------------------------------------------------------------

pub struct TenetClient {
    inner: Mutex<TenetClientInner>,
}

impl TenetClient {
    /// Initialize (or load) an identity in `data_dir` and configure the relay.
    pub fn new(data_dir: String, relay_url: String) -> Result<Self, TenetError> {
        let path = Path::new(&data_dir);
        let resolved =
            resolve_identity(path, None).map_err(|e| TenetError::Init(e.to_string()))?;
        store_relay_for_identity(&resolved.storage, &relay_url)
            .map_err(|e| TenetError::Init(e.to_string()))?;
        let db_path = tenet::storage::db_path(&resolved.identity_dir);
        Ok(Self {
            inner: Mutex::new(TenetClientInner {
                keypair: resolved.keypair,
                relay_url,
                storage: resolved.storage,
                db_path,
            }),
        })
    }

    pub fn my_peer_id(&self) -> String {
        self.inner.lock().unwrap().keypair.id.clone()
    }

    pub fn relay_url(&self) -> String {
        self.inner.lock().unwrap().relay_url.clone()
    }

    // -------------------------------------------------------------------------
    // Messages
    // -------------------------------------------------------------------------

    pub fn get_message(&self, message_id: String) -> Result<Option<FfiMessage>, TenetError> {
        let inner = self.inner.lock().unwrap();
        let row = inner
            .storage
            .get_message(&message_id)
            .map_err(|e| TenetError::Storage(e.to_string()))?;
        Ok(row.map(row_to_ffi))
    }

    pub fn list_messages(
        &self,
        kind: Option<String>,
        limit: u32,
        before_ts: Option<i64>,
    ) -> Result<Vec<FfiMessage>, TenetError> {
        let inner = self.inner.lock().unwrap();
        let rows = inner
            .storage
            .list_messages(kind.as_deref(), None, before_ts.map(|t| t as u64), limit)
            .map_err(|e| TenetError::Storage(e.to_string()))?;
        Ok(rows.into_iter().map(row_to_ffi).collect())
    }

    pub fn mark_read(&self, message_id: String) -> Result<(), TenetError> {
        self.inner
            .lock()
            .unwrap()
            .storage
            .mark_message_read(&message_id)
            .map_err(|e| TenetError::Storage(e.to_string()))?;
        Ok(())
    }

    // -------------------------------------------------------------------------
    // Direct messages
    // -------------------------------------------------------------------------

    pub fn send_direct(&self, recipient_id: String, body: String) -> Result<(), TenetError> {
        let inner = self.inner.lock().unwrap();
        let peer = inner
            .storage
            .get_peer(&recipient_id)
            .map_err(|e| TenetError::Storage(e.to_string()))?
            .ok_or_else(|| TenetError::NotFound(format!("peer: {recipient_id}")))?;
        let enc_key = peer
            .encryption_public_key
            .as_deref()
            .unwrap_or(&peer.signing_public_key)
            .to_string();
        let relay = RelayClient::new(inner.keypair.clone(), client_config(&inner.relay_url));
        let envelope = relay
            .send_message(&recipient_id, &enc_key, &body)
            .map_err(|e| TenetError::Send(e.to_string()))?;
        let now = now_secs();
        let _ = inner.storage.insert_message(&make_msg_row(
            &envelope,
            &inner.keypair.id,
            &recipient_id,
            "direct",
            None,
            Some(body),
            now,
            None,
        ));
        Ok(())
    }

    pub fn list_direct_messages(
        &self,
        peer_id: String,
        limit: u32,
        before_ts: Option<i64>,
    ) -> Result<Vec<FfiMessage>, TenetError> {
        let inner = self.inner.lock().unwrap();
        let my_id = inner.keypair.id.clone();
        // Fetch a generous batch then filter in-process (no native peer-pair index in Phase 2).
        let rows = inner
            .storage
            .list_messages(
                Some("direct"),
                None,
                before_ts.map(|t| t as u64),
                (limit * 4).max(200),
            )
            .map_err(|e| TenetError::Storage(e.to_string()))?;
        Ok(rows
            .into_iter()
            .filter(|r| {
                (r.sender_id == my_id && r.recipient_id == peer_id)
                    || (r.sender_id == peer_id && r.recipient_id == my_id)
            })
            .take(limit as usize)
            .map(row_to_ffi)
            .collect())
    }

    // -------------------------------------------------------------------------
    // Public / group messages
    // -------------------------------------------------------------------------

    pub fn send_public(&self, body: String) -> Result<(), TenetError> {
        let inner = self.inner.lock().unwrap();
        let peers = inner
            .storage
            .list_peers()
            .map_err(|e| TenetError::Send(e.to_string()))?;
        let mut relay = RelayClient::new(inner.keypair.clone(), client_config(&inner.relay_url));
        for p in &peers {
            relay.add_peer_with_encryption(
                p.peer_id.clone(),
                p.signing_public_key.clone(),
                p.encryption_public_key
                    .clone()
                    .unwrap_or_else(|| p.signing_public_key.clone()),
            );
        }
        let envelope = relay
            .send_public_message(&body)
            .map_err(|e| TenetError::Send(e.to_string()))?;
        let now = now_secs();
        let _ = inner.storage.insert_message(&make_msg_row(
            &envelope,
            &inner.keypair.id,
            "",
            "public",
            None,
            Some(body),
            now,
            None,
        ));
        Ok(())
    }

    pub fn send_group(&self, group_id: String, body: String) -> Result<(), TenetError> {
        let inner = self.inner.lock().unwrap();
        let group = inner
            .storage
            .get_group(&group_id)
            .map_err(|e| TenetError::Storage(e.to_string()))?
            .ok_or_else(|| TenetError::NotFound(format!("group: {group_id}")))?;
        let group_key: [u8; 32] = group
            .group_key
            .try_into()
            .map_err(|_| TenetError::Send("invalid group key length".to_string()))?;
        let now = now_secs();
        let payload = build_group_message_payload(body.as_bytes(), &group_key, group_id.as_bytes())
            .map_err(|e| TenetError::Send(e.to_string()))?;
        let envelope = build_envelope_from_payload(
            &inner.keypair.id,
            "*",
            None,
            None,
            now,
            DEFAULT_TTL_SECONDS,
            MessageKind::FriendGroup,
            Some(group_id.clone()),
            None,
            payload,
            &inner.keypair.signing_private_key_hex,
        )
        .map_err(|e| TenetError::Send(e.to_string()))?;
        post_envelope(&inner.relay_url, &envelope).map_err(|e| TenetError::Send(e.to_string()))?;
        let _ = inner.storage.insert_message(&make_msg_row(
            &envelope,
            &inner.keypair.id,
            "*",
            "friend_group",
            Some(group_id),
            Some(body),
            now,
            None,
        ));
        Ok(())
    }

    // -------------------------------------------------------------------------
    // Replies
    // -------------------------------------------------------------------------

    pub fn reply_to(&self, parent_message_id: String, body: String) -> Result<(), TenetError> {
        let inner = self.inner.lock().unwrap();
        let parent = inner
            .storage
            .get_message(&parent_message_id)
            .map_err(|e| TenetError::Storage(e.to_string()))?
            .ok_or_else(|| TenetError::NotFound(format!("message: {parent_message_id}")))?;
        let now = now_secs();
        let kind = parent.message_kind.clone();
        let gid = parent.group_id.clone();

        let envelope = match kind.as_str() {
            "public" => {
                let mut salt = [0u8; 16];
                rand::rngs::OsRng.fill_bytes(&mut salt);
                build_plaintext_envelope(
                    &inner.keypair.id,
                    "*",
                    None,
                    None,
                    now,
                    DEFAULT_TTL_SECONDS,
                    MessageKind::Public,
                    None,
                    Some(parent_message_id.clone()),
                    &body,
                    salt,
                    &inner.keypair.signing_private_key_hex,
                )
                .map_err(|e| TenetError::Send(e.to_string()))?
            }
            "friend_group" => {
                let group_id = gid
                    .clone()
                    .ok_or_else(|| TenetError::Send("group_id missing".to_string()))?;
                let group = inner
                    .storage
                    .get_group(&group_id)
                    .map_err(|e| TenetError::Storage(e.to_string()))?
                    .ok_or_else(|| TenetError::NotFound(format!("group: {group_id}")))?;
                let gk: [u8; 32] = group
                    .group_key
                    .try_into()
                    .map_err(|_| TenetError::Send("invalid group key length".to_string()))?;
                let payload =
                    build_group_message_payload(body.as_bytes(), &gk, group_id.as_bytes())
                        .map_err(|e| TenetError::Send(e.to_string()))?;
                build_envelope_from_payload(
                    &inner.keypair.id,
                    "*",
                    None,
                    None,
                    now,
                    DEFAULT_TTL_SECONDS,
                    MessageKind::FriendGroup,
                    Some(group_id),
                    Some(parent_message_id.clone()),
                    payload,
                    &inner.keypair.signing_private_key_hex,
                )
                .map_err(|e| TenetError::Send(e.to_string()))?
            }
            other => {
                return Err(TenetError::Send(format!(
                    "cannot reply to kind: {other}"
                )))
            }
        };

        post_envelope(&inner.relay_url, &envelope).map_err(|e| TenetError::Send(e.to_string()))?;
        let recip = envelope.header.recipient_id.clone();
        let _ = inner.storage.insert_message(&make_msg_row(
            &envelope,
            &inner.keypair.id,
            &recip,
            &kind,
            gid,
            Some(body),
            now,
            Some(parent_message_id),
        ));
        Ok(())
    }

    pub fn list_replies(
        &self,
        parent_message_id: String,
        limit: u32,
        before_ts: Option<i64>,
    ) -> Result<Vec<FfiMessage>, TenetError> {
        let inner = self.inner.lock().unwrap();
        let rows = inner
            .storage
            .list_replies(&parent_message_id, before_ts.map(|t| t as u64), limit)
            .map_err(|e| TenetError::Storage(e.to_string()))?;
        Ok(rows.into_iter().map(row_to_ffi).collect())
    }

    // -------------------------------------------------------------------------
    // Reactions
    // -------------------------------------------------------------------------

    pub fn react(
        &self,
        target_message_id: String,
        reaction: String,
    ) -> Result<FfiReactionSummary, TenetError> {
        if reaction != "upvote" && reaction != "downvote" {
            return Err(TenetError::Send(
                "reaction must be 'upvote' or 'downvote'".to_string(),
            ));
        }
        let inner = self.inner.lock().unwrap();
        let now = now_secs();

        let reaction_body = serde_json::json!({
            "target_message_id": &target_message_id,
            "reaction": &reaction,
            "timestamp": now,
        })
        .to_string();
        let mut salt = [0u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut salt);

        if let Ok(envelope) = build_plaintext_envelope(
            &inner.keypair.id,
            "*",
            None,
            None,
            now,
            DEFAULT_TTL_SECONDS,
            MessageKind::Meta,
            None,
            None,
            &reaction_body,
            salt,
            &inner.keypair.signing_private_key_hex,
        ) {
            let row = ReactionRow {
                message_id: envelope.header.message_id.0.clone(),
                target_id: target_message_id.clone(),
                sender_id: inner.keypair.id.clone(),
                reaction: reaction.clone(),
                timestamp: now,
            };
            let _ = inner.storage.upsert_reaction(&row);
            let _ = post_envelope(&inner.relay_url, &envelope);
        }

        reaction_summary(&inner.storage, &target_message_id, &inner.keypair.id)
    }

    pub fn unreact(&self, target_message_id: String) -> Result<FfiReactionSummary, TenetError> {
        let inner = self.inner.lock().unwrap();
        let my_id = inner.keypair.id.clone();
        let _ = inner.storage.delete_reaction(&target_message_id, &my_id);
        reaction_summary(&inner.storage, &target_message_id, &my_id)
    }

    pub fn get_reactions(
        &self,
        target_message_id: String,
    ) -> Result<FfiReactionSummary, TenetError> {
        let inner = self.inner.lock().unwrap();
        let my_id = inner.keypair.id.clone();
        reaction_summary(&inner.storage, &target_message_id, &my_id)
    }

    // -------------------------------------------------------------------------
    // Attachments
    // -------------------------------------------------------------------------

    pub fn upload_attachment(
        &self,
        data: Vec<u8>,
        content_type: String,
    ) -> Result<String, TenetError> {
        let inner = self.inner.lock().unwrap();
        let now = now_secs();
        let digest = Sha256::digest(&data);
        let content_hash =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest.as_slice());
        let row = AttachmentRow {
            content_hash: content_hash.clone(),
            content_type,
            size_bytes: data.len() as u64,
            data,
            created_at: now,
        };
        inner
            .storage
            .insert_attachment(&row)
            .map_err(|e| TenetError::Storage(e.to_string()))?;
        Ok(content_hash)
    }

    pub fn download_attachment(&self, content_hash: String) -> Result<Vec<u8>, TenetError> {
        let inner = self.inner.lock().unwrap();
        inner
            .storage
            .get_attachment(&content_hash)
            .map_err(|e| TenetError::Storage(e.to_string()))?
            .ok_or_else(|| TenetError::NotFound(format!("attachment: {content_hash}")))
            .map(|r| r.data)
    }

    // -------------------------------------------------------------------------
    // Sync
    // -------------------------------------------------------------------------

    pub fn sync(&self) -> Result<FfiSyncResult, TenetError> {
        let (keypair, relay_url, db_path, peers, groups) = {
            let inner = self.inner.lock().unwrap();
            let peers = inner
                .storage
                .list_peers()
                .map_err(|e| TenetError::Sync(e.to_string()))?;
            let groups = inner
                .storage
                .list_groups()
                .map_err(|e| TenetError::Sync(e.to_string()))?;
            (
                inner.keypair.clone(),
                inner.relay_url.clone(),
                inner.db_path.clone(),
                peers,
                groups,
            )
        };

        let handler_storage =
            Storage::open(&db_path).map_err(|e| TenetError::Sync(e.to_string()))?;
        let handler = StorageMessageHandler::new_with_crypto(
            handler_storage,
            keypair.clone(),
            FFI_HPKE_INFO.to_vec(),
            FFI_PAYLOAD_AAD.to_vec(),
        );

        let mut relay = RelayClient::new(keypair, client_config(&relay_url));
        for p in &peers {
            relay.add_peer_with_encryption(
                p.peer_id.clone(),
                p.signing_public_key.clone(),
                p.encryption_public_key
                    .clone()
                    .unwrap_or_else(|| p.signing_public_key.clone()),
            );
        }
        for g in &groups {
            relay
                .group_manager_mut()
                .add_group_key(g.group_id.clone(), g.group_key.clone());
        }
        relay.set_handler(Box::new(handler));

        let outcome = relay
            .sync_inbox(None)
            .map_err(|e| TenetError::Sync(e.to_string()))?;

        Ok(FfiSyncResult {
            fetched: outcome.fetched as u32,
            new_messages: outcome.messages.len() as u32,
            errors: outcome.errors,
        })
    }

    // -------------------------------------------------------------------------
    // Conversations
    // -------------------------------------------------------------------------

    pub fn list_conversations(&self) -> Result<Vec<FfiConversation>, TenetError> {
        let inner = self.inner.lock().unwrap();
        let my_id = inner.keypair.id.clone();
        let summaries = inner
            .storage
            .list_conversations(&my_id)
            .map_err(|e| TenetError::Storage(e.to_string()))?;

        let mut result = Vec::with_capacity(summaries.len());
        for s in summaries {
            let display_name = inner
                .storage
                .get_peer(&s.peer_id)
                .ok()
                .flatten()
                .and_then(|p| p.display_name);
            result.push(FfiConversation {
                peer_id: s.peer_id,
                display_name,
                last_message: s.last_message,
                last_timestamp: s.last_timestamp as i64,
                unread_count: s.unread_count,
            });
        }
        Ok(result)
    }

    // -------------------------------------------------------------------------
    // Peers
    // -------------------------------------------------------------------------

    pub fn list_peers(&self) -> Result<Vec<FfiPeer>, TenetError> {
        let inner = self.inner.lock().unwrap();
        Ok(inner
            .storage
            .list_peers()
            .map_err(|e| TenetError::Storage(e.to_string()))?
            .into_iter()
            .map(peer_row_to_ffi)
            .collect())
    }

    pub fn get_peer(&self, peer_id: String) -> Result<Option<FfiPeer>, TenetError> {
        let inner = self.inner.lock().unwrap();
        Ok(inner
            .storage
            .get_peer(&peer_id)
            .map_err(|e| TenetError::Storage(e.to_string()))?
            .map(peer_row_to_ffi))
    }

    pub fn add_peer(
        &self,
        peer_id: String,
        display_name: Option<String>,
        signing_public_key_hex: String,
    ) -> Result<(), TenetError> {
        tenet::crypto::validate_hex_key(&signing_public_key_hex, 32, "signing_public_key_hex")
            .map_err(TenetError::InvalidKey)?;
        let inner = self.inner.lock().unwrap();
        let now = now_secs();
        inner
            .storage
            .insert_peer(&PeerRow {
                peer_id,
                display_name,
                signing_public_key: signing_public_key_hex,
                encryption_public_key: None,
                added_at: now,
                is_friend: false,
                last_seen_online: None,
                online: false,
                last_profile_requested_at: None,
                last_profile_responded_at: None,
                is_blocked: false,
                is_muted: false,
                blocked_at: None,
                muted_at: None,
            })
            .map_err(|e| TenetError::Storage(e.to_string()))
    }

    pub fn remove_peer(&self, peer_id: String) -> Result<(), TenetError> {
        self.inner
            .lock()
            .unwrap()
            .storage
            .delete_peer(&peer_id)
            .map_err(|e| TenetError::Storage(e.to_string()))?;
        Ok(())
    }

    pub fn block_peer(&self, peer_id: String) -> Result<(), TenetError> {
        self.inner
            .lock()
            .unwrap()
            .storage
            .set_peer_blocked(&peer_id, true)
            .map_err(|e| TenetError::Storage(e.to_string()))
    }

    pub fn unblock_peer(&self, peer_id: String) -> Result<(), TenetError> {
        self.inner
            .lock()
            .unwrap()
            .storage
            .set_peer_blocked(&peer_id, false)
            .map_err(|e| TenetError::Storage(e.to_string()))
    }

    pub fn mute_peer(&self, peer_id: String) -> Result<(), TenetError> {
        self.inner
            .lock()
            .unwrap()
            .storage
            .set_peer_muted(&peer_id, true)
            .map_err(|e| TenetError::Storage(e.to_string()))
    }

    pub fn unmute_peer(&self, peer_id: String) -> Result<(), TenetError> {
        self.inner
            .lock()
            .unwrap()
            .storage
            .set_peer_muted(&peer_id, false)
            .map_err(|e| TenetError::Storage(e.to_string()))
    }

    // -------------------------------------------------------------------------
    // Friends
    // -------------------------------------------------------------------------

    pub fn send_friend_request(
        &self,
        peer_id: String,
        message: Option<String>,
    ) -> Result<(), TenetError> {
        let inner = self.inner.lock().unwrap();
        let my_id = inner.keypair.id.clone();

        // Look up the peer so we have keys to fill in the request row.
        let peer = inner
            .storage
            .get_peer(&peer_id)
            .map_err(|e| TenetError::Storage(e.to_string()))?
            .ok_or_else(|| TenetError::NotFound(format!("peer: {peer_id}")))?;

        let now = now_secs();

        // Upsert: refresh if a request already exists, otherwise insert fresh.
        if let Ok(Some(existing)) = inner.storage.find_request_between(&my_id, &peer_id) {
            inner
                .storage
                .refresh_friend_request(
                    existing.id,
                    message.as_deref(),
                    &inner.keypair.signing_public_key_hex,
                    &inner.keypair.public_key_hex,
                )
                .map_err(|e| TenetError::Storage(e.to_string()))?;
        } else {
            inner
                .storage
                .insert_friend_request(&FriendRequestRow {
                    id: 0,
                    from_peer_id: my_id.clone(),
                    to_peer_id: peer_id.clone(),
                    status: "pending".to_string(),
                    message: message.clone(),
                    from_signing_key: inner.keypair.signing_public_key_hex.clone(),
                    from_encryption_key: inner.keypair.public_key_hex.clone(),
                    direction: "outgoing".to_string(),
                    created_at: now,
                    updated_at: now,
                })
                .map_err(|e| TenetError::Storage(e.to_string()))?;
        }

        // Best-effort: send an encrypted notification to the peer.
        let notify_body = serde_json::json!({
            "type": "friend_request",
            "from": &my_id,
            "message": message,
            "signing_key": &inner.keypair.signing_public_key_hex,
            "encryption_key": &inner.keypair.public_key_hex,
        })
        .to_string();
        let enc_key_hex = peer
            .encryption_public_key
            .as_deref()
            .unwrap_or(&peer.signing_public_key)
            .to_string();
        let relay = RelayClient::new(inner.keypair.clone(), client_config(&inner.relay_url));
        let _ = relay.send_message(&peer_id, &enc_key_hex, &notify_body);

        Ok(())
    }

    pub fn list_friend_requests(&self) -> Result<Vec<FfiFriendRequest>, TenetError> {
        let inner = self.inner.lock().unwrap();
        let rows = inner
            .storage
            .list_friend_requests(None, None)
            .map_err(|e| TenetError::Storage(e.to_string()))?;
        Ok(rows.into_iter().map(friend_request_to_ffi).collect())
    }

    pub fn accept_friend_request(&self, request_id: i64) -> Result<(), TenetError> {
        let inner = self.inner.lock().unwrap();
        let req = inner
            .storage
            .get_friend_request(request_id)
            .map_err(|e| TenetError::Storage(e.to_string()))?
            .ok_or_else(|| TenetError::NotFound(format!("friend_request: {request_id}")))?;

        inner
            .storage
            .update_friend_request_status(request_id, "accepted")
            .map_err(|e| TenetError::Storage(e.to_string()))?;

        // Mark the sending peer as a friend by re-inserting with is_friend=true.
        // Use INSERT OR REPLACE to update all fields atomically.
        let from_id = req.from_peer_id.clone();
        let now = now_secs();
        if let Ok(Some(existing_peer)) = inner.storage.get_peer(&from_id) {
            inner
                .storage
                .insert_peer(&PeerRow {
                    is_friend: true,
                    ..existing_peer
                })
                .map_err(|e| TenetError::Storage(e.to_string()))?;
        } else {
            // Peer not in DB yet — add them using keys from the request.
            inner
                .storage
                .insert_peer(&PeerRow {
                    peer_id: from_id,
                    display_name: None,
                    signing_public_key: req.from_signing_key,
                    encryption_public_key: Some(req.from_encryption_key),
                    added_at: now,
                    is_friend: true,
                    last_seen_online: None,
                    online: false,
                    last_profile_requested_at: None,
                    last_profile_responded_at: None,
                    is_blocked: false,
                    is_muted: false,
                    blocked_at: None,
                    muted_at: None,
                })
                .map_err(|e| TenetError::Storage(e.to_string()))?;
        }

        Ok(())
    }

    pub fn ignore_friend_request(&self, request_id: i64) -> Result<(), TenetError> {
        self.inner
            .lock()
            .unwrap()
            .storage
            .update_friend_request_status(request_id, "ignored")
            .map_err(|e| TenetError::Storage(e.to_string()))?;
        Ok(())
    }

    pub fn block_friend_request(&self, request_id: i64) -> Result<(), TenetError> {
        let inner = self.inner.lock().unwrap();
        let req = inner
            .storage
            .get_friend_request(request_id)
            .map_err(|e| TenetError::Storage(e.to_string()))?
            .ok_or_else(|| TenetError::NotFound(format!("friend_request: {request_id}")))?;

        inner
            .storage
            .update_friend_request_status(request_id, "blocked")
            .map_err(|e| TenetError::Storage(e.to_string()))?;

        // Also block the peer at the peer level.
        inner
            .storage
            .set_peer_blocked(&req.from_peer_id, true)
            .map_err(|e| TenetError::Storage(e.to_string()))?;

        Ok(())
    }

    // -------------------------------------------------------------------------
    // Groups
    // -------------------------------------------------------------------------

    pub fn list_groups(&self) -> Result<Vec<FfiGroup>, TenetError> {
        let inner = self.inner.lock().unwrap();
        let groups = inner
            .storage
            .list_groups()
            .map_err(|e| TenetError::Storage(e.to_string()))?;
        let mut result = Vec::with_capacity(groups.len());
        for g in groups {
            let members = inner
                .storage
                .list_group_members(&g.group_id)
                .unwrap_or_default();
            result.push(FfiGroup {
                group_id: g.group_id,
                creator_id: g.creator_id,
                member_count: members.len() as u32,
                created_at: g.created_at as i64,
            });
        }
        Ok(result)
    }

    pub fn get_group(&self, group_id: String) -> Result<Option<FfiGroup>, TenetError> {
        let inner = self.inner.lock().unwrap();
        let group = inner
            .storage
            .get_group(&group_id)
            .map_err(|e| TenetError::Storage(e.to_string()))?;
        match group {
            None => Ok(None),
            Some(g) => {
                let members = inner
                    .storage
                    .list_group_members(&g.group_id)
                    .unwrap_or_default();
                Ok(Some(FfiGroup {
                    group_id: g.group_id,
                    creator_id: g.creator_id,
                    member_count: members.len() as u32,
                    created_at: g.created_at as i64,
                }))
            }
        }
    }

    pub fn list_group_members(&self, group_id: String) -> Result<Vec<FfiGroupMember>, TenetError> {
        let inner = self.inner.lock().unwrap();
        let members = inner
            .storage
            .list_group_members(&group_id)
            .map_err(|e| TenetError::Storage(e.to_string()))?;
        let result = members
            .into_iter()
            .map(|m| {
                let display_name = inner
                    .storage
                    .get_peer(&m.peer_id)
                    .ok()
                    .flatten()
                    .and_then(|p| p.display_name);
                FfiGroupMember {
                    group_id: m.group_id,
                    peer_id: m.peer_id,
                    display_name,
                    joined_at: m.joined_at as i64,
                }
            })
            .collect();
        Ok(result)
    }

    pub fn create_group(&self, member_ids: Vec<String>) -> Result<String, TenetError> {
        let inner = self.inner.lock().unwrap();
        let my_id = inner.keypair.id.clone();
        let now = now_secs();

        // Generate a random group ID.
        let mut id_bytes = [0u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut id_bytes);
        let group_id = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(id_bytes);

        // Generate a random 32-byte group key.
        let mut group_key = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut group_key);

        let group_row = GroupRow {
            group_id: group_id.clone(),
            group_key: group_key.to_vec(),
            creator_id: my_id.clone(),
            created_at: now,
            key_version: 0,
        };

        // Build member rows (include the creator).
        let mut members: Vec<GroupMemberRow> = member_ids
            .iter()
            .map(|pid| GroupMemberRow {
                group_id: group_id.clone(),
                peer_id: pid.clone(),
                joined_at: now,
            })
            .collect();
        if !member_ids.contains(&my_id) {
            members.push(GroupMemberRow {
                group_id: group_id.clone(),
                peer_id: my_id,
                joined_at: now,
            });
        }

        inner
            .storage
            .insert_group_with_members(&group_row, &members)
            .map_err(|e| TenetError::Storage(e.to_string()))?;

        Ok(group_id)
    }

    pub fn add_group_member(&self, group_id: String, peer_id: String) -> Result<(), TenetError> {
        let inner = self.inner.lock().unwrap();
        let now = now_secs();
        inner
            .storage
            .insert_group_member(&GroupMemberRow {
                group_id,
                peer_id,
                joined_at: now,
            })
            .map_err(|e| TenetError::Storage(e.to_string()))
    }

    pub fn remove_group_member(
        &self,
        group_id: String,
        peer_id: String,
    ) -> Result<(), TenetError> {
        self.inner
            .lock()
            .unwrap()
            .storage
            .remove_group_member(&group_id, &peer_id)
            .map_err(|e| TenetError::Storage(e.to_string()))?;
        Ok(())
    }

    pub fn leave_group(&self, group_id: String) -> Result<(), TenetError> {
        let inner = self.inner.lock().unwrap();
        let my_id = inner.keypair.id.clone();
        // Remove self from the member list, then delete the group record if no one is left.
        let _ = inner.storage.remove_group_member(&group_id, &my_id);
        let remaining = inner
            .storage
            .list_group_members(&group_id)
            .unwrap_or_default();
        if remaining.is_empty() {
            let _ = inner.storage.delete_group(&group_id);
        }
        Ok(())
    }

    pub fn list_group_invites(&self) -> Result<Vec<FfiGroupInvite>, TenetError> {
        let inner = self.inner.lock().unwrap();
        let rows = inner
            .storage
            .list_group_invites(None, None)
            .map_err(|e| TenetError::Storage(e.to_string()))?;
        Ok(rows.into_iter().map(group_invite_to_ffi).collect())
    }

    pub fn accept_group_invite(&self, invite_id: i64) -> Result<(), TenetError> {
        let inner = self.inner.lock().unwrap();
        let invite = inner
            .storage
            .get_group_invite(invite_id)
            .map_err(|e| TenetError::Storage(e.to_string()))?
            .ok_or_else(|| TenetError::NotFound(format!("group_invite: {invite_id}")))?;

        inner
            .storage
            .update_group_invite_status(invite_id, "accepted")
            .map_err(|e| TenetError::Storage(e.to_string()))?;

        // Add self as a group member if the group exists.
        let my_id = inner.keypair.id.clone();
        let now = now_secs();
        if inner
            .storage
            .get_group(&invite.group_id)
            .ok()
            .flatten()
            .is_some()
        {
            let _ = inner.storage.insert_group_member(&GroupMemberRow {
                group_id: invite.group_id,
                peer_id: my_id,
                joined_at: now,
            });
        }

        Ok(())
    }

    pub fn ignore_group_invite(&self, invite_id: i64) -> Result<(), TenetError> {
        self.inner
            .lock()
            .unwrap()
            .storage
            .update_group_invite_status(invite_id, "ignored")
            .map_err(|e| TenetError::Storage(e.to_string()))?;
        Ok(())
    }

    // -------------------------------------------------------------------------
    // Profiles
    // -------------------------------------------------------------------------

    pub fn get_own_profile(&self) -> Result<Option<FfiProfile>, TenetError> {
        let inner = self.inner.lock().unwrap();
        let my_id = inner.keypair.id.clone();
        let row = inner
            .storage
            .get_profile(&my_id)
            .map_err(|e| TenetError::Storage(e.to_string()))?;
        Ok(row.map(profile_to_ffi))
    }

    pub fn update_own_profile(
        &self,
        display_name: Option<String>,
        bio: Option<String>,
        avatar_hash: Option<String>,
    ) -> Result<(), TenetError> {
        let inner = self.inner.lock().unwrap();
        let my_id = inner.keypair.id.clone();
        let now = now_secs();

        // Merge with existing profile so unset fields are not wiped.
        let existing = inner
            .storage
            .get_profile(&my_id)
            .ok()
            .flatten()
            .unwrap_or_else(|| ProfileRow {
                user_id: my_id.clone(),
                display_name: None,
                bio: None,
                avatar_hash: None,
                public_fields: "{}".to_string(),
                friends_fields: "{}".to_string(),
                updated_at: now,
            });

        let updated = ProfileRow {
            user_id: my_id,
            display_name: display_name.or(existing.display_name),
            bio: bio.or(existing.bio),
            avatar_hash: avatar_hash.or(existing.avatar_hash),
            public_fields: existing.public_fields,
            friends_fields: existing.friends_fields,
            updated_at: now,
        };

        inner
            .storage
            .upsert_profile(&updated)
            .map_err(|e| TenetError::Storage(e.to_string()))
    }

    pub fn get_peer_profile(&self, peer_id: String) -> Result<Option<FfiProfile>, TenetError> {
        let inner = self.inner.lock().unwrap();
        let row = inner
            .storage
            .get_profile(&peer_id)
            .map_err(|e| TenetError::Storage(e.to_string()))?;
        Ok(row.map(profile_to_ffi))
    }

    // -------------------------------------------------------------------------
    // Notifications
    // -------------------------------------------------------------------------

    pub fn list_notifications(&self, unread_only: bool) -> Result<Vec<FfiNotification>, TenetError> {
        let inner = self.inner.lock().unwrap();
        let rows = inner
            .storage
            .list_notifications(unread_only, 200)
            .map_err(|e| TenetError::Storage(e.to_string()))?;
        Ok(rows.into_iter().map(notification_to_ffi).collect())
    }

    pub fn notification_count(&self) -> Result<u32, TenetError> {
        let inner = self.inner.lock().unwrap();
        inner
            .storage
            .count_unread_notifications()
            .map_err(|e| TenetError::Storage(e.to_string()))
    }

    pub fn mark_notification_read(&self, notification_id: i64) -> Result<(), TenetError> {
        self.inner
            .lock()
            .unwrap()
            .storage
            .mark_notification_read(notification_id)
            .map_err(|e| TenetError::Storage(e.to_string()))?;
        Ok(())
    }

    pub fn mark_all_notifications_read(&self) -> Result<(), TenetError> {
        self.inner
            .lock()
            .unwrap()
            .storage
            .mark_all_notifications_read()
            .map_err(|e| TenetError::Storage(e.to_string()))?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn client_config(relay_url: &str) -> ClientConfig {
    ClientConfig::new(
        relay_url,
        DEFAULT_TTL_SECONDS,
        ClientEncryption::Encrypted {
            hpke_info: FFI_HPKE_INFO.to_vec(),
            payload_aad: FFI_PAYLOAD_AAD.to_vec(),
        },
    )
}

fn row_to_ffi(r: tenet::storage::MessageRow) -> FfiMessage {
    FfiMessage {
        message_id: r.message_id,
        sender_id: r.sender_id,
        recipient_id: Some(r.recipient_id).filter(|s| !s.is_empty()),
        kind: r.message_kind,
        group_id: r.group_id,
        body: r.body.unwrap_or_default(),
        timestamp: r.timestamp as i64,
        is_read: r.is_read,
        reply_to: r.reply_to,
    }
}

fn peer_row_to_ffi(p: PeerRow) -> FfiPeer {
    FfiPeer {
        peer_id: p.peer_id,
        display_name: p.display_name,
        is_online: p.online,
        is_blocked: p.is_blocked,
        is_muted: p.is_muted,
        is_friend: p.is_friend,
        last_seen: p.last_seen_online.map(|t| t as i64),
    }
}

fn friend_request_to_ffi(r: FriendRequestRow) -> FfiFriendRequest {
    FfiFriendRequest {
        id: r.id,
        from_peer_id: r.from_peer_id,
        to_peer_id: r.to_peer_id,
        status: r.status,
        message: r.message,
        direction: r.direction,
        created_at: r.created_at as i64,
    }
}

fn group_invite_to_ffi(r: GroupInviteRow) -> FfiGroupInvite {
    FfiGroupInvite {
        id: r.id,
        group_id: r.group_id,
        from_peer_id: r.from_peer_id,
        to_peer_id: r.to_peer_id,
        status: r.status,
        message: r.message,
        direction: r.direction,
        created_at: r.created_at as i64,
    }
}

fn profile_to_ffi(r: ProfileRow) -> FfiProfile {
    FfiProfile {
        user_id: r.user_id,
        display_name: r.display_name,
        bio: r.bio,
        avatar_hash: r.avatar_hash,
        public_fields: r.public_fields,
        friends_fields: r.friends_fields,
        updated_at: r.updated_at as i64,
    }
}

fn notification_to_ffi(r: NotificationRow) -> FfiNotification {
    FfiNotification {
        id: r.id,
        notification_type: r.notification_type,
        message_id: r.message_id,
        sender_id: r.sender_id,
        created_at: r.created_at as i64,
        is_read: r.read,
    }
}

#[allow(clippy::too_many_arguments)]
fn make_msg_row(
    envelope: &tenet::protocol::Envelope,
    sender_id: &str,
    recipient_id: &str,
    kind: &str,
    group_id: Option<String>,
    body: Option<String>,
    received_at: u64,
    reply_to: Option<String>,
) -> tenet::storage::MessageRow {
    tenet::storage::MessageRow {
        message_id: envelope.header.message_id.0.clone(),
        sender_id: sender_id.to_string(),
        recipient_id: recipient_id.to_string(),
        message_kind: kind.to_string(),
        group_id,
        body,
        timestamp: envelope.header.timestamp,
        received_at,
        ttl_seconds: envelope.header.ttl_seconds,
        is_read: true,
        raw_envelope: serde_json::to_string(envelope).ok(),
        reply_to,
        signature_verified: true,
    }
}

fn reaction_summary(
    storage: &Storage,
    target_id: &str,
    my_id: &str,
) -> Result<FfiReactionSummary, TenetError> {
    let (upvotes, downvotes) = storage
        .count_reactions(target_id)
        .map_err(|e| TenetError::Storage(e.to_string()))?;
    let my_reaction = storage
        .get_reaction(target_id, my_id)
        .ok()
        .flatten()
        .map(|r| r.reaction);
    Ok(FfiReactionSummary {
        upvotes,
        downvotes,
        my_reaction,
    })
}

// Pull in the UniFFI scaffolding generated from tenet_ffi.udl.
uniffi::include_scaffolding!("tenet_ffi");
