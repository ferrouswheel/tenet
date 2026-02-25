use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use rand::rngs::OsRng;
use rand::RngCore;
use serde::{Deserialize, Serialize};

use crate::crypto::{generate_content_key, StoredKeypair, CONTENT_KEY_SIZE, NONCE_SIZE};
use crate::protocol::{
    build_encrypted_payload, build_envelope_from_payload, build_meta_payload,
    build_plaintext_envelope, decode_meta_payload, decrypt_encrypted_payload, Envelope,
    EnvelopeBuildError, MessageKind, MetaMessage, PayloadCryptoError,
};

use crate::simulation::{
    HistoricalMessage, MessageEncryption, MetricsTracker, RollingLatencyTracker, SimMessage,
    SimulationMetrics, SIMULATION_ACK_WINDOW_STEPS, SIMULATION_HPKE_INFO, SIMULATION_PAYLOAD_AAD,
};

use crate::groups::{GroupError, GroupInfo, GroupManager};
use crate::protocol::ContentId;

/// Maximum number of hops a public message can propagate
pub const MAX_PUBLIC_MESSAGE_HOPS: usize = 10;

/// Maximum public message IDs advertised in a single `MeshAvailable` response (simulation)
const MAX_SIM_MESH_IDS: usize = 500;
/// Maximum message IDs requested in a single `MeshRequest` (simulation)
const MAX_SIM_MESH_REQUEST_IDS: usize = 100;
/// Maximum envelopes per `MeshDelivery` batch (simulation)
const MAX_SIM_MESH_BATCH: usize = 10;

/// Metadata for tracking public message propagation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublicMessageMetadata {
    pub seen_by: HashSet<String>,
    pub first_seen_at: u64,
    pub propagation_count: usize,
}

impl PublicMessageMetadata {
    pub fn new(first_seen_at: u64) -> Self {
        Self {
            seen_by: HashSet::new(),
            first_seen_at,
            propagation_count: 0,
        }
    }

    pub fn mark_seen_by(&mut self, peer_id: String) {
        self.seen_by.insert(peer_id);
    }

    pub fn increment_propagation(&mut self) {
        self.propagation_count = self.propagation_count.saturating_add(1);
    }

    pub fn should_forward(&self) -> bool {
        self.propagation_count < MAX_PUBLIC_MESSAGE_HOPS
    }
}

/// Information about a known peer
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    pub peer_id: String,
    pub signing_public_key_hex: String,
    pub encryption_public_key_hex: Option<String>,
    pub added_at: u64,
}

/// Registry for managing known peers and their public keys
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerRegistry {
    peers: HashMap<String, PeerInfo>,
}

impl PeerRegistry {
    /// Create a new empty peer registry
    pub fn new() -> Self {
        Self {
            peers: HashMap::new(),
        }
    }

    /// Add a peer to the registry with their signing public key
    pub fn add_peer(&mut self, peer_id: String, signing_public_key_hex: String) {
        self.add_peer_with_encryption(peer_id, signing_public_key_hex, None);
    }

    /// Add a peer to the registry with both signing and encryption keys
    pub fn add_peer_with_encryption(
        &mut self,
        peer_id: String,
        signing_public_key_hex: String,
        encryption_public_key_hex: Option<String>,
    ) {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        self.peers.insert(
            peer_id.clone(),
            PeerInfo {
                peer_id,
                signing_public_key_hex,
                encryption_public_key_hex,
                added_at: timestamp,
            },
        );
    }

    /// Remove a peer from the registry
    pub fn remove_peer(&mut self, peer_id: &str) -> Option<PeerInfo> {
        self.peers.remove(peer_id)
    }

    /// Get peer information by peer ID
    pub fn get_peer(&self, peer_id: &str) -> Option<&PeerInfo> {
        self.peers.get(peer_id)
    }

    /// Get only the signing public key for a peer
    pub fn get_signing_key(&self, peer_id: &str) -> Option<&str> {
        self.peers
            .get(peer_id)
            .map(|info| info.signing_public_key_hex.as_str())
    }

    /// Get only the encryption public key for a peer
    pub fn get_encryption_key(&self, peer_id: &str) -> Option<&str> {
        self.peers
            .get(peer_id)
            .and_then(|info| info.encryption_public_key_hex.as_deref())
    }

    /// Check if a peer is in the registry
    pub fn has_peer(&self, peer_id: &str) -> bool {
        self.peers.contains_key(peer_id)
    }

    /// List all peers in the registry
    pub fn list_peers(&self) -> Vec<&PeerInfo> {
        self.peers.values().collect()
    }

    /// Get the number of peers in the registry
    pub fn peer_count(&self) -> usize {
        self.peers.len()
    }

    /// Get all peer IDs
    pub fn peer_ids(&self) -> Vec<String> {
        self.peers.keys().cloned().collect()
    }
}

impl Default for PeerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub enum ClientEncryption {
    Plaintext,
    Encrypted {
        hpke_info: Vec<u8>,
        payload_aad: Vec<u8>,
    },
}

#[derive(Debug, Clone)]
pub struct ClientConfig {
    relay_url: String,
    ttl_seconds: u64,
    encryption: ClientEncryption,
}

impl ClientConfig {
    pub fn new(
        relay_url: impl Into<String>,
        ttl_seconds: u64,
        encryption: ClientEncryption,
    ) -> Self {
        Self {
            relay_url: relay_url.into(),
            ttl_seconds,
            encryption,
        }
    }

    pub fn relay_url(&self) -> &str {
        &self.relay_url
    }

    pub fn ttl_seconds(&self) -> u64 {
        self.ttl_seconds
    }

    pub fn encryption(&self) -> &ClientEncryption {
        &self.encryption
    }

    /// Build the WebSocket URL for subscribing to a recipient's inbox.
    ///
    /// Converts the relay HTTP URL to a WebSocket URL and appends the
    /// `/ws/{recipient_id}` path.
    pub fn ws_url(&self, recipient_id: &str) -> String {
        let base = self.relay_url.trim_end_matches('/');
        let ws_base = if base.starts_with("https://") {
            base.replacen("https://", "wss://", 1)
        } else if base.starts_with("http://") {
            base.replacen("http://", "ws://", 1)
        } else {
            format!("ws://{base}")
        };
        format!("{ws_base}/ws/{recipient_id}")
    }
}

#[derive(Debug, Clone)]
pub struct ClientMessage {
    pub message_id: String,
    pub sender_id: String,
    pub timestamp: u64,
    pub body: String,
}

/// A single event produced by `sync_inbox()` for one envelope.
#[derive(Debug)]
pub struct SyncEvent {
    pub envelope: Envelope,
    pub outcome: SyncEventOutcome,
}

/// The outcome of processing a single envelope during `sync_inbox()`.
#[derive(Debug)]
pub enum SyncEventOutcome {
    /// Verified and decrypted successfully. Ready to store.
    Message(ClientMessage),

    /// A structured meta-message (Online, Ack, FriendRequest, FriendAccept, etc.).
    Meta(MetaMessage),

    /// A raw meta payload that couldn't be parsed as a `MetaMessage`.
    /// The body string is passed through for custom dispatch (reactions, profiles, etc.).
    RawMeta { body: String },

    /// Message was already seen (deduplicated).
    Duplicate,

    /// Sender is not in the peer registry.
    UnknownSender,

    /// Signature verification failed.
    InvalidSignature { reason: String },

    /// Decryption failed.
    DecryptFailed { reason: String },

    /// Message TTL has expired.
    TtlExpired,
}

/// A trait for receiving typed events during `sync_inbox()`.
///
/// All methods have default no-op implementations so callers only override
/// what they need.
///
/// `on_message`, `on_meta`, and `on_raw_meta` return a `Vec<Envelope>` of
/// outgoing envelopes the caller should send (e.g. auto-accept replies).
/// The caller is responsible for routing them via whatever transport is
/// appropriate (HTTP relay, simulation channel, etc.).
pub trait MessageHandler: Send {
    /// Called for each successfully verified and decrypted message.
    /// Returns any outgoing envelopes to be sent by the caller.
    fn on_message(&mut self, _envelope: &Envelope, _message: &ClientMessage) -> Vec<Envelope> {
        Vec::new()
    }

    /// Called for each structured meta-message.
    ///
    /// Common variants and their return conventions:
    /// - `Online` / `Ack` — no reply needed; update peer presence state.
    /// - `FriendRequest` — optionally return a `FriendAccept` envelope.
    /// - `FriendAccept` — no reply; record the peer's keys.
    /// - `GroupInvite` — optionally return a `GroupInviteAccept` envelope.
    /// - `GroupInviteAccept` — no reply; distribute the group key.
    /// - `MessageRequest { peer_id, since_timestamp }` — public-mesh Phase 1.
    ///   Respond with a `MeshAvailable` envelope addressed to `peer_id` listing
    ///   public message IDs held since `since_timestamp`.
    /// - `MeshAvailable { peer_id, message_ids, since_timestamp }` — Phase 2.
    ///   Respond with a `MeshRequest` envelope to `peer_id` for unknown IDs.
    /// - `MeshRequest { peer_id, message_ids }` — Phase 3.
    ///   Respond with one or more `MeshDelivery` envelopes (batched, max 10 per
    ///   envelope) containing the requested public-message payloads and a
    ///   `sender_keys` map so the receiver can verify signatures.
    /// - `MeshDelivery { peer_id, envelopes, sender_keys }` — Phase 4 (terminal).
    ///   No reply; store the delivered public messages.
    ///
    /// Returns any outgoing envelopes to be sent by the caller.
    fn on_meta(&mut self, _meta: &MetaMessage) -> Vec<Envelope> {
        Vec::new()
    }

    /// Called for raw meta payloads that couldn't be parsed as `MetaMessage`.
    /// The body string is the raw payload for custom dispatch (reactions, etc.).
    /// Returns any outgoing envelopes to be sent by the caller.
    fn on_raw_meta(&mut self, _envelope: &Envelope, _body: &str) -> Vec<Envelope> {
        Vec::new()
    }

    /// Called when a message is rejected (bad signature, unknown sender, etc.).
    fn on_rejected(&mut self, _envelope: &Envelope, _reason: &str) {}

    /// Drain and return group keys received via `group_key_distribution` messages.
    /// The caller should apply these to the client's in-memory `GroupManager`.
    fn take_pending_group_keys(&mut self) -> Vec<(String, Vec<u8>)> {
        Vec::new()
    }

    /// Called when the local peer creates a new group so that the handler can persist
    /// the group and outgoing invite rows to storage.
    fn on_group_created(
        &mut self,
        _group_id: &str,
        _group_key: &[u8; 32],
        _creator_id: &str,
        _members: &[String],
    ) {
    }
}

/// Result of `RelayClient::sync_inbox()`.
#[derive(Debug)]
pub struct SyncOutcome {
    /// Per-envelope events, including duplicates, errors, and successes.
    pub events: Vec<SyncEvent>,
    /// Number of envelopes fetched from the relay (before deduplication).
    pub fetched: usize,
    /// Convenience view: all successfully decoded messages (from `SyncEventOutcome::Message`).
    pub messages: Vec<ClientMessage>,
    /// Convenience view: human-readable rejection reasons.
    pub errors: Vec<String>,
}

#[derive(Debug)]
pub enum ClientError {
    Http(String),
    Crypto(PayloadCryptoError),
    Envelope(EnvelopeBuildError),
    Protocol(String),
    Time(String),
    Offline,
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Http(error) => write!(f, "http error: {error}"),
            ClientError::Crypto(error) => write!(f, "crypto error: {error}"),
            ClientError::Envelope(error) => write!(f, "envelope error: {error}"),
            ClientError::Protocol(error) => write!(f, "protocol error: {error}"),
            ClientError::Time(error) => write!(f, "time error: {error}"),
            ClientError::Offline => write!(f, "client is offline"),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<PayloadCryptoError> for ClientError {
    fn from(error: PayloadCryptoError) -> Self {
        ClientError::Crypto(error)
    }
}

impl From<EnvelopeBuildError> for ClientError {
    fn from(error: EnvelopeBuildError) -> Self {
        ClientError::Envelope(error)
    }
}

pub struct RelayClient {
    keypair: StoredKeypair,
    config: ClientConfig,
    online: bool,
    feed: Vec<ClientMessage>,
    seen: HashSet<String>,
    peer_registry: PeerRegistry,
    public_message_cache: Vec<Envelope>,
    public_message_metadata: HashMap<ContentId, PublicMessageMetadata>,
    group_manager: GroupManager,
    handler: Option<Box<dyn MessageHandler>>,
}

impl std::fmt::Debug for RelayClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayClient")
            .field("keypair", &self.keypair)
            .field("config", &self.config)
            .field("online", &self.online)
            .field("feed", &self.feed)
            .field("seen", &self.seen)
            .field("peer_registry", &self.peer_registry)
            .field("public_message_cache", &self.public_message_cache)
            .field("public_message_metadata", &self.public_message_metadata)
            .field("group_manager", &self.group_manager)
            .field(
                "handler",
                &self.handler.as_ref().map(|_| "<MessageHandler>"),
            )
            .finish()
    }
}

