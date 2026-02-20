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

use tenet::client::{ClientConfig, ClientEncryption, RelayClient};
use tenet::crypto::StoredKeypair;
use tenet::identity::{resolve_identity, store_relay_for_identity};
use tenet::message_handler::StorageMessageHandler;
use tenet::storage::{PeerRow, Storage};

pub use types::{FfiMessage, FfiPeer, FfiSyncResult};

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
    ///
    /// If no identity exists a fresh keypair is generated and persisted.
    /// The relay URL is stored in the identity's database so it survives
    /// app restarts.
    pub fn new(data_dir: String, relay_url: String) -> Result<Self, TenetError> {
        let path = Path::new(&data_dir);

        let resolved =
            resolve_identity(path, None).map_err(|e| TenetError::Init(e.to_string()))?;

        // Persist the relay URL so it's available after process restarts.
        store_relay_for_identity(&resolved.storage, &relay_url)
            .map_err(|e| TenetError::Init(e.to_string()))?;

        let db_path = resolved.identity_dir.join("tenet.db");

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

    // ---------------------------------------------------------------------------
    // Messages
    // ---------------------------------------------------------------------------

    pub fn list_messages(
        &self,
        kind: Option<String>,
        limit: u32,
        before_ts: Option<i64>,
    ) -> Result<Vec<FfiMessage>, TenetError> {
        let inner = self.inner.lock().unwrap();
        let before = before_ts.map(|t| t as u64);
        let rows = inner
            .storage
            .list_messages(kind.as_deref(), None, before, limit)
            .map_err(|e| TenetError::Storage(e.to_string()))?;

        Ok(rows
            .into_iter()
            .map(|r| FfiMessage {
                message_id: r.message_id,
                sender_id: r.sender_id,
                recipient_id: Some(r.recipient_id).filter(|s| !s.is_empty()),
                kind: r.message_kind,
                group_id: r.group_id,
                body: r.body.unwrap_or_default(),
                timestamp: r.timestamp as i64,
                is_read: r.is_read,
                reply_to: r.reply_to,
            })
            .collect())
    }

    pub fn mark_read(&self, message_id: String) -> Result<(), TenetError> {
        let inner = self.inner.lock().unwrap();
        inner
            .storage
            .mark_message_read(&message_id)
            .map_err(|e| TenetError::Storage(e.to_string()))?;
        Ok(())
    }

    pub fn send_direct(&self, recipient_id: String, body: String) -> Result<(), TenetError> {
        let inner = self.inner.lock().unwrap();

        // Look up the peer's encryption public key.
        let peer = inner
            .storage
            .get_peer(&recipient_id)
            .map_err(|e| TenetError::Storage(e.to_string()))?
            .ok_or_else(|| TenetError::NotFound(format!("peer not found: {recipient_id}")))?;

        let enc_key = peer
            .encryption_public_key
            .as_deref()
            .unwrap_or(&peer.signing_public_key)
            .to_string();

        let config = ClientConfig::new(
            &inner.relay_url,
            DEFAULT_TTL_SECONDS,
            ClientEncryption::Encrypted {
                hpke_info: FFI_HPKE_INFO.to_vec(),
                payload_aad: FFI_PAYLOAD_AAD.to_vec(),
            },
        );
        let relay_client = RelayClient::new(inner.keypair.clone(), config);
        let envelope = relay_client
            .send_message(&recipient_id, &enc_key, &body)
            .map_err(|e| TenetError::Send(e.to_string()))?;

        // Also persist a local copy of the sent message so it appears in DM history.
        let now = now_secs();
        let row = tenet::storage::MessageRow {
            message_id: envelope.header.message_id.0.clone(),
            sender_id: inner.keypair.id.clone(),
            recipient_id: recipient_id.clone(),
            message_kind: "direct".to_string(),
            group_id: None,
            body: Some(body),
            timestamp: envelope.header.timestamp,
            received_at: now,
            ttl_seconds: envelope.header.ttl_seconds,
            is_read: true,
            raw_envelope: serde_json::to_string(&envelope).ok(),
            reply_to: None,
            signature_verified: true,
        };
        let _ = inner.storage.insert_message(&row);

        Ok(())
    }

    pub fn send_public(&self, body: String) -> Result<(), TenetError> {
        let inner = self.inner.lock().unwrap();

        let peers = inner
            .storage
            .list_peers()
            .map_err(|e| TenetError::Send(e.to_string()))?;

        let config = ClientConfig::new(
            &inner.relay_url,
            DEFAULT_TTL_SECONDS,
            ClientEncryption::Encrypted {
                hpke_info: FFI_HPKE_INFO.to_vec(),
                payload_aad: FFI_PAYLOAD_AAD.to_vec(),
            },
        );
        let mut relay_client = RelayClient::new(inner.keypair.clone(), config);

        // Populate peers so forwarding to known peers works correctly.
        for peer in &peers {
            relay_client.add_peer_with_encryption(
                peer.peer_id.clone(),
                peer.signing_public_key.clone(),
                peer.encryption_public_key
                    .clone()
                    .unwrap_or_else(|| peer.signing_public_key.clone()),
            );
        }

        // send_public_message posts to relay and forwards to known peers internally.
        let envelope = relay_client
            .send_public_message(&body)
            .map_err(|e| TenetError::Send(e.to_string()))?;

        // Persist locally so the post appears in the timeline immediately.
        let now = now_secs();
        let row = tenet::storage::MessageRow {
            message_id: envelope.header.message_id.0.clone(),
            sender_id: inner.keypair.id.clone(),
            recipient_id: String::new(),
            message_kind: "public".to_string(),
            group_id: None,
            body: Some(body),
            timestamp: envelope.header.timestamp,
            received_at: now,
            ttl_seconds: envelope.header.ttl_seconds,
            is_read: true,
            raw_envelope: serde_json::to_string(&envelope).ok(),
            reply_to: None,
            signature_verified: true,
        };
        let _ = inner.storage.insert_message(&row);

        Ok(())
    }

    // ---------------------------------------------------------------------------
    // Sync
    // ---------------------------------------------------------------------------

    pub fn sync(&self) -> Result<FfiSyncResult, TenetError> {
        // Extract what we need while holding the lock, then drop it before
        // doing network I/O so we don't hold the mutex across the HTTP call.
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

        // Open a dedicated storage connection for the handler (SQLite WAL mode
        // allows concurrent readers/writers).
        let handler_storage = Storage::open(&db_path)
            .map_err(|e| TenetError::Sync(format!("handler storage: {e}")))?;

        let handler = StorageMessageHandler::new_with_crypto(
            handler_storage,
            keypair.clone(),
            FFI_HPKE_INFO.to_vec(),
            FFI_PAYLOAD_AAD.to_vec(),
        );

        let config = ClientConfig::new(
            &relay_url,
            DEFAULT_TTL_SECONDS,
            ClientEncryption::Encrypted {
                hpke_info: FFI_HPKE_INFO.to_vec(),
                payload_aad: FFI_PAYLOAD_AAD.to_vec(),
            },
        );
        let mut relay_client = RelayClient::new(keypair, config);

        // Register all known peers so the client can verify and decrypt.
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

        relay_client.set_handler(Box::new(handler));

        let outcome = relay_client
            .sync_inbox(None)
            .map_err(|e| TenetError::Sync(e.to_string()))?;

        // Count genuinely new messages (not duplicates or errors).
        let new_messages = outcome.messages.len() as u32;

        Ok(FfiSyncResult {
            fetched: outcome.fetched as u32,
            new_messages,
            errors: outcome.errors,
        })
    }

    // ---------------------------------------------------------------------------
    // Peers
    // ---------------------------------------------------------------------------

    pub fn list_peers(&self) -> Result<Vec<FfiPeer>, TenetError> {
        let inner = self.inner.lock().unwrap();
        let rows = inner
            .storage
            .list_peers()
            .map_err(|e| TenetError::Storage(e.to_string()))?;

        Ok(rows
            .into_iter()
            .map(|p| FfiPeer {
                peer_id: p.peer_id,
                display_name: p.display_name,
                is_online: p.online,
                is_blocked: p.is_blocked,
                is_muted: p.is_muted,
                is_friend: p.is_friend,
                last_seen: p.last_seen_online.map(|t| t as i64),
            })
            .collect())
    }

    pub fn add_peer(
        &self,
        peer_id: String,
        display_name: Option<String>,
        signing_public_key_hex: String,
    ) -> Result<(), TenetError> {
        // Validate the hex key before storing (Ed25519 signing keys are 32 bytes).
        tenet::crypto::validate_hex_key(&signing_public_key_hex, 32, "signing_public_key_hex")
            .map_err(|e| TenetError::InvalidKey(e))?;

        let inner = self.inner.lock().unwrap();
        let now = now_secs();
        let row = PeerRow {
            peer_id: peer_id.clone(),
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
        };
        inner
            .storage
            .insert_peer(&row)
            .map_err(|e| TenetError::Storage(e.to_string()))?;
        Ok(())
    }

    pub fn remove_peer(&self, peer_id: String) -> Result<(), TenetError> {
        let inner = self.inner.lock().unwrap();
        inner
            .storage
            .delete_peer(&peer_id)
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

// Pull in the UniFFI scaffolding generated from tenet_ffi.udl.
uniffi::include_scaffolding!("tenet_ffi");