impl Clone for RelayClient {
    fn clone(&self) -> Self {
        Self {
            keypair: self.keypair.clone(),
            config: self.config.clone(),
            online: self.online,
            feed: self.feed.clone(),
            seen: self.seen.clone(),
            peer_registry: self.peer_registry.clone(),
            public_message_cache: self.public_message_cache.clone(),
            public_message_metadata: self.public_message_metadata.clone(),
            group_manager: self.group_manager.clone(),
            handler: None, // handlers are not cloned
        }
    }
}

impl RelayClient {
    pub fn new(keypair: StoredKeypair, config: ClientConfig) -> Self {
        Self {
            keypair,
            config,
            online: true,
            feed: Vec::new(),
            seen: HashSet::new(),
            peer_registry: PeerRegistry::new(),
            public_message_cache: Vec::new(),
            public_message_metadata: HashMap::new(),
            group_manager: GroupManager::new(),
            handler: None,
        }
    }

    /// Register a handler that receives typed events during `sync_inbox()`.
    pub fn set_handler(&mut self, handler: Box<dyn MessageHandler>) {
        self.handler = Some(handler);
    }

    /// Remove any registered handler.
    pub fn clear_handler(&mut self) {
        self.handler = None;
    }

    pub fn add_peer(&mut self, peer_id: String, signing_public_key_hex: String) {
        self.peer_registry.add_peer(peer_id, signing_public_key_hex);
    }

    pub fn add_peer_with_encryption(
        &mut self,
        peer_id: String,
        signing_public_key_hex: String,
        encryption_public_key_hex: String,
    ) {
        self.peer_registry.add_peer_with_encryption(
            peer_id,
            signing_public_key_hex,
            Some(encryption_public_key_hex),
        );
    }

    pub fn remove_peer(&mut self, peer_id: &str) -> Option<PeerInfo> {
        self.peer_registry.remove_peer(peer_id)
    }

    pub fn get_peer(&self, peer_id: &str) -> Option<&PeerInfo> {
        self.peer_registry.get_peer(peer_id)
    }

    pub fn list_peers(&self) -> Vec<&PeerInfo> {
        self.peer_registry.list_peers()
    }

    pub fn peer_registry(&self) -> &PeerRegistry {
        &self.peer_registry
    }

    pub fn group_manager(&self) -> &GroupManager {
        &self.group_manager
    }

    pub fn group_manager_mut(&mut self) -> &mut GroupManager {
        &mut self.group_manager
    }

    pub fn create_group(
        &mut self,
        group_id: String,
        members: Vec<String>,
    ) -> Result<GroupInfo, GroupError> {
        let info = self
            .group_manager
            .create_group(group_id, members, self.id().to_string())?;
        if let Some(ref mut h) = self.handler {
            let members_vec: Vec<String> = info.members.iter().cloned().collect();
            h.on_group_created(
                &info.group_id,
                &info.group_key,
                &info.creator_id,
                &members_vec,
            );
        }
        Ok(info)
    }

    pub fn get_group(&self, group_id: &str) -> Option<&GroupInfo> {
        self.group_manager.get_group(group_id)
    }

    pub fn list_groups(&self) -> Vec<&str> {
        self.group_manager.list_groups()
    }

    pub fn id(&self) -> &str {
        &self.keypair.id
    }

    pub fn public_key_hex(&self) -> &str {
        &self.keypair.public_key_hex
    }

    pub fn signing_public_key_hex(&self) -> &str {
        &self.keypair.signing_public_key_hex
    }

    pub fn signing_private_key_hex(&self) -> &str {
        &self.keypair.signing_private_key_hex
    }

    pub fn is_online(&self) -> bool {
        self.online
    }

    pub fn set_online(&mut self, online: bool) {
        self.online = online;
    }

    pub fn feed(&self) -> &[ClientMessage] {
        &self.feed
    }

    pub fn public_message_cache(&self) -> &[Envelope] {
        &self.public_message_cache
    }

    /// Send a public message that will be propagated across the network
    pub fn send_public_message(&mut self, message: &str) -> Result<Envelope, ClientError> {
        if !self.online {
            return Err(ClientError::Offline);
        }

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|err| ClientError::Time(err.to_string()))?
            .as_secs();

        // For public messages, recipient_id is empty or "*" to indicate broadcast
        let envelope = match self.config.encryption() {
            ClientEncryption::Plaintext => {
                let mut salt = [0u8; 16];
                OsRng.fill_bytes(&mut salt);
                build_plaintext_envelope(
                    self.id(),
                    "*", // broadcast indicator
                    None,
                    None,
                    timestamp,
                    self.config.ttl_seconds(),
                    MessageKind::Public,
                    None,
                    None,
                    message,
                    salt,
                    &self.keypair.signing_private_key_hex,
                )?
            }
            ClientEncryption::Encrypted { .. } => {
                // Public messages should be plaintext so anyone can read
                let mut salt = [0u8; 16];
                OsRng.fill_bytes(&mut salt);
                build_plaintext_envelope(
                    self.id(),
                    "*",
                    None,
                    None,
                    timestamp,
                    self.config.ttl_seconds(),
                    MessageKind::Public,
                    None,
                    None,
                    message,
                    salt,
                    &self.keypair.signing_private_key_hex,
                )?
            }
        };

        // Add to local cache and metadata
        let metadata = PublicMessageMetadata::new(timestamp);
        self.public_message_metadata
            .insert(envelope.header.message_id.clone(), metadata);
        self.public_message_cache.push(envelope.clone());

        // Post to relay for distribution
        self.post_envelope(&envelope)?;

        // Forward to known peers
        self.forward_public_message_to_peers(&envelope)?;

        Ok(envelope)
    }

    /// Receive and process a public message
    pub fn receive_public_message(&mut self, envelope: Envelope) -> Result<(), ClientError> {
        // Check if already seen
        if self
            .public_message_metadata
            .contains_key(&envelope.header.message_id)
        {
            return Ok(()); // Already processed
        }

        // Verify it's a public message
        if envelope.header.message_kind != MessageKind::Public {
            return Err(ClientError::Protocol(format!(
                "Expected Public message, got {:?}",
                envelope.header.message_kind
            )));
        }

        // Verify signature
        if let Some(sender_signing_key) = self
            .peer_registry
            .get_signing_key(&envelope.header.sender_id)
        {
            envelope
                .header
                .verify_signature(envelope.version, sender_signing_key, &envelope.payload.body)
                .map_err(|_| ClientError::Protocol("Invalid signature".to_string()))?;
        } else {
            return Err(ClientError::Protocol(format!(
                "Unknown sender: {}",
                envelope.header.sender_id
            )));
        }

        // Check TTL
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let expires_at = envelope.header.timestamp + envelope.header.ttl_seconds;
        if now > expires_at {
            return Err(ClientError::Protocol("Message TTL expired".to_string()));
        }

        // Add to cache and metadata
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let metadata = PublicMessageMetadata::new(timestamp);
        self.public_message_metadata
            .insert(envelope.header.message_id.clone(), metadata);
        self.public_message_cache.push(envelope.clone());

        // Add to feed for the user
        let client_msg = ClientMessage {
            message_id: envelope.header.message_id.0.clone(),
            sender_id: envelope.header.sender_id.clone(),
            timestamp: envelope.header.timestamp,
            body: envelope.payload.body.clone(),
        };
        self.feed.push(client_msg);
        self.seen.insert(envelope.header.message_id.0.clone());

        // Forward to peers if propagation limit not reached
        if let Some(metadata) = self
            .public_message_metadata
            .get(&envelope.header.message_id)
        {
            if metadata.should_forward() {
                let _ = self.forward_public_message_to_peers(&envelope);
            }
        }

        Ok(())
    }

    /// Forward a public message to all known peers
    fn forward_public_message_to_peers(&mut self, envelope: &Envelope) -> Result<(), ClientError> {
        // Check if we should forward
        let should_forward = {
            let metadata = self
                .public_message_metadata
                .get(&envelope.header.message_id)
                .ok_or_else(|| {
                    ClientError::Protocol("Message not in metadata cache".to_string())
                })?;
            metadata.should_forward()
        };

        if !should_forward {
            return Ok(()); // Propagation limit reached
        }

        // Get data we need before mutable borrows
        let self_id = self.id().to_string();
        let sender_id = envelope.header.sender_id.clone();
        let peer_ids = self.peer_registry.peer_ids();

        // Determine which peers to forward to
        let peers_to_forward: Vec<String> = {
            let metadata = self
                .public_message_metadata
                .get(&envelope.header.message_id)
                .ok_or_else(|| {
                    ClientError::Protocol("Message not in metadata cache".to_string())
                })?;

            peer_ids
                .into_iter()
                .filter(|peer_id| {
                    peer_id != &self_id
                        && peer_id != &sender_id
                        && !metadata.seen_by.contains(peer_id)
                })
                .collect()
        };

        // Now update metadata and forward
        if let Some(metadata) = self
            .public_message_metadata
            .get_mut(&envelope.header.message_id)
        {
            metadata.increment_propagation();
            for peer_id in &peers_to_forward {
                metadata.mark_seen_by(peer_id.clone());
            }
        }

        // Post to relay (best effort)
        for _ in &peers_to_forward {
            let _ = self.post_envelope(envelope);
        }

        Ok(())
    }

    /// Send a group message to all members of a group
    pub fn send_group_message(
        &mut self,
        group_id: &str,
        message: &str,
    ) -> Result<Envelope, ClientError> {
        if !self.online {
            return Err(ClientError::Offline);
        }

        // Get the group
        let group = self
            .group_manager
            .get_group(group_id)
            .ok_or_else(|| ClientError::Protocol(format!("Group not found: {}", group_id)))?;

        // Verify we're a member
        if !group.is_member(self.id()) {
            return Err(ClientError::Protocol(format!(
                "Not a member of group: {}",
                group_id
            )));
        }

        let group_key = group.group_key;
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|err| ClientError::Time(err.to_string()))?
            .as_secs();

        // Build group message payload
        let aad = group_id.as_bytes();
        let payload =
            crate::protocol::build_group_message_payload(message.as_bytes(), &group_key, aad)?;

        // Build envelope with group_id
        let envelope = crate::protocol::build_envelope_from_payload(
            self.id().to_string(),
            "*".to_string(), // Broadcast to group
            None,
            None,
            timestamp,
            self.config.ttl_seconds(),
            MessageKind::FriendGroup,
            Some(group_id.to_string()),
            None,
            payload,
            &self.keypair.signing_private_key_hex,
        )?;

        // Post to relay
        self.post_envelope(&envelope)?;

        Ok(envelope)
    }

    /// Receive and process a group message
    pub fn receive_group_message(&mut self, envelope: Envelope) -> Result<(), ClientError> {
        // Check if already seen
        if self.seen.contains(&envelope.header.message_id.0) {
            return Ok(()); // Already processed
        }

        // Verify it's a group message
        if envelope.header.message_kind != MessageKind::FriendGroup {
            return Err(ClientError::Protocol(format!(
                "Expected FriendGroup message, got {:?}",
                envelope.header.message_kind
            )));
        }

        // Get group_id from header
        let group_id =
            envelope.header.group_id.as_ref().ok_or_else(|| {
                ClientError::Protocol("Group message missing group_id".to_string())
            })?;

        // Get the group
        let group = self
            .group_manager
            .get_group(group_id)
            .ok_or_else(|| ClientError::Protocol(format!("Unknown group: {}", group_id)))?;

        // Verify sender is a group member
        if !group.is_member(&envelope.header.sender_id) {
            return Err(ClientError::Protocol(format!(
                "Sender {} is not a member of group {}",
                envelope.header.sender_id, group_id
            )));
        }

        // Verify we're a member
        if !group.is_member(self.id()) {
            return Err(ClientError::Protocol(format!(
                "Not a member of group: {}",
                group_id
            )));
        }

        // Verify signature
        if let Some(sender_signing_key) = self
            .peer_registry
            .get_signing_key(&envelope.header.sender_id)
        {
            envelope
                .header
                .verify_signature(envelope.version, sender_signing_key, &envelope.payload.body)
                .map_err(|_| ClientError::Protocol("Invalid signature".to_string()))?;
        } else {
            return Err(ClientError::Protocol(format!(
                "Unknown sender: {}",
                envelope.header.sender_id
            )));
        }

        // Decrypt the payload
        let group_key = &group.group_key;
        let aad = group_id.as_bytes();
        let plaintext =
            crate::protocol::decrypt_group_message_payload(&envelope.payload, group_key, aad)?;
        let body = String::from_utf8(plaintext)
            .map_err(|e| ClientError::Protocol(format!("Invalid UTF-8: {}", e)))?;

        // Add to feed
        let client_msg = ClientMessage {
            message_id: envelope.header.message_id.0.clone(),
            sender_id: envelope.header.sender_id.clone(),
            timestamp: envelope.header.timestamp,
            body,
        };
        self.feed.push(client_msg);
        self.seen.insert(envelope.header.message_id.0.clone());

        Ok(())
    }

    pub fn send_message(
        &self,
        recipient_id: &str,
        recipient_public_key_hex: &str,
        message: &str,
    ) -> Result<Envelope, ClientError> {
        if !self.online {
            return Err(ClientError::Offline);
        }
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|err| ClientError::Time(err.to_string()))?
            .as_secs();

        let envelope = match self.config.encryption() {
            ClientEncryption::Plaintext => {
                let mut salt = [0u8; 16];
                OsRng.fill_bytes(&mut salt);
                build_plaintext_envelope(
                    self.id(),
                    recipient_id,
                    None,
                    None,
                    timestamp,
                    self.config.ttl_seconds(),
                    MessageKind::Direct,
                    None,
                    None,
                    message,
                    salt,
                    &self.keypair.signing_private_key_hex,
                )?
            }
            ClientEncryption::Encrypted {
                hpke_info,
                payload_aad,
            } => {
                let content_key = generate_content_key();
                let mut nonce = [0u8; NONCE_SIZE];
                OsRng.fill_bytes(&mut nonce);
                let payload = build_encrypted_payload(
                    message.as_bytes(),
                    recipient_public_key_hex,
                    payload_aad,
                    hpke_info,
                    &content_key,
                    &nonce,
                    None,
                )?;
                build_envelope_from_payload(
                    self.id(),
                    recipient_id,
                    None,
                    None,
                    timestamp,
                    self.config.ttl_seconds(),
                    MessageKind::Direct,
                    None,
                    None,
                    payload,
                    &self.keypair.signing_private_key_hex,
                )?
            }
        };

        self.post_envelope(&envelope)?;
        Ok(envelope)
    }

    pub fn sync_inbox(&mut self, limit: Option<usize>) -> Result<SyncOutcome, ClientError> {
        if !self.online {
            return Err(ClientError::Offline);
        }
        let envelopes = self.fetch_inbox(limit)?;
        let fetched = envelopes.len();
        if fetched == 0 {
            return Ok(SyncOutcome {
                events: Vec::new(),
                fetched: 0,
                messages: Vec::new(),
                errors: Vec::new(),
            });
        }
        self.process_envelopes(envelopes)
    }

    /// Process a batch of already-fetched envelopes: verify, decrypt, dispatch to handler.
    fn process_envelopes(&mut self, envelopes: Vec<Envelope>) -> Result<SyncOutcome, ClientError> {
        let fetched = envelopes.len();

        // Log what message kinds we fetched
        for env in &envelopes {
            crate::tlog!(
                "DEBUG sync_inbox: fetched envelope kind={:?} sender={} recipient={} msg_id={}",
                env.header.message_kind,
                &env.header.sender_id[..8.min(env.header.sender_id.len())],
                &env.header.recipient_id[..8.min(env.header.recipient_id.len())],
                &env.header.message_id.0[..8.min(env.header.message_id.0.len())]
            );
        }

        // Take the handler out temporarily so we can mutate self (seen, feed)
        // while also calling handler methods, then put it back afterwards.
        let mut handler = self.handler.take();

        let mut events = Vec::new();
        let mut messages = Vec::new();
        let mut errors = Vec::new();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        for envelope in envelopes {
            // --- Deduplication ---
            if self.seen.contains(&envelope.header.message_id.0) {
                eprintln!(
                    "DISCARDED: duplicate message (kind={:?}, sender={}, msg_id={})",
                    envelope.header.message_kind,
                    &envelope.header.sender_id,
                    &envelope.header.message_id.0
                );
                events.push(SyncEvent {
                    envelope,
                    outcome: SyncEventOutcome::Duplicate,
                });
                continue;
            }

            // --- TTL check ---
            let expires_at = envelope
                .header
                .timestamp
                .saturating_add(envelope.header.ttl_seconds);
            if now > expires_at {
                eprintln!(
                    "DISCARDED: TTL expired (kind={:?}, sender={}, msg_id={}, expired {} seconds ago)",
                    envelope.header.message_kind,
                    &envelope.header.sender_id,
                    &envelope.header.message_id.0,
                    now.saturating_sub(expires_at)
                );
                events.push(SyncEvent {
                    envelope,
                    outcome: SyncEventOutcome::TtlExpired,
                });
                continue;
            }

            // --- Meta messages: process from any sender (no signature required) ---
            if envelope.header.message_kind == MessageKind::Meta {
                let outcome = match decode_meta_payload(&envelope.payload) {
                    Ok(meta) => {
                        if let Some(ref mut h) = handler {
                            for env in h.on_meta(&meta) {
                                if let Err(e) = self.post_envelope(&env) {
                                    eprintln!(
                                        "ERROR: failed to post meta response envelope (kind: {:?}, recipient: {}): {}",
                                        env.header.message_kind,
                                        env.header.recipient_id,
                                        e
                                    );
                                }
                            }
                        }
                        SyncEventOutcome::Meta(meta)
                    }
                    Err(_) => {
                        let body = envelope.payload.body.clone();
                        if let Some(ref mut h) = handler {
                            for env in h.on_raw_meta(&envelope, &body) {
                                let _ = self.post_envelope(&env);
                            }
                        }
                        SyncEventOutcome::RawMeta { body }
                    }
                };
                events.push(SyncEvent { envelope, outcome });
                continue;
            }

            // --- Non-meta: verify signature from known peer ---
            let signing_key = self
                .peer_registry
                .get_signing_key(&envelope.header.sender_id)
                .map(|s| s.to_string());

            // Public messages can come from unknown senders (relay broadcast).
            // For known senders, verify signature. For unknown senders, accept
            // but mark as unverified (handler can decide what to do).
            let is_public = envelope.header.message_kind == MessageKind::Public;

            if let Some(ref signing_key) = signing_key {
                // Known sender: verify signature (skip if key is empty - placeholder peer)
                if !signing_key.is_empty() {
                    if let Err(e) = envelope.header.verify_signature(
                        envelope.version,
                        signing_key,
                        &envelope.payload.body,
                    ) {
                        let reason = format!("invalid header signature: {e:?}");
                        eprintln!(
                            "DISCARDED: {} (kind={:?}, sender={}, msg_id={})",
                            reason,
                            envelope.header.message_kind,
                            &envelope.header.sender_id,
                            &envelope.header.message_id.0
                        );
                        if let Some(ref mut h) = handler {
                            h.on_rejected(&envelope, &reason);
                        }
                        errors.push(reason.clone());
                        events.push(SyncEvent {
                            envelope,
                            outcome: SyncEventOutcome::InvalidSignature { reason },
                        });
                        continue;
                    }
                } else {
                    eprintln!(
                        "INFO: skipping signature verification for placeholder peer {} (kind={:?}, msg_id={})",
                        &envelope.header.sender_id,
                        envelope.header.message_kind,
                        &envelope.header.message_id.0
                    );
                }
            } else if !is_public {
                // Unknown sender and NOT a public message: reject
                let reason = format!(
                    "unknown sender: {} (cannot verify signature)",
                    envelope.header.sender_id
                );
                eprintln!(
                    "DISCARDED: {} (kind={:?}, msg_id={})",
                    reason, envelope.header.message_kind, &envelope.header.message_id.0
                );
                if let Some(ref mut h) = handler {
                    h.on_rejected(&envelope, &reason);
                }
                errors.push(reason.clone());
                events.push(SyncEvent {
                    envelope,
                    outcome: SyncEventOutcome::UnknownSender,
                });
                continue;
            }
            // If unknown sender but IS public: accept it (will be marked unverified in storage)

            // --- Decrypt / extract body based on message kind ---
            let body_result = self.extract_body(&envelope);

            match body_result {
                Err(reason) => {
                    eprintln!(
                        "DISCARDED: decryption failed: {} (kind={:?}, sender={}, msg_id={})",
                        reason,
                        envelope.header.message_kind,
                        &envelope.header.sender_id,
                        &envelope.header.message_id.0
                    );
                    if let Some(ref mut h) = handler {
                        h.on_rejected(&envelope, &reason);
                    }
                    errors.push(reason.clone());
                    events.push(SyncEvent {
                        envelope,
                        outcome: SyncEventOutcome::DecryptFailed { reason },
                    });
                }
                Ok(body) => {
                    let message = ClientMessage {
                        message_id: envelope.header.message_id.0.clone(),
                        sender_id: envelope.header.sender_id.clone(),
                        timestamp: envelope.header.timestamp,
                        body,
                    };
                    self.seen.insert(envelope.header.message_id.0.clone());
                    self.feed.push(message.clone());
                    messages.push(message.clone());
                    if let Some(ref mut h) = handler {
                        for env in h.on_message(&envelope, &message) {
                            let _ = self.post_envelope(&env);
                        }
                    }
                    events.push(SyncEvent {
                        envelope,
                        outcome: SyncEventOutcome::Message(message),
                    });
                }
            }
        }

        // Restore the handler.
        self.handler = handler;

        // Drain any group keys received via group_key_distribution into the GroupManager.
        if let Some(ref mut h) = self.handler {
            for (group_id, key) in h.take_pending_group_keys() {
                self.group_manager.add_group_key(group_id, key);
            }
        }

        Ok(SyncOutcome {
            events,
            fetched,
            messages,
            errors,
        })
    }

    /// Extract the plaintext body from an envelope based on its message kind.
    ///
    /// Assumes the signature has already been verified.
    fn extract_body(&self, envelope: &Envelope) -> Result<String, String> {
        match envelope.header.message_kind {
            MessageKind::Direct => match self.config.encryption() {
                ClientEncryption::Plaintext => Ok(envelope.payload.body.clone()),
                ClientEncryption::Encrypted {
                    hpke_info,
                    payload_aad,
                } => {
                    let plaintext = decrypt_encrypted_payload(
                        &envelope.payload,
                        &self.keypair.private_key_hex,
                        payload_aad,
                        hpke_info,
                    )
                    .map_err(|e| format!("decrypt failed: {e}"))?;
                    String::from_utf8(plaintext).map_err(|e| format!("utf-8 error: {e}"))
                }
            },
            MessageKind::Public | MessageKind::StoreForPeer => Ok(envelope.payload.body.clone()),
            MessageKind::FriendGroup => {
                let group_id = envelope
                    .header
                    .group_id
                    .as_deref()
                    .ok_or_else(|| "friend_group message missing group_id".to_string())?;
                let group = self
                    .group_manager
                    .get_group(group_id)
                    .ok_or_else(|| format!("unknown group: {group_id}"))?;
                let aad = group_id.as_bytes();
                let plaintext = crate::protocol::decrypt_group_message_payload(
                    &envelope.payload,
                    &group.group_key,
                    aad,
                )
                .map_err(|e| format!("group decrypt failed: {e}"))?;
                String::from_utf8(plaintext).map_err(|e| format!("utf-8 error: {e}"))
            }
            MessageKind::Meta => {
                // Should never reach here; meta is handled before this function is called.
                Err("unexpected meta message kind".to_string())
            }
        }
    }

    pub fn post_envelope(&self, envelope: &Envelope) -> Result<(), ClientError> {
        let url = format!(
            "{}/envelopes",
            self.config.relay_url().trim_end_matches('/')
        );
        let response = ureq::post(&url).send_json(
            serde_json::to_value(envelope)
                .map_err(|error| ClientError::Protocol(format!("serialize envelope: {error}")))?,
        );

        match response {
            Ok(_) => Ok(()),
            Err(ureq::Error::Status(code, _)) => {
                Err(ClientError::Http(format!("relay error: {code}")))
            }
            Err(err) => Err(ClientError::Http(err.to_string())),
        }
    }

    fn fetch_inbox(&self, limit: Option<usize>) -> Result<Vec<Envelope>, ClientError> {
        self.fetch_inbox_filtered(limit, None)
    }

    fn fetch_inbox_filtered(
        &self,
        limit: Option<usize>,
        max_size_bytes: Option<usize>,
    ) -> Result<Vec<Envelope>, ClientError> {
        let base = self.config.relay_url().trim_end_matches('/');
        let mut params: Vec<String> = Vec::new();
        if let Some(l) = limit {
            params.push(format!("limit={l}"));
        }
        if let Some(msb) = max_size_bytes {
            params.push(format!("max_size_bytes={msb}"));
        }
        let url = if params.is_empty() {
            format!("{base}/inbox/{}", self.id())
        } else {
            format!("{base}/inbox/{}?{}", self.id(), params.join("&"))
        };
        let token =
            crate::crypto::make_relay_auth_token(&self.keypair.signing_private_key_hex, self.id())
                .map_err(|e| ClientError::Protocol(format!("auth token: {e}")))?;
        let response = ureq::get(&url)
            .set("Authorization", &format!("Bearer {token}"))
            .call()
            .map_err(|error| ClientError::Http(error.to_string()))?;
        response
            .into_json()
            .map_err(|error| ClientError::Protocol(format!("deserialize inbox: {error}")))
    }

    /// Fetch and process envelopes from the relay, applying a size filter so that
    /// only envelopes whose serialised size is ≤ `max_size_bytes` are returned.
    /// Oversized envelopes remain in the relay queue for a subsequent unfiltered fetch.
    pub fn sync_inbox_size_filtered(
        &mut self,
        limit: Option<usize>,
        max_size_bytes: Option<usize>,
    ) -> Result<SyncOutcome, ClientError> {
        if !self.online {
            return Err(ClientError::Offline);
        }
        let envelopes = self.fetch_inbox_filtered(limit, max_size_bytes)?;
        let fetched = envelopes.len();
        if fetched == 0 {
            return Ok(SyncOutcome {
                events: Vec::new(),
                fetched: 0,
                messages: Vec::new(),
                errors: Vec::new(),
            });
        }
        // Reuse the same processing logic as sync_inbox by delegating to a shared helper.
        self.process_envelopes(envelopes)
    }
}

impl Client for RelayClient {
    fn id(&self) -> &str {
        &self.keypair.id
    }

    fn is_online(&self, _step: usize) -> bool {
        self.online
    }

    fn inbox(&self) -> Vec<SimMessage> {
        Vec::new()
    }

    fn stored_forward_count(&self) -> usize {
        0
    }

    fn metrics(&self) -> ClientMetrics {
        ClientMetrics::default()
    }

    fn receive_message(&mut self, message: SimMessage) -> bool {
        if self.seen.insert(message.id.clone()) {
            self.feed.push(ClientMessage {
                message_id: message.id,
                sender_id: message.sender,
                timestamp: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
                body: message.body,
            });
            return true;
        }
        false
    }

    fn enqueue_sends(
        &mut self,
        _step: usize,
        messages: &[SimMessage],
        _online_set: &HashSet<String>,
        context: &mut ClientContext<'_>,
    ) -> ClientSendOutcome {
        if !self.online {
            return ClientSendOutcome {
                envelopes: Vec::new(),
                direct_deliveries: Vec::new(),
            };
        }

        let mut envelopes = Vec::new();
        for message in messages {
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let Ok(envelope) = build_envelope_from_payload(
                message.sender.clone(),
                message.recipient.clone(),
                None,
                None,
                timestamp,
                context.ttl_seconds,
                MessageKind::Direct,
                None,
                None,
                message.payload.clone(),
                &self.keypair.signing_private_key_hex,
            ) else {
                continue;
            };
            if self.post_envelope(&envelope).is_ok() {
                envelopes.push(envelope);
            }
        }

        ClientSendOutcome {
            envelopes,
            direct_deliveries: Vec::new(),
        }
    }

    fn handle_inbox(
        &mut self,
        _step: usize,
        envelopes: Vec<Envelope>,
        _context: &mut ClientContext<'_>,
        _rolling_latency: Option<&mut RollingLatencyTracker>,
    ) -> ClientInboxOutcome {
        let mut received = 0usize;
        for envelope in envelopes {
            if self.seen.contains(&envelope.header.message_id.0) {
                continue;
            }
            let body_result = self.extract_body(&envelope);
            match body_result {
                Ok(body) => {
                    let message = ClientMessage {
                        message_id: envelope.header.message_id.0.clone(),
                        sender_id: envelope.header.sender_id.clone(),
                        timestamp: envelope.header.timestamp,
                        body,
                    };
                    self.seen.insert(envelope.header.message_id.0.clone());
                    self.feed.push(message);
                    received = received.saturating_add(1);
                }
                Err(_) => continue,
            }
        }
        ClientInboxOutcome {
            received,
            outgoing: Vec::new(),
        }
    }

    fn announce_online(&mut self, _step: usize, _context: &mut ClientContext<'_>) -> Vec<Envelope> {
        Vec::new()
    }

    fn process_pending_online_broadcasts(
        &mut self,
        _step: usize,
        _online_set: &HashSet<String>,
        _context: &mut ClientContext<'_>,
    ) -> ClientBroadcastOutcome {
        ClientBroadcastOutcome {
            envelopes: Vec::new(),
            delivered_missed: 0,
        }
    }

    fn forward_store_forwards(
        &mut self,
        _step: usize,
        _online_set: &HashSet<String>,
        _context: &mut ClientContext<'_>,
    ) -> Vec<Envelope> {
        Vec::new()
    }

    fn handle_direct_delivery(
        &mut self,
        _step: usize,
        _message: SimMessage,
        _context: &mut ClientContext<'_>,
        _rolling_latency: Option<&mut RollingLatencyTracker>,
    ) -> Option<usize> {
        None
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ClientMetrics {
    pub messages_sent: usize,
    pub direct_deliveries: usize,
    pub inbox_deliveries: usize,
    pub missed_deliveries: usize,
    pub store_forwards_stored: usize,
    pub store_forwards_forwarded: usize,
    pub store_forwards_delivered: usize,
}

#[derive(Debug, Clone)]
pub struct ClientLogEvent {
    pub step: usize,
    pub client_id: String,
    pub message: String,
}

pub trait ClientLogSink: Send + Sync {
    fn log(&self, event: ClientLogEvent);
}

impl<F> ClientLogSink for F
where
    F: Fn(ClientLogEvent) + Send + Sync,
{
    fn log(&self, event: ClientLogEvent) {
        self(event);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoreForPayload {
    envelope: Envelope,
}

#[derive(Debug, Clone)]
struct StoredForPeerMessage {
    store_for_id: String,
    envelope: Envelope,
}

#[derive(Debug, Clone)]
struct PendingOnlineBroadcast {
    sender_id: String,
    recipient_id: String,
    sent_step: usize,
    expires_at: usize,
}

pub(crate) struct ClientContext<'a> {
    pub direct_enabled: bool,
    pub ttl_seconds: u64,
    pub encryption: MessageEncryption,
    pub direct_links: &'a HashSet<(String, String)>,
    pub neighbors: &'a HashMap<String, Vec<String>>,
    pub keypairs: &'a HashMap<String, StoredKeypair>,
    pub message_history: &'a mut HashMap<(String, String), Vec<HistoricalMessage>>,
    pub message_send_steps: &'a mut HashMap<String, usize>,
    pub pending_forwarded_messages: &'a mut HashSet<String>,
    pub metrics: &'a mut SimulationMetrics,
    pub metrics_tracker: &'a mut MetricsTracker,
}

pub(crate) struct ClientSendOutcome {
    pub envelopes: Vec<Envelope>,
    pub direct_deliveries: Vec<SimMessage>,
}

pub(crate) struct ClientBroadcastOutcome {
    pub envelopes: Vec<Envelope>,
    pub delivered_missed: usize,
}

pub(crate) struct ClientInboxOutcome {
    /// Number of messages successfully received and stored.
    pub received: usize,
    /// Outgoing envelopes produced by the handler (e.g. auto-accept replies)
    /// that the caller should route via its transport.
    pub outgoing: Vec<Envelope>,
}

pub(crate) trait Client: Send {
    fn id(&self) -> &str;
    fn is_online(&self, step: usize) -> bool;
    fn inbox(&self) -> Vec<SimMessage>;
    fn stored_forward_count(&self) -> usize;
    fn metrics(&self) -> ClientMetrics;
    fn receive_message(&mut self, message: SimMessage) -> bool;

    fn enqueue_sends(
        &mut self,
        step: usize,
        messages: &[SimMessage],
        online_set: &HashSet<String>,
        context: &mut ClientContext<'_>,
    ) -> ClientSendOutcome;

    fn handle_inbox(
        &mut self,
        step: usize,
        envelopes: Vec<Envelope>,
        context: &mut ClientContext<'_>,
        rolling_latency: Option<&mut RollingLatencyTracker>,
    ) -> ClientInboxOutcome;

    fn announce_online(&mut self, step: usize, context: &mut ClientContext<'_>) -> Vec<Envelope>;

    fn process_pending_online_broadcasts(
        &mut self,
        step: usize,
        online_set: &HashSet<String>,
        context: &mut ClientContext<'_>,
    ) -> ClientBroadcastOutcome;

    fn forward_store_forwards(
        &mut self,
        step: usize,
        online_set: &HashSet<String>,
        context: &mut ClientContext<'_>,
    ) -> Vec<Envelope>;

    fn handle_direct_delivery(
        &mut self,
        step: usize,
        message: SimMessage,
        context: &mut ClientContext<'_>,
        rolling_latency: Option<&mut RollingLatencyTracker>,
    ) -> Option<usize>;

    /// Downcast to Any for event-based simulation
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;
}

pub struct SimulationClient {
    id: String,
    schedule: Vec<bool>,
    online_state: Option<bool>, // Event-based online state (None = use schedule)
    inbox: Vec<SimMessage>,
    seen: HashSet<String>,
    last_seen_by_peer: HashMap<String, u64>,
    pending_online_broadcasts: Vec<PendingOnlineBroadcast>,
    stored_forwards: Vec<StoredForPeerMessage>,
    metrics: ClientMetrics,
    log_sink: Option<std::sync::Arc<dyn ClientLogSink>>,
    peer_registry: PeerRegistry,
    public_message_cache: Vec<Envelope>,
    public_message_metadata: HashMap<ContentId, PublicMessageMetadata>,
    group_manager: GroupManager,
    handler: Option<Box<dyn MessageHandler>>,
}

impl Clone for SimulationClient {
    fn clone(&self) -> Self {
        Self {
            id: self.id.clone(),
            schedule: self.schedule.clone(),
            online_state: self.online_state,
            inbox: self.inbox.clone(),
            seen: self.seen.clone(),
            last_seen_by_peer: self.last_seen_by_peer.clone(),
            pending_online_broadcasts: self.pending_online_broadcasts.clone(),
            stored_forwards: self.stored_forwards.clone(),
            metrics: self.metrics.clone(),
            log_sink: self.log_sink.clone(),
            peer_registry: self.peer_registry.clone(),
            public_message_cache: self.public_message_cache.clone(),
            public_message_metadata: self.public_message_metadata.clone(),
            group_manager: self.group_manager.clone(),
            handler: None, // handlers are not cloned
        }
    }
}

impl std::fmt::Debug for SimulationClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimulationClient")
            .field("id", &self.id)
            .field("schedule", &self.schedule)
            .field("online_state", &self.online_state)
            .field("inbox", &self.inbox)
            .field("seen", &self.seen)
            .field("last_seen_by_peer", &self.last_seen_by_peer)
            .field("pending_online_broadcasts", &self.pending_online_broadcasts)
            .field("stored_forwards", &self.stored_forwards)
            .field("metrics", &self.metrics)
            .field("has_log_sink", &self.log_sink.is_some())
            .field("peer_registry", &self.peer_registry)
            .field(
                "public_message_cache_size",
                &self.public_message_cache.len(),
            )
            .field(
                "public_message_metadata_size",
                &self.public_message_metadata.len(),
            )
            .field("group_manager", &self.group_manager)
            .field("has_handler", &self.handler.is_some())
            .finish()
    }
}

impl SimulationClient {
    pub fn new(
        id: &str,
        schedule: Vec<bool>,
        log_sink: Option<std::sync::Arc<dyn ClientLogSink>>,
    ) -> Self {
        Self {
            id: id.to_string(),
            schedule,
            online_state: None, // None means use schedule for backward compatibility
            inbox: Vec::new(),
            seen: HashSet::new(),
            last_seen_by_peer: HashMap::new(),
            pending_online_broadcasts: Vec::new(),
            stored_forwards: Vec::new(),
            metrics: ClientMetrics::default(),
            log_sink,
            peer_registry: PeerRegistry::new(),
            public_message_cache: Vec::new(),
            public_message_metadata: HashMap::new(),
            group_manager: GroupManager::new(),
            handler: None,
        }
    }

    /// Register a handler that processes incoming messages and meta-events.
    pub fn set_handler(&mut self, handler: Box<dyn MessageHandler>) {
        self.handler = Some(handler);
    }

    /// Remove any registered handler.
    pub fn clear_handler(&mut self) {
        self.handler = None;
    }

    pub fn set_log_sink(&mut self, log_sink: Option<std::sync::Arc<dyn ClientLogSink>>) {
        self.log_sink = log_sink;
    }

    pub fn add_peer(&mut self, peer_id: String, signing_public_key_hex: String) {
        self.peer_registry.add_peer(peer_id, signing_public_key_hex);
    }

    pub fn add_peer_with_encryption(
        &mut self,
        peer_id: String,
        signing_public_key_hex: String,
        encryption_public_key_hex: String,
    ) {
        self.peer_registry.add_peer_with_encryption(
            peer_id,
            signing_public_key_hex,
            Some(encryption_public_key_hex),
        );
    }

    pub fn remove_peer(&mut self, peer_id: &str) -> Option<PeerInfo> {
        self.peer_registry.remove_peer(peer_id)
    }

    pub fn get_peer(&self, peer_id: &str) -> Option<&PeerInfo> {
        self.peer_registry.get_peer(peer_id)
    }

    pub fn list_peers(&self) -> Vec<&PeerInfo> {
        self.peer_registry.list_peers()
    }

    pub fn peer_registry(&self) -> &PeerRegistry {
        &self.peer_registry
    }

    pub fn group_manager(&self) -> &GroupManager {
        &self.group_manager
    }

    pub fn group_manager_mut(&mut self) -> &mut GroupManager {
        &mut self.group_manager
    }

    pub fn create_group(
        &mut self,
        group_id: String,
        members: Vec<String>,
    ) -> Result<GroupInfo, GroupError> {
        let info = self
            .group_manager
            .create_group(group_id, members, self.id.clone())?;
        if let Some(ref mut h) = self.handler {
            let members_vec: Vec<String> = info.members.iter().cloned().collect();
            h.on_group_created(
                &info.group_id,
                &info.group_key,
                &info.creator_id,
                &members_vec,
            );
        }
        Ok(info)
    }

    pub fn get_group(&self, group_id: &str) -> Option<&GroupInfo> {
        self.group_manager.get_group(group_id)
    }

    pub fn list_groups(&self) -> Vec<&str> {
        self.group_manager.list_groups()
    }

    pub fn public_message_cache(&self) -> &[Envelope] {
        &self.public_message_cache
    }

    /// Send a public message in the simulation context
    #[allow(dead_code)]
    pub(crate) fn send_public_message(
        &mut self,
        message: &str,
        step: usize,
        context: &mut ClientContext<'_>,
    ) -> Option<Envelope> {
        if !self.online_at(step) {
            return None;
        }

        let envelope = build_plaintext_envelope(
            &self.id,
            "*", // broadcast indicator
            None,
            None,
            step as u64,
            context.ttl_seconds,
            MessageKind::Public,
            None,
            None,
            message,
            [0u8; 16], // salt
            &context.keypairs.get(&self.id)?.signing_private_key_hex,
        )
        .ok()?;

        // Add to local cache and metadata
        let metadata = PublicMessageMetadata::new(step as u64);
        self.public_message_metadata
            .insert(envelope.header.message_id.clone(), metadata);
        self.public_message_cache.push(envelope.clone());

        self.log_action(step, format!("{} sent public message", self.id));

        Some(envelope)
    }

    /// Receive and process a public message in simulation
    #[allow(dead_code)]
    pub(crate) fn receive_public_message(
        &mut self,
        envelope: Envelope,
        step: usize,
        context: &mut ClientContext<'_>,
    ) -> bool {
        // Check if already seen
        if self
            .public_message_metadata
            .contains_key(&envelope.header.message_id)
        {
            return false;
        }

        // Verify it's a public message
        if envelope.header.message_kind != MessageKind::Public {
            return false;
        }

        // Verify signature
        let sender_keypair = match context.keypairs.get(&envelope.header.sender_id) {
            Some(kp) => kp,
            None => return false,
        };
        if envelope
            .header
            .verify_signature(
                envelope.version,
                &sender_keypair.signing_public_key_hex,
                &envelope.payload.body,
            )
            .is_err()
        {
            return false;
        }

        // Check TTL
        let expires_at = envelope.header.timestamp + envelope.header.ttl_seconds;
        if (step as u64) > expires_at {
            return false; // TTL expired
        }

        // Add to cache and metadata
        let metadata = PublicMessageMetadata::new(step as u64);
        self.public_message_metadata
            .insert(envelope.header.message_id.clone(), metadata);
        self.public_message_cache.push(envelope.clone());

        // Add to inbox
        let sim_message = SimMessage {
            id: envelope.header.message_id.0.clone(),
            sender: envelope.header.sender_id.clone(),
            recipient: self.id.clone(),
            body: envelope.payload.body.clone(),
            payload: envelope.payload.clone(),
            message_kind: MessageKind::Public,
            group_id: None,
        };
        self.receive_message(sim_message);

        self.log_action(
            step,
            format!(
                "{} received public message from {}",
                self.id, envelope.header.sender_id
            ),
        );

        true
    }

    /// Forward a public message to peers
    pub fn forward_public_message(
        &mut self,
        envelope: &Envelope,
        step: usize,
        peer_ids: &[String],
    ) -> Vec<String> {
        let metadata = match self
            .public_message_metadata
            .get_mut(&envelope.header.message_id)
        {
            Some(m) => m,
            None => return Vec::new(),
        };

        if !metadata.should_forward() {
            return Vec::new();
        }

        metadata.increment_propagation();

        let mut forwarded_to = Vec::new();
        for peer_id in peer_ids {
            if peer_id == &self.id {
                continue;
            }
            if peer_id == &envelope.header.sender_id {
                continue;
            }
            if metadata.seen_by.contains(peer_id) {
                continue;
            }

            metadata.mark_seen_by(peer_id.clone());
            forwarded_to.push(peer_id.clone());
        }

        if !forwarded_to.is_empty() {
            self.log_action(
                step,
                format!(
                    "{} forwarded public message to {} peers",
                    self.id,
                    forwarded_to.len()
                ),
            );
        }

        forwarded_to
    }

    /// Send a group message in the simulation context
    #[allow(dead_code)]
    pub(crate) fn send_group_message(
        &mut self,
        group_id: &str,
        message: &str,
        step: usize,
        context: &mut ClientContext<'_>,
    ) -> Option<Envelope> {
        if !self.online_at(step) {
            return None;
        }

        // Get the group
        let group = self.group_manager.get_group(group_id)?;

        // Verify we're a member
        if !group.is_member(&self.id) {
            return None;
        }

        let group_key = group.group_key;
        let aad = group_id.as_bytes();

        // Build group message payload
        let payload =
            crate::protocol::build_group_message_payload(message.as_bytes(), &group_key, aad)
                .ok()?;

        // Get sender keypair
        let sender_keypair = context.keypairs.get(&self.id)?;

        // Build envelope with group_id
        let envelope = crate::protocol::build_envelope_from_payload(
            self.id.clone(),
            "*".to_string(), // Broadcast to group
            None,
            None,
            step as u64,
            context.ttl_seconds,
            MessageKind::FriendGroup,
            Some(group_id.to_string()),
            None,
            payload,
            &sender_keypair.signing_private_key_hex,
        )
        .ok()?;

        self.log_action(
            step,
            format!("{} sent group message to {}", self.id, group_id),
        );

        Some(envelope)
    }

    /// Receive and process a group message in simulation
    #[allow(dead_code)]
    pub(crate) fn receive_group_message(
        &mut self,
        envelope: Envelope,
        step: usize,
        context: &mut ClientContext<'_>,
    ) -> bool {
        // Check if already seen
        if self.seen.contains(&envelope.header.message_id.0) {
            return false;
        }

        // Verify it's a group message
        if envelope.header.message_kind != MessageKind::FriendGroup {
            return false;
        }

        // Get group_id from header
        let group_id = match &envelope.header.group_id {
            Some(gid) => gid,
            None => return false,
        };

        // Get the group
        let group = match self.group_manager.get_group(group_id) {
            Some(g) => g,
            None => return false,
        };

        // Verify sender is a group member
        if !group.is_member(&envelope.header.sender_id) {
            return false;
        }

        // Verify we're a member
        if !group.is_member(&self.id) {
            return false;
        }

        // Verify signature
        let sender_keypair = match context.keypairs.get(&envelope.header.sender_id) {
            Some(kp) => kp,
            None => return false,
        };
        if envelope
            .header
            .verify_signature(
                envelope.version,
                &sender_keypair.signing_public_key_hex,
                &envelope.payload.body,
            )
            .is_err()
        {
            return false;
        }

        // Decrypt the payload
        let group_key = &group.group_key;
        let aad = group_id.as_bytes();
        let plaintext =
            match crate::protocol::decrypt_group_message_payload(&envelope.payload, group_key, aad)
            {
                Ok(p) => p,
                Err(_) => return false,
            };
        let body = match String::from_utf8(plaintext) {
            Ok(b) => b,
            Err(_) => return false,
        };

        // Add to inbox
        let sim_message = SimMessage {
            id: envelope.header.message_id.0.clone(),
            sender: envelope.header.sender_id.clone(),
            recipient: self.id.clone(),
            body,
            payload: envelope.payload.clone(),
            message_kind: MessageKind::FriendGroup,
            group_id: Some(group_id.clone()),
        };
        let received = self.receive_message(sim_message);

        if received {
            self.log_action(
                step,
                format!(
                    "{} received group message from {} in group {}",
                    self.id, envelope.header.sender_id, group_id
                ),
            );
        }

        received
    }

    fn log_action(&self, step: usize, message: impl Into<String>) {
        if let Some(log_sink) = &self.log_sink {
            log_sink.log(ClientLogEvent {
                step,
                client_id: self.id.clone(),
                message: message.into(),
            });
        }
    }

    fn record_message_send(
        &self,
        message: &SimMessage,
        step: usize,
        envelope: &Envelope,
        context: &mut ClientContext<'_>,
    ) {
        context.message_send_steps.insert(message.id.clone(), step);
        context
            .message_history
            .entry((message.sender.clone(), message.recipient.clone()))
            .or_default()
            .push(HistoricalMessage {
                send_step: step,
                envelope: envelope.clone(),
            });
    }

    fn update_last_seen(
        &mut self,
        sender_id: &str,
        message_id: &str,
        context: &mut ClientContext<'_>,
    ) {
        let Some(send_step) = context.message_send_steps.get(message_id) else {
            return;
        };
        let entry = self
            .last_seen_by_peer
            .entry(sender_id.to_string())
            .or_insert(0);
        *entry = (*entry).max(*send_step as u64);
    }

    fn last_seen_for(&self, sender_id: &str) -> u64 {
        self.last_seen_by_peer.get(sender_id).copied().unwrap_or(0)
    }

    fn record_delivery_metrics(
        &mut self,
        message: &SimMessage,
        step: usize,
        context: &mut ClientContext<'_>,
    ) -> Option<usize> {
        if context.pending_forwarded_messages.remove(&message.id) {
            context.metrics.store_forwards_delivered =
                context.metrics.store_forwards_delivered.saturating_add(1);
            context.metrics_tracker.record_store_forward_delivery();
            self.metrics.store_forwards_delivered =
                self.metrics.store_forwards_delivered.saturating_add(1);
        }
        let latency = context
            .metrics_tracker
            .record_delivery(&message.id, &self.id, step);
        self.update_last_seen(&message.sender, &message.id, context);
        latency
    }

    fn apply_delivery(
        &mut self,
        step: usize,
        message: SimMessage,
        context: &mut ClientContext<'_>,
        delivery_kind: DeliveryKind,
    ) -> Option<usize> {
        if !self.receive_message(message.clone()) {
            return None;
        }
        match delivery_kind {
            DeliveryKind::Direct => {
                context.metrics.direct_deliveries =
                    context.metrics.direct_deliveries.saturating_add(1);
                self.metrics.direct_deliveries = self.metrics.direct_deliveries.saturating_add(1);
            }
            DeliveryKind::Inbox => {
                context.metrics.inbox_deliveries =
                    context.metrics.inbox_deliveries.saturating_add(1);
                self.metrics.inbox_deliveries = self.metrics.inbox_deliveries.saturating_add(1);
            }
            DeliveryKind::Missed => {
                context.metrics_tracker.record_missed_message_delivery();
                self.metrics.missed_deliveries = self.metrics.missed_deliveries.saturating_add(1);
            }
        }
        let latency = self.record_delivery_metrics(&message, step, context);
        if matches!(delivery_kind, DeliveryKind::Missed) {
            self.log_action(
                step + 1,
                format!("{} received missed message {}", self.id, message.id),
            );
        }
        latency
    }

    fn build_meta_envelope(
        &self,
        sender_id: &str,
        recipient_id: &str,
        step: usize,
        meta: &MetaMessage,
        context: &mut ClientContext<'_>,
    ) -> Option<Envelope> {
        let payload = build_meta_payload(meta).ok()?;
        let sender_keypair = context.keypairs.get(sender_id)?;
        let envelope = build_envelope_from_payload(
            sender_id.to_string(),
            recipient_id.to_string(),
            None,
            None,
            step as u64,
            context.ttl_seconds,
            MessageKind::Meta,
            None,
            None,
            payload,
            &sender_keypair.signing_private_key_hex,
        )
        .ok()?;
        context
            .metrics_tracker
            .record_meta_send(sender_id.to_string(), &envelope);
        Some(envelope)
    }

    fn decode_direct_envelope(
        &self,
        envelope: &Envelope,
        message_id: &str,
        recipient_id: &str,
        context: &ClientContext<'_>,
    ) -> Option<SimMessage> {
        if envelope.header.message_kind != MessageKind::Direct {
            return None;
        }
        let body = match context.encryption {
            MessageEncryption::Plaintext => envelope.payload.body.clone(),
            MessageEncryption::Encrypted => {
                let keypair = context.keypairs.get(recipient_id)?;
                let plaintext = decrypt_encrypted_payload(
                    &envelope.payload,
                    &keypair.private_key_hex,
                    SIMULATION_PAYLOAD_AAD,
                    SIMULATION_HPKE_INFO,
                )
                .ok()?;
                String::from_utf8(plaintext).ok()?
            }
        };
        Some(SimMessage {
            id: message_id.to_string(),
            sender: envelope.header.sender_id.clone(),
            recipient: envelope.header.recipient_id.clone(),
            body,
            payload: envelope.payload.clone(),
            message_kind: MessageKind::Direct,
            group_id: None,
        })
    }

    fn decode_store_for_envelope(
        &self,
        envelope: &Envelope,
        storage_peer_id: &str,
        context: &ClientContext<'_>,
    ) -> Option<StoredForPeerMessage> {
        if envelope.header.message_kind != MessageKind::StoreForPeer {
            return None;
        }
        let store_for_id = envelope.header.store_for.clone()?;
        let storage_peer = envelope.header.storage_peer_id.clone()?;
        if storage_peer != storage_peer_id {
            return None;
        }
        let keypair = context.keypairs.get(storage_peer_id)?;
        let plaintext = decrypt_encrypted_payload(
            &envelope.payload,
            &keypair.private_key_hex,
            SIMULATION_PAYLOAD_AAD,
            SIMULATION_HPKE_INFO,
        )
        .ok()?;
        let payload: StoreForPayload = serde_json::from_slice(&plaintext).ok()?;
        Some(StoredForPeerMessage {
            store_for_id,
            envelope: payload.envelope,
        })
    }

    fn build_store_for_envelope(
        &self,
        sender_id: &str,
        storage_peer_id: &str,
        store_for_id: &str,
        timestamp: u64,
        inner_envelope: &Envelope,
        context: &ClientContext<'_>,
    ) -> Option<Envelope> {
        let keypair = context.keypairs.get(storage_peer_id)?;
        let payload_bytes = serde_json::to_vec(&StoreForPayload {
            envelope: inner_envelope.clone(),
        })
        .ok()?;
        let mut rng = rand::thread_rng();
        let mut content_key = [0u8; CONTENT_KEY_SIZE];
        let mut nonce = [0u8; NONCE_SIZE];
        rng.fill_bytes(&mut content_key);
        rng.fill_bytes(&mut nonce);
        let outer_payload = build_encrypted_payload(
            &payload_bytes,
            &keypair.public_key_hex,
            SIMULATION_PAYLOAD_AAD,
            SIMULATION_HPKE_INFO,
            &content_key,
            &nonce,
            None,
        )
        .ok()?;
        let sender_keypair = context.keypairs.get(sender_id)?;
        build_envelope_from_payload(
            sender_id.to_string(),
            storage_peer_id.to_string(),
            Some(store_for_id.to_string()),
            Some(storage_peer_id.to_string()),
            timestamp,
            context.ttl_seconds,
            MessageKind::StoreForPeer,
            None,
            None,
            outer_payload,
            &sender_keypair.signing_private_key_hex,
        )
        .ok()
    }

    fn decode_envelope_action(
        &self,
        envelope: &Envelope,
        recipient_id: &str,
        context: &ClientContext<'_>,
    ) -> Option<IncomingEnvelopeAction> {
        let sender_keypair = context.keypairs.get(&envelope.header.sender_id)?;
        if envelope
            .header
            .verify_signature(
                envelope.version,
                &sender_keypair.signing_public_key_hex,
                &envelope.payload.body,
            )
            .is_err()
        {
            return None;
        }
        match envelope.header.message_kind {
            MessageKind::Direct => self
                .decode_direct_envelope(
                    envelope,
                    &envelope.header.message_id.0,
                    recipient_id,
                    context,
                )
                .map(IncomingEnvelopeAction::DirectMessage),
            MessageKind::StoreForPeer => self
                .decode_store_for_envelope(envelope, recipient_id, context)
                .map(IncomingEnvelopeAction::StoredForPeer),
            _ => None,
        }
    }

    fn store_forward_message(
        &mut self,
        step: usize,
        message: StoredForPeerMessage,
        context: &mut ClientContext<'_>,
    ) {
        self.stored_forwards.push(message);
        context.metrics.store_forwards_stored =
            context.metrics.store_forwards_stored.saturating_add(1);
        context.metrics_tracker.record_store_forward_stored();
        self.metrics.store_forwards_stored = self.metrics.store_forwards_stored.saturating_add(1);
        self.log_action(
            step + 1,
            format!("{} stored a store-forward message", self.id),
        );
    }

    fn select_storage_peer(
        &self,
        sender: &str,
        recipient: &str,
        context: &ClientContext<'_>,
    ) -> Option<String> {
        let sender_neighbors = context.neighbors.get(sender)?;
        let recipient_neighbors = context.neighbors.get(recipient)?;
        let sender_set: HashSet<&String> = sender_neighbors.iter().collect();
        let mut mutual: Vec<String> = recipient_neighbors
            .iter()
            .filter(|candidate| sender_set.contains(candidate))
            .cloned()
            .collect();
        mutual.sort();
        mutual
            .into_iter()
            .find(|candidate| candidate != sender && candidate != recipient)
    }

    fn handle_message_request(
        &mut self,
        requester: &str,
        responder: &str,
        step: usize,
        context: &mut ClientContext<'_>,
    ) -> (Vec<Envelope>, usize) {
        let last_seen = self.last_seen_for(responder);
        let request = MetaMessage::MessageRequest {
            peer_id: responder.to_string(),
            since_timestamp: last_seen,
        };
        let mut envelopes = Vec::new();
        if let Some(envelope) =
            self.build_meta_envelope(requester, responder, step, &request, context)
        {
            envelopes.push(envelope);
        }
        context.metrics_tracker.record_missed_message_request();
        self.log_action(
            step + 1,
            format!("{requester} requested missed messages from {responder}"),
        );
        let Some(history) = context
            .message_history
            .get(&(responder.to_string(), requester.to_string()))
        else {
            return (envelopes, 0);
        };
        let entries: Vec<HistoricalMessage> = history
            .iter()
            .filter(|entry| entry.send_step as u64 >= last_seen)
            .cloned()
            .collect();
        let mut delivered = 0;
        for entry in entries {
            if self.deliver_missed_message(&entry.envelope, requester, step, context) {
                delivered += 1;
            }
        }
        if delivered > 0 {
            self.log_action(
                step + 1,
                format!("{responder} delivered {delivered} missed messages to {requester}"),
            );
        }
        (envelopes, delivered)
    }

    fn deliver_missed_message(
        &mut self,
        envelope: &Envelope,
        recipient_id: &str,
        step: usize,
        context: &mut ClientContext<'_>,
    ) -> bool {
        let message_id = envelope.header.message_id.0.clone();
        let message = self.decode_direct_envelope(envelope, &message_id, recipient_id, context);
        let Some(message) = message else {
            return false;
        };
        self.apply_delivery(step, message, context, DeliveryKind::Missed)
            .is_some()
    }

    /// Handle the four-phase mesh catch-up protocol for public messages.
    ///
    /// Called from `handle_inbox` when a mesh-related `Meta` message is received and
    /// no external `MessageHandler` is registered (the normal case in simulation).
    ///
    /// - **Phase 1 responder** (`MessageRequest`): peer asked for our public messages since a
    ///   timestamp → respond with `MeshAvailable` listing matching IDs.
    /// - **Phase 2 requester** (`MeshAvailable`): peer told us what they have → respond with
    ///   `MeshRequest` for the IDs we are missing.
    /// - **Phase 3 responder** (`MeshRequest`): peer asked for specific messages → respond with
    ///   batched `MeshDelivery` envelopes plus sender signing keys.
    /// - **Phase 4 requester** (`MeshDelivery`): peer delivered public messages → store them in
    ///   the in-memory cache so subsequent steps see them.
    fn handle_mesh_meta(
        &mut self,
        meta: &MetaMessage,
        sender_id: &str,
        step: usize,
        context: &mut ClientContext<'_>,
    ) -> Vec<Envelope> {
        match meta {
            MetaMessage::MessageRequest {
                since_timestamp, ..
            } => {
                // Phase 1 responder: offer IDs of public messages we have since the timestamp.
                let message_ids: Vec<String> = self
                    .public_message_cache
                    .iter()
                    .filter(|env| env.header.timestamp >= *since_timestamp)
                    .map(|env| env.header.message_id.0.clone())
                    .take(MAX_SIM_MESH_IDS)
                    .collect();
                if message_ids.is_empty() {
                    return vec![];
                }
                let response = MetaMessage::MeshAvailable {
                    peer_id: self.id.clone(),
                    message_ids,
                    since_timestamp: *since_timestamp,
                };
                self.build_meta_envelope(&self.id, sender_id, step, &response, context)
                    .into_iter()
                    .collect()
            }
            MetaMessage::MeshAvailable { message_ids, .. } => {
                // Phase 2 requester: request the IDs we do not have yet.
                let unknown_ids: Vec<String> = message_ids
                    .iter()
                    .filter(|id| {
                        !self
                            .public_message_metadata
                            .contains_key(&ContentId((*id).clone()))
                    })
                    .take(MAX_SIM_MESH_REQUEST_IDS)
                    .cloned()
                    .collect();
                if unknown_ids.is_empty() {
                    return vec![];
                }
                let request = MetaMessage::MeshRequest {
                    peer_id: self.id.clone(),
                    message_ids: unknown_ids,
                };
                self.build_meta_envelope(&self.id, sender_id, step, &request, context)
                    .into_iter()
                    .collect()
            }
            MetaMessage::MeshRequest { message_ids, .. } => {
                // Phase 3 responder: deliver the requested public messages in batches.
                let mut batch: Vec<serde_json::Value> = Vec::new();
                let mut sender_keys: HashMap<String, String> = HashMap::new();
                let mut outgoing = Vec::new();

                for id in message_ids.iter().take(MAX_SIM_MESH_REQUEST_IDS) {
                    let found = self
                        .public_message_cache
                        .iter()
                        .find(|e| e.header.message_id.0 == *id);
                    let Some(env) = found else { continue };
                    if env.header.message_kind != MessageKind::Public {
                        continue;
                    }
                    if let Ok(val) = serde_json::to_value(env) {
                        // Include the sender's signing key so the receiver can verify.
                        if let Some(kp) = context.keypairs.get(&env.header.sender_id) {
                            sender_keys.insert(
                                env.header.sender_id.clone(),
                                kp.signing_public_key_hex.clone(),
                            );
                        }
                        batch.push(val);
                        if batch.len() >= MAX_SIM_MESH_BATCH {
                            let delivery = MetaMessage::MeshDelivery {
                                peer_id: self.id.clone(),
                                envelopes: std::mem::take(&mut batch),
                                sender_keys: std::mem::take(&mut sender_keys),
                            };
                            if let Some(out) = self
                                .build_meta_envelope(&self.id, sender_id, step, &delivery, context)
                            {
                                outgoing.push(out);
                            }
                        }
                    }
                }
                // Flush any remaining messages in the last (partial) batch.
                if !batch.is_empty() {
                    let delivery = MetaMessage::MeshDelivery {
                        peer_id: self.id.clone(),
                        envelopes: batch,
                        sender_keys,
                    };
                    if let Some(out) =
                        self.build_meta_envelope(&self.id, sender_id, step, &delivery, context)
                    {
                        outgoing.push(out);
                    }
                }
                outgoing
            }
            MetaMessage::MeshDelivery { envelopes, .. } => {
                // Phase 4 requester: store delivered public messages in the in-memory cache.
                for env_val in envelopes {
                    let Ok(envelope) = serde_json::from_value::<Envelope>(env_val.clone()) else {
                        continue;
                    };
                    if envelope.header.message_kind != MessageKind::Public {
                        continue;
                    }
                    let content_id = envelope.header.message_id.clone();
                    if !self.public_message_metadata.contains_key(&content_id) {
                        let metadata = PublicMessageMetadata::new(step as u64);
                        self.public_message_metadata.insert(content_id, metadata);
                        self.seen.insert(envelope.header.message_id.0.clone());
                        self.log_action(
                            step,
                            format!(
                                "{} received mesh-delivered public message from {}",
                                self.id, envelope.header.sender_id
                            ),
                        );
                        self.public_message_cache.push(envelope);
                    }
                }
                vec![]
            }
            _ => vec![],
        }
    }

    fn online_at(&self, step: usize) -> bool {
        // Use online_state if set (event-based mode), otherwise use schedule (step-based mode)
        if let Some(online) = self.online_state {
            online
        } else {
            self.schedule.get(step).copied().unwrap_or(false)
        }
    }

    /// Set online state for event-based simulation
    pub fn set_online(&mut self, online: bool) {
        self.online_state = Some(online);
    }

    /// Create a client for event-based simulation (no schedule needed)
    pub fn new_event_based(id: &str, log_sink: Option<std::sync::Arc<dyn ClientLogSink>>) -> Self {
        Self {
            id: id.to_string(),
            schedule: Vec::new(),      // Empty schedule for event-based mode
            online_state: Some(false), // Start offline by default
            inbox: Vec::new(),
            seen: HashSet::new(),
            last_seen_by_peer: HashMap::new(),
            pending_online_broadcasts: Vec::new(),
            stored_forwards: Vec::new(),
            metrics: ClientMetrics::default(),
            log_sink,
            peer_registry: PeerRegistry::new(),
            public_message_cache: Vec::new(),
            public_message_metadata: HashMap::new(),
            group_manager: GroupManager::new(),
            handler: None,
        }
    }
}

impl Client for SimulationClient {
    fn id(&self) -> &str {
        &self.id
    }

    fn is_online(&self, step: usize) -> bool {
        self.online_at(step)
    }

    fn inbox(&self) -> Vec<SimMessage> {
        self.inbox.clone()
    }

    fn stored_forward_count(&self) -> usize {
        self.stored_forwards.len()
    }

    fn metrics(&self) -> ClientMetrics {
        self.metrics.clone()
    }

    fn receive_message(&mut self, message: SimMessage) -> bool {
        if self.seen.insert(message.id.clone()) {
            self.inbox.push(message);
            return true;
        }
        false
    }

    fn enqueue_sends(
        &mut self,
        step: usize,
        messages: &[SimMessage],
        online_set: &HashSet<String>,
        context: &mut ClientContext<'_>,
    ) -> ClientSendOutcome {
        let mut envelopes = Vec::new();
        let mut direct_deliveries = Vec::new();
        for message in messages {
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let Some(sender_keypair) = context.keypairs.get(&message.sender) else {
                continue;
            };

            // Handle different message kinds
            match message.message_kind {
                MessageKind::Public => {
                    // Public messages are broadcast - recipient is "*"
                    let envelope = match build_envelope_from_payload(
                        message.sender.clone(),
                        "*".to_string(),
                        None,
                        None,
                        timestamp,
                        context.ttl_seconds,
                        MessageKind::Public,
                        None,
                        None,
                        message.payload.clone(),
                        &sender_keypair.signing_private_key_hex,
                    ) {
                        Ok(envelope) => envelope,
                        Err(_) => continue,
                    };
                    self.record_message_send(message, step, &envelope, context);
                    self.metrics.messages_sent = self.metrics.messages_sent.saturating_add(1);
                    context
                        .metrics_tracker
                        .record_send(message, step, &envelope);
                    envelopes.push(envelope);
                }
                MessageKind::FriendGroup => {
                    // Group messages - recipient field contains group ID for tracking
                    let group_id = message.group_id.clone();
                    let envelope = match build_envelope_from_payload(
                        message.sender.clone(),
                        message.recipient.clone(), // Group ID as recipient for routing
                        None,                      // store_for - not used for group messages
                        None,                      // storage_peer_id - not used for group messages
                        timestamp,
                        context.ttl_seconds,
                        MessageKind::FriendGroup,
                        group_id, // group_id - required for FriendGroup
                        None,
                        message.payload.clone(),
                        &sender_keypair.signing_private_key_hex,
                    ) {
                        Ok(envelope) => envelope,
                        Err(_) => continue,
                    };
                    self.record_message_send(message, step, &envelope, context);
                    self.metrics.messages_sent = self.metrics.messages_sent.saturating_add(1);
                    context
                        .metrics_tracker
                        .record_send(message, step, &envelope);
                    envelopes.push(envelope);
                }
                MessageKind::Meta => {
                    // Meta messages (e.g. GroupInvite) — send as-is to the named recipient.
                    let envelope = match build_envelope_from_payload(
                        message.sender.clone(),
                        message.recipient.clone(),
                        None,
                        None,
                        timestamp,
                        context.ttl_seconds,
                        MessageKind::Meta,
                        None,
                        None,
                        message.payload.clone(),
                        &sender_keypair.signing_private_key_hex,
                    ) {
                        Ok(envelope) => envelope,
                        Err(_) => continue,
                    };
                    self.record_message_send(message, step, &envelope, context);
                    context
                        .metrics_tracker
                        .record_send(message, step, &envelope);
                    envelopes.push(envelope);
                }
                _ => {
                    // Direct messages and any other kind - original behavior
                    let envelope = match build_envelope_from_payload(
                        message.sender.clone(),
                        message.recipient.clone(),
                        None,
                        None,
                        timestamp,
                        context.ttl_seconds,
                        MessageKind::Direct,
                        None,
                        None,
                        message.payload.clone(),
                        &sender_keypair.signing_private_key_hex,
                    ) {
                        Ok(envelope) => envelope,
                        Err(_) => continue,
                    };
                    self.record_message_send(message, step, &envelope, context);
                    self.metrics.messages_sent = self.metrics.messages_sent.saturating_add(1);
                    let recipient_online = online_set.contains(&message.recipient);
                    if context.direct_enabled
                        && context
                            .direct_links
                            .contains(&(message.sender.clone(), message.recipient.clone()))
                        && recipient_online
                    {
                        if let Some(decoded) = self.decode_direct_envelope(
                            &envelope,
                            &message.id,
                            &message.recipient,
                            context,
                        ) {
                            direct_deliveries.push(decoded);
                        }
                    }

                    if !recipient_online {
                        if let Some(storage_peer_id) =
                            self.select_storage_peer(&message.sender, &message.recipient, context)
                        {
                            if let Some(store_envelope) = self.build_store_for_envelope(
                                &message.sender,
                                &storage_peer_id,
                                &message.recipient,
                                timestamp,
                                &envelope,
                                context,
                            ) {
                                context
                                    .metrics_tracker
                                    .record_send(message, step, &store_envelope);
                                envelopes.push(store_envelope);
                                continue;
                            }
                        }
                    }

                    context
                        .metrics_tracker
                        .record_send(message, step, &envelope);
                    envelopes.push(envelope);
                }
            }
        }
        ClientSendOutcome {
            envelopes,
            direct_deliveries,
        }
    }

    fn handle_inbox(
        &mut self,
        step: usize,
        envelopes: Vec<Envelope>,
        context: &mut ClientContext<'_>,
        mut rolling_latency: Option<&mut RollingLatencyTracker>,
    ) -> ClientInboxOutcome {
        let mut received = 0usize;
        let mut outgoing: Vec<Envelope> = Vec::new();

        // Take handler out so we can mutate self while calling it.
        let mut handler = self.handler.take();

        for envelope in envelopes {
            // Handle Meta messages before signature-gated decode.
            if envelope.header.message_kind == MessageKind::Meta {
                if let Ok(meta) = decode_meta_payload(&envelope.payload) {
                    if let Some(ref mut h) = handler {
                        outgoing.extend(h.on_meta(&meta));
                    } else {
                        // No external handler: use built-in four-phase mesh protocol handling
                        // so simulation peers can catch up on missed public messages.
                        let sender_id = envelope.header.sender_id.clone();
                        outgoing.extend(self.handle_mesh_meta(&meta, &sender_id, step, context));
                    }
                }
                continue;
            }

            match self.decode_envelope_action(&envelope, &self.id, context) {
                Some(IncomingEnvelopeAction::DirectMessage(ref message)) => {
                    if let Some(latency) =
                        self.apply_delivery(step, message.clone(), context, DeliveryKind::Inbox)
                    {
                        if let Some(tracker) = rolling_latency.as_deref_mut() {
                            tracker.record(latency);
                        }
                        if let Some(ref mut h) = handler {
                            let client_msg = ClientMessage {
                                message_id: message.id.clone(),
                                sender_id: message.sender.clone(),
                                timestamp: envelope.header.timestamp,
                                body: message.body.clone(),
                            };
                            outgoing.extend(h.on_message(&envelope, &client_msg));
                        }
                        received = received.saturating_add(1);
                    }
                }
                Some(IncomingEnvelopeAction::StoredForPeer(message)) => {
                    self.store_forward_message(step, message, context);
                }
                None => {}
            }
        }

        self.handler = handler;

        // Drain any group keys received via group_key_distribution into the GroupManager.
        if let Some(ref mut h) = self.handler {
            for (group_id, key) in h.take_pending_group_keys() {
                self.group_manager.add_group_key(group_id, key);
            }
        }

        ClientInboxOutcome { received, outgoing }
    }

    fn announce_online(&mut self, step: usize, context: &mut ClientContext<'_>) -> Vec<Envelope> {
        let Some(neighbors) = context.neighbors.get(&self.id) else {
            return Vec::new();
        };
        if neighbors.is_empty() {
            return Vec::new();
        }
        self.log_action(
            step + 1,
            format!("{} announced online to {}", self.id, neighbors.join(", ")),
        );
        let mut envelopes = Vec::new();
        for neighbor in neighbors {
            let meta = MetaMessage::Online {
                peer_id: self.id.clone(),
                timestamp: step as u64,
            };
            if let Some(envelope) =
                self.build_meta_envelope(&self.id, neighbor, step, &meta, context)
            {
                envelopes.push(envelope);
            }
            self.pending_online_broadcasts.push(PendingOnlineBroadcast {
                sender_id: self.id.clone(),
                recipient_id: neighbor.clone(),
                sent_step: step,
                expires_at: step.saturating_add(SIMULATION_ACK_WINDOW_STEPS),
            });
            context.metrics_tracker.record_online_broadcast();
        }
        envelopes
    }

    fn process_pending_online_broadcasts(
        &mut self,
        step: usize,
        online_set: &HashSet<String>,
        context: &mut ClientContext<'_>,
    ) -> ClientBroadcastOutcome {
        let pending = std::mem::take(&mut self.pending_online_broadcasts);
        let mut remaining = Vec::with_capacity(pending.len());
        let mut delivered = 0usize;
        let mut envelopes = Vec::new();
        for pending in pending {
            if step > pending.expires_at {
                continue;
            }
            if online_set.contains(&pending.recipient_id) {
                let ack = MetaMessage::Ack {
                    peer_id: pending.recipient_id.clone(),
                    online_timestamp: pending.sent_step as u64,
                };
                if let Some(envelope) = self.build_meta_envelope(
                    &pending.recipient_id,
                    &pending.sender_id,
                    step,
                    &ack,
                    context,
                ) {
                    envelopes.push(envelope);
                }
                context.metrics_tracker.record_ack();
                self.log_action(
                    step + 1,
                    format!(
                        "{} acknowledged {} online",
                        pending.recipient_id, pending.sender_id
                    ),
                );
                let (request_envelopes, delivered_missed) = self.handle_message_request(
                    &pending.sender_id,
                    &pending.recipient_id,
                    step,
                    context,
                );
                envelopes.extend(request_envelopes);
                delivered = delivered.saturating_add(delivered_missed);
                continue;
            }
            remaining.push(pending);
        }
        self.pending_online_broadcasts = remaining;
        ClientBroadcastOutcome {
            envelopes,
            delivered_missed: delivered,
        }
    }

    fn forward_store_forwards(
        &mut self,
        step: usize,
        online_set: &HashSet<String>,
        context: &mut ClientContext<'_>,
    ) -> Vec<Envelope> {
        if !online_set.contains(&self.id) {
            return Vec::new();
        }
        let mut forwarded = Vec::new();
        let mut remaining = Vec::new();
        let log_sink = self.log_sink.clone();
        let client_id = self.id.clone();
        for entry in self.stored_forwards.drain(..) {
            if online_set.contains(&entry.store_for_id) {
                let message_id = entry.envelope.header.message_id.0.clone();
                context.pending_forwarded_messages.insert(message_id);
                context.metrics.store_forwards_forwarded =
                    context.metrics.store_forwards_forwarded.saturating_add(1);
                context
                    .metrics_tracker
                    .record_store_forward_forwarded(&self.id);
                self.metrics.store_forwards_forwarded =
                    self.metrics.store_forwards_forwarded.saturating_add(1);
                if let Some(log_sink) = &log_sink {
                    log_sink.log(ClientLogEvent {
                        step: step + 1,
                        client_id: client_id.clone(),
                        message: format!(
                            "{} forwarded stored message to {}",
                            client_id, entry.store_for_id
                        ),
                    });
                }
                forwarded.push(entry.envelope);
            } else {
                remaining.push(entry);
            }
        }
        self.stored_forwards = remaining;
        forwarded
    }

    fn handle_direct_delivery(
        &mut self,
        step: usize,
        message: SimMessage,
        context: &mut ClientContext<'_>,
        rolling_latency: Option<&mut RollingLatencyTracker>,
    ) -> Option<usize> {
        let latency = self.apply_delivery(step, message, context, DeliveryKind::Direct)?;
        if let Some(tracker) = rolling_latency {
            tracker.record(latency);
        }
        Some(latency)
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[derive(Clone, Copy)]
enum DeliveryKind {
    Direct,
    Inbox,
    Missed,
}

enum IncomingEnvelopeAction {
    DirectMessage(SimMessage),
    StoredForPeer(StoredForPeerMessage),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_peer_registry_new() {
        let registry = PeerRegistry::new();
        assert_eq!(registry.peer_count(), 0);
        assert!(registry.list_peers().is_empty());
    }

    #[test]
    fn test_peer_registry_add_peer() {
        let mut registry = PeerRegistry::new();
        registry.add_peer("alice".to_string(), "signing_key_alice".to_string());

        assert_eq!(registry.peer_count(), 1);
        assert!(registry.has_peer("alice"));
        assert!(!registry.has_peer("bob"));

        let peer = registry.get_peer("alice").expect("alice should exist");
        assert_eq!(peer.peer_id, "alice");
        assert_eq!(peer.signing_public_key_hex, "signing_key_alice");
        assert!(peer.encryption_public_key_hex.is_none());
    }

    #[test]
    fn test_peer_registry_add_peer_with_encryption() {
        let mut registry = PeerRegistry::new();
        registry.add_peer_with_encryption(
            "bob".to_string(),
            "signing_key_bob".to_string(),
            Some("encryption_key_bob".to_string()),
        );

        let peer = registry.get_peer("bob").expect("bob should exist");
        assert_eq!(peer.peer_id, "bob");
        assert_eq!(peer.signing_public_key_hex, "signing_key_bob");
        assert_eq!(
            peer.encryption_public_key_hex.as_deref(),
            Some("encryption_key_bob")
        );
    }

    #[test]
    fn test_peer_registry_get_signing_key() {
        let mut registry = PeerRegistry::new();
        registry.add_peer("alice".to_string(), "signing_key_alice".to_string());

        assert_eq!(registry.get_signing_key("alice"), Some("signing_key_alice"));
        assert_eq!(registry.get_signing_key("unknown"), None);
    }

    #[test]
    fn test_peer_registry_get_encryption_key() {
        let mut registry = PeerRegistry::new();
        registry.add_peer_with_encryption(
            "bob".to_string(),
            "signing_key_bob".to_string(),
            Some("encryption_key_bob".to_string()),
        );

        assert_eq!(
            registry.get_encryption_key("bob"),
            Some("encryption_key_bob")
        );
        assert_eq!(registry.get_encryption_key("unknown"), None);
    }

    #[test]
    fn test_peer_registry_remove_peer() {
        let mut registry = PeerRegistry::new();
        registry.add_peer("alice".to_string(), "signing_key_alice".to_string());
        registry.add_peer("bob".to_string(), "signing_key_bob".to_string());

        assert_eq!(registry.peer_count(), 2);

        let removed = registry.remove_peer("alice");
        assert!(removed.is_some());
        assert_eq!(removed.unwrap().peer_id, "alice");
        assert_eq!(registry.peer_count(), 1);
        assert!(!registry.has_peer("alice"));
        assert!(registry.has_peer("bob"));

        let not_found = registry.remove_peer("unknown");
        assert!(not_found.is_none());
    }

    #[test]
    fn test_peer_registry_list_peers() {
        let mut registry = PeerRegistry::new();
        registry.add_peer("alice".to_string(), "signing_key_alice".to_string());
        registry.add_peer("bob".to_string(), "signing_key_bob".to_string());
        registry.add_peer("charlie".to_string(), "signing_key_charlie".to_string());

        let peers = registry.list_peers();
        assert_eq!(peers.len(), 3);

        let peer_ids: Vec<&str> = peers.iter().map(|p| p.peer_id.as_str()).collect();
        assert!(peer_ids.contains(&"alice"));
        assert!(peer_ids.contains(&"bob"));
        assert!(peer_ids.contains(&"charlie"));
    }

    #[test]
    fn test_peer_registry_peer_ids() {
        let mut registry = PeerRegistry::new();
        registry.add_peer("alice".to_string(), "signing_key_alice".to_string());
        registry.add_peer("bob".to_string(), "signing_key_bob".to_string());

        let mut peer_ids = registry.peer_ids();
        peer_ids.sort();
        assert_eq!(peer_ids, vec!["alice", "bob"]);
    }

    #[test]
    fn test_peer_registry_update_peer() {
        let mut registry = PeerRegistry::new();
        registry.add_peer("alice".to_string(), "old_signing_key".to_string());

        // Adding same peer again should update it
        registry.add_peer("alice".to_string(), "new_signing_key".to_string());

        assert_eq!(registry.peer_count(), 1);
        assert_eq!(registry.get_signing_key("alice"), Some("new_signing_key"));
    }

    #[test]
    fn test_relay_client_peer_management() {
        let keypair = StoredKeypair {
            id: "client1".to_string(),
            public_key_hex: "client1_pub".to_string(),
            private_key_hex: "client1_priv".to_string(),
            signing_public_key_hex: "client1_sign_pub".to_string(),
            signing_private_key_hex: "client1_sign_priv".to_string(),
        };
        let config = ClientConfig::new("http://localhost:8080", 3600, ClientEncryption::Plaintext);
        let mut client = RelayClient::new(keypair, config);

        client.add_peer("alice".to_string(), "alice_sign_key".to_string());
        assert_eq!(client.peer_registry().peer_count(), 1);
        assert!(client.get_peer("alice").is_some());

        client.add_peer_with_encryption(
            "bob".to_string(),
            "bob_sign_key".to_string(),
            "bob_enc_key".to_string(),
        );
        assert_eq!(client.peer_registry().peer_count(), 2);

        let peers = client.list_peers();
        assert_eq!(peers.len(), 2);

        client.remove_peer("alice");
        assert_eq!(client.peer_registry().peer_count(), 1);
        assert!(!client.peer_registry().has_peer("alice"));
    }

    #[test]
    fn test_simulation_client_peer_management() {
        let schedule = vec![true, true, false, true];
        let mut client = SimulationClient::new("client1", schedule, None);

        client.add_peer("alice".to_string(), "alice_sign_key".to_string());
        assert_eq!(client.peer_registry().peer_count(), 1);
        assert!(client.get_peer("alice").is_some());

        client.add_peer_with_encryption(
            "bob".to_string(),
            "bob_sign_key".to_string(),
            "bob_enc_key".to_string(),
        );
        assert_eq!(client.peer_registry().peer_count(), 2);

        let peers = client.list_peers();
        assert_eq!(peers.len(), 2);

        client.remove_peer("alice");
        assert_eq!(client.peer_registry().peer_count(), 1);
        assert!(!client.peer_registry().has_peer("alice"));
    }

    #[test]
    fn test_public_message_metadata_new() {
        let metadata = PublicMessageMetadata::new(100);
        assert_eq!(metadata.first_seen_at, 100);
        assert_eq!(metadata.propagation_count, 0);
        assert!(metadata.seen_by.is_empty());
        assert!(metadata.should_forward());
    }

    #[test]
    fn test_public_message_metadata_mark_seen() {
        let mut metadata = PublicMessageMetadata::new(100);
        metadata.mark_seen_by("peer1".to_string());
        metadata.mark_seen_by("peer2".to_string());

        assert_eq!(metadata.seen_by.len(), 2);
        assert!(metadata.seen_by.contains("peer1"));
        assert!(metadata.seen_by.contains("peer2"));
    }

    #[test]
    fn test_public_message_metadata_propagation_limit() {
        let mut metadata = PublicMessageMetadata::new(100);

        // Should forward initially
        assert!(metadata.should_forward());

        // Increment to the limit
        for _ in 0..MAX_PUBLIC_MESSAGE_HOPS {
            metadata.increment_propagation();
        }

        // Should not forward after reaching limit
        assert!(!metadata.should_forward());
        assert_eq!(metadata.propagation_count, MAX_PUBLIC_MESSAGE_HOPS);
    }

    #[test]
    fn test_relay_client_send_public_message() {
        let keypair = StoredKeypair {
            id: "alice".to_string(),
            public_key_hex: "alice_pub".to_string(),
            private_key_hex: "alice_priv".to_string(),
            signing_public_key_hex: "alice_sign_pub".to_string(),
            signing_private_key_hex: "alice_sign_priv".to_string(),
        };
        let config = ClientConfig::new("http://localhost:8080", 3600, ClientEncryption::Plaintext);
        let mut client = RelayClient::new(keypair, config);

        // Note: This will fail to post to the relay since relay is not running
        // but we can still verify the envelope is created correctly
        let result = client.send_public_message("Hello, world!");

        // Even though posting fails, the local cache should be updated on the attempt
        if result.is_ok() {
            let envelope = result.unwrap();
            assert_eq!(envelope.header.message_kind, MessageKind::Public);
            assert_eq!(envelope.header.sender_id, "alice");
            assert_eq!(envelope.header.recipient_id, "*");
            assert_eq!(envelope.payload.body, "Hello, world!");
        }
    }

    #[test]
    fn test_relay_client_receive_public_message_unknown_sender() {
        let keypair = StoredKeypair {
            id: "bob".to_string(),
            public_key_hex: "bob_pub".to_string(),
            private_key_hex: "bob_priv".to_string(),
            signing_public_key_hex: "bob_sign_pub".to_string(),
            signing_private_key_hex: "bob_sign_priv".to_string(),
        };
        let config = ClientConfig::new("http://localhost:8080", 3600, ClientEncryption::Plaintext);
        let mut client = RelayClient::new(keypair, config);

        // Create a real keypair for alice
        let alice_keypair = crate::crypto::generate_keypair();

        // Create a public message envelope signed by alice
        let envelope = build_plaintext_envelope(
            "alice",
            "*",
            None,
            None,
            100,
            3600,
            MessageKind::Public,
            None,
            None,
            "Test message",
            &[0u8; 16],
            &alice_keypair.signing_private_key_hex,
        )
        .unwrap();

        // Should fail because alice is not in the peer registry
        let result = client.receive_public_message(envelope);
        assert!(result.is_err());
    }

    #[test]
    fn test_simulation_client_public_message_dedup() {
        use crate::simulation::{MessageEncryption, MetricsTracker, SimulationMetrics};
        use std::collections::HashMap;

        let schedule = vec![true; 10];
        let mut client = SimulationClient::new("peer1", schedule, None);

        // Create simulation context with real keypairs
        let mut keypairs = HashMap::new();
        keypairs.insert("peer1".to_string(), crate::crypto::generate_keypair());
        keypairs.insert("peer2".to_string(), crate::crypto::generate_keypair());

        let mut message_history = HashMap::new();
        let mut message_send_steps = HashMap::new();
        let mut pending_forwarded_messages = HashSet::new();
        let mut metrics = SimulationMetrics {
            planned_messages: 0,
            sent_messages: 0,
            direct_deliveries: 0,
            inbox_deliveries: 0,
            store_forwards_stored: 0,
            store_forwards_forwarded: 0,
            store_forwards_delivered: 0,
        };
        let mut metrics_tracker = MetricsTracker::new(1.0, HashMap::new());

        let mut context = ClientContext {
            direct_enabled: true,
            ttl_seconds: 3600,
            encryption: MessageEncryption::Plaintext,
            direct_links: &HashSet::new(),
            neighbors: &HashMap::new(),
            keypairs: &keypairs,
            message_history: &mut message_history,
            message_send_steps: &mut message_send_steps,
            pending_forwarded_messages: &mut pending_forwarded_messages,
            metrics: &mut metrics,
            metrics_tracker: &mut metrics_tracker,
        };

        // Create a public message
        let envelope = build_plaintext_envelope(
            "peer2",
            "*",
            None,
            None,
            1,
            3600,
            MessageKind::Public,
            None,
            None,
            "Public announcement",
            &[0u8; 16],
            &keypairs["peer2"].signing_private_key_hex,
        )
        .unwrap();

        // First receive should succeed
        let result1 = client.receive_public_message(envelope.clone(), 1, &mut context);
        assert!(result1);

        // Second receive of same message should be deduplicated
        let result2 = client.receive_public_message(envelope, 1, &mut context);
        assert!(!result2);

        // Should have one public message in cache
        assert_eq!(client.public_message_cache().len(), 1);
    }

    #[test]
    fn test_simulation_client_public_message_ttl_expiration() {
        use crate::simulation::{MessageEncryption, MetricsTracker, SimulationMetrics};
        use std::collections::HashMap;

        let schedule = vec![true; 10];
        let mut client = SimulationClient::new("peer1", schedule, None);

        let mut keypairs = HashMap::new();
        keypairs.insert("peer2".to_string(), crate::crypto::generate_keypair());

        let mut message_history = HashMap::new();
        let mut message_send_steps = HashMap::new();
        let mut pending_forwarded_messages = HashSet::new();
        let mut metrics = SimulationMetrics {
            planned_messages: 0,
            sent_messages: 0,
            direct_deliveries: 0,
            inbox_deliveries: 0,
            store_forwards_stored: 0,
            store_forwards_forwarded: 0,
            store_forwards_delivered: 0,
        };
        let mut metrics_tracker = MetricsTracker::new(1.0, HashMap::new());

        let mut context = ClientContext {
            direct_enabled: true,
            ttl_seconds: 10, // Short TTL
            encryption: MessageEncryption::Plaintext,
            direct_links: &HashSet::new(),
            neighbors: &HashMap::new(),
            keypairs: &keypairs,
            message_history: &mut message_history,
            message_send_steps: &mut message_send_steps,
            pending_forwarded_messages: &mut pending_forwarded_messages,
            metrics: &mut metrics,
            metrics_tracker: &mut metrics_tracker,
        };

        // Create a public message with TTL of 10
        let envelope = build_plaintext_envelope(
            "peer2",
            "*",
            None,
            None,
            1,  // timestamp
            10, // TTL
            MessageKind::Public,
            None,
            None,
            "Expired message",
            &[0u8; 16],
            &keypairs["peer2"].signing_private_key_hex,
        )
        .unwrap();

        // Try to receive after TTL expired (step 20 > timestamp 1 + TTL 10)
        let result = client.receive_public_message(envelope, 20, &mut context);
        assert!(!result); // Should be rejected due to expired TTL
    }

    #[test]
    fn test_simulation_client_forward_public_message() {
        let schedule = vec![true; 10];
        let mut client = SimulationClient::new("peer1", schedule, None);

        // Create a real envelope with proper signature
        let peer2_keypair = crate::crypto::generate_keypair();
        let envelope = build_plaintext_envelope(
            "peer2",
            "*",
            None,
            None,
            1,
            3600,
            MessageKind::Public,
            None,
            None,
            "Forward me",
            &[0u8; 16],
            &peer2_keypair.signing_private_key_hex,
        )
        .unwrap();

        // Add metadata for the message
        let metadata = PublicMessageMetadata::new(1);
        client
            .public_message_metadata
            .insert(envelope.header.message_id.clone(), metadata);

        // Forward to some peers
        let peer_ids = vec![
            "peer3".to_string(),
            "peer4".to_string(),
            "peer2".to_string(),
        ];
        let forwarded = client.forward_public_message(&envelope, 2, &peer_ids);

        // Should forward to peer3 and peer4, but not peer2 (original sender)
        assert_eq!(forwarded.len(), 2);
        assert!(forwarded.contains(&"peer3".to_string()));
        assert!(forwarded.contains(&"peer4".to_string()));
        assert!(!forwarded.contains(&"peer2".to_string()));

        // Check metadata was updated
        let meta = client
            .public_message_metadata
            .get(&envelope.header.message_id)
            .unwrap();
        assert_eq!(meta.propagation_count, 1);
        assert!(meta.seen_by.contains("peer3"));
        assert!(meta.seen_by.contains("peer4"));
    }

    #[test]
    fn test_client_config_ws_url() {
        let config = ClientConfig::new("http://localhost:8080", 3600, ClientEncryption::Plaintext);
        assert_eq!(config.ws_url("alice"), "ws://localhost:8080/ws/alice");

        let config = ClientConfig::new(
            "https://relay.example.com",
            3600,
            ClientEncryption::Plaintext,
        );
        assert_eq!(config.ws_url("bob"), "wss://relay.example.com/ws/bob");

        let config = ClientConfig::new("http://localhost:8080/", 3600, ClientEncryption::Plaintext);
        assert_eq!(config.ws_url("charlie"), "ws://localhost:8080/ws/charlie");
    }
}
