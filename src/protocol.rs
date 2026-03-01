//! Tenet protocol message types (Rust).
//!
//! ## Spec summary
//! - Messages are serialized with serde-compatible formats (JSON, CBOR, etc.).
//! - Content-addressed IDs are derived from canonical serialization bytes and
//!   encoded as URL-safe base64 without padding.
//! - `Envelope` binds a `Header` to an encrypted `Payload`, while
//!   `RelayPacket` wraps an opaque envelope for store-and-forward delivery.
//! - Per-recipient key material is carried in `RecipientKey` entries, allowing
//!   a single payload to be encrypted once and shared with many recipients.
//!
//! These types are intentionally small and self-contained so they can be reused
//! across transport layers and storage backends.

use crate::crypto::{
    decrypt_payload, encrypt_payload, sign_message, unwrap_content_key, verify_signature,
    wrap_content_key, CryptoError,
};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// A content-addressed identifier derived from serialized bytes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContentId(pub String);

impl ContentId {
    /// Compute a content ID from arbitrary bytes.
    pub fn from_bytes(bytes: &[u8]) -> Self {
        let digest = Sha256::digest(bytes);
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
        ContentId(encoded)
    }

    /// Compute a content ID from a serde value serialized to JSON.
    pub fn from_value<T: Serialize>(value: &T) -> Result<Self, serde_json::Error> {
        let bytes = serde_json::to_vec(value)?;
        Ok(Self::from_bytes(&bytes))
    }
}

/// Per-recipient encrypted payload key material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct RecipientKey {
    pub recipient_id: String,
    pub key_scheme: String,
    pub encrypted_key: String,
}

/// Supported protocol versions for envelopes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolVersion {
    V1,
    V2,
}

impl ProtocolVersion {
    pub fn is_supported(self) -> bool {
        matches!(self, ProtocolVersion::V1 | ProtocolVersion::V2)
    }
}

/// Types of messages supported by the protocol.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    Public,
    Meta,
    Direct,
    FriendGroup,
    StoreForPeer,
}

/// High-level metadata that binds a payload to a sender and recipient.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Header {
    pub sender_id: String,
    pub recipient_id: String,
    pub store_for: Option<String>,
    pub storage_peer_id: Option<String>,
    pub timestamp: u64,
    pub message_id: ContentId,
    pub message_kind: MessageKind,
    pub group_id: Option<String>,
    pub ttl_seconds: u64,
    pub payload_size: u64,
    #[serde(default)]
    pub payload_hash: Option<String>,
    pub signature: Option<String>,
    #[serde(default)]
    pub reply_to: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
struct CanonicalHeader<'a> {
    version: ProtocolVersion,
    sender_id: &'a str,
    recipient_id: &'a str,
    store_for: Option<&'a str>,
    storage_peer_id: Option<&'a str>,
    timestamp: u64,
    message_id: &'a ContentId,
    message_kind: &'a MessageKind,
    group_id: Option<&'a str>,
    ttl_seconds: u64,
    payload_hash: &'a str,
    reply_to: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeaderError {
    MissingSignature,
    InvalidSignature,
    UnsupportedVersion(ProtocolVersion),
    TtlOutOfRange { ttl_seconds: u64 },
    InvalidMessageKind(String),
    MissingPayloadHash,
    PayloadHashMismatch,
}

#[derive(Debug)]
pub enum EnvelopeBuildError {
    Header(HeaderError),
    Serde(serde_json::Error),
}

impl std::fmt::Display for EnvelopeBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EnvelopeBuildError::Header(error) => write!(f, "header error: {error:?}"),
            EnvelopeBuildError::Serde(error) => write!(f, "serde error: {error}"),
        }
    }
}

impl std::error::Error for EnvelopeBuildError {}

impl From<HeaderError> for EnvelopeBuildError {
    fn from(error: HeaderError) -> Self {
        EnvelopeBuildError::Header(error)
    }
}

impl From<serde_json::Error> for EnvelopeBuildError {
    fn from(error: serde_json::Error) -> Self {
        EnvelopeBuildError::Serde(error)
    }
}

impl Header {
    pub const MAX_TTL_SECONDS: u64 = 7 * 24 * 60 * 60;

    /// Canonical bytes for signing (JSON, deterministic field order).
    pub fn canonical_signing_bytes(
        &self,
        version: ProtocolVersion,
        payload_body: &str,
    ) -> Result<Vec<u8>, serde_json::Error> {
        let hash = hex::encode(Sha256::digest(payload_body.as_bytes()));
        let canonical = CanonicalHeader {
            version,
            sender_id: &self.sender_id,
            recipient_id: &self.recipient_id,
            store_for: self.store_for.as_deref(),
            storage_peer_id: self.storage_peer_id.as_deref(),
            timestamp: self.timestamp,
            message_id: &self.message_id,
            message_kind: &self.message_kind,
            group_id: self.group_id.as_deref(),
            ttl_seconds: self.ttl_seconds,
            payload_hash: &hash,
            reply_to: self.reply_to.as_deref(),
        };
        serde_json::to_vec(&canonical)
    }

    /// Compute a signature for this header using Ed25519.
    pub fn compute_signature(
        &self,
        version: ProtocolVersion,
        signing_private_key_hex: &str,
        payload_body: &str,
    ) -> Result<String, HeaderError> {
        let bytes = self
            .canonical_signing_bytes(version, payload_body)
            .map_err(|_| HeaderError::InvalidSignature)?;
        sign_message(&bytes, signing_private_key_hex).map_err(|_| HeaderError::InvalidSignature)
    }

    /// Verify signature validity, version compatibility, and TTL bounds.
    pub fn verify_signature(
        &self,
        version: ProtocolVersion,
        signing_public_key_hex: &str,
        payload_body: &str,
    ) -> Result<(), HeaderError> {
        if !version.is_supported() {
            return Err(HeaderError::UnsupportedVersion(version));
        }
        if self.ttl_seconds == 0 || self.ttl_seconds > Self::MAX_TTL_SECONDS {
            return Err(HeaderError::TtlOutOfRange {
                ttl_seconds: self.ttl_seconds,
            });
        }
        self.validate_message_kind()?;

        // Verify payload hash matches (required for V2)
        let expected = hex::encode(Sha256::digest(payload_body.as_bytes()));
        let declared = self
            .payload_hash
            .as_deref()
            .ok_or(HeaderError::MissingPayloadHash)?;
        if expected != declared {
            return Err(HeaderError::PayloadHashMismatch);
        }

        let signature = self
            .signature
            .as_deref()
            .ok_or(HeaderError::MissingSignature)?;
        let bytes = self
            .canonical_signing_bytes(version, payload_body)
            .map_err(|_| HeaderError::InvalidSignature)?;
        verify_signature(&bytes, signature, signing_public_key_hex)
            .map_err(|_| HeaderError::InvalidSignature)
    }

    fn validate_message_kind(&self) -> Result<(), HeaderError> {
        match self.message_kind {
            MessageKind::FriendGroup => {
                if self.store_for.is_some() || self.storage_peer_id.is_some() {
                    return Err(HeaderError::InvalidMessageKind(
                        "store_for and storage_peer_id are only valid for store_for_peer messages"
                            .to_string(),
                    ));
                }
                match self.group_id.as_deref() {
                    Some(id) if !id.trim().is_empty() => Ok(()),
                    _ => Err(HeaderError::InvalidMessageKind(
                        "friend_group requires a non-empty group_id".to_string(),
                    )),
                }
            }
            MessageKind::StoreForPeer => {
                if self.group_id.is_some() {
                    return Err(HeaderError::InvalidMessageKind(
                        "group_id is only valid for friend_group messages".to_string(),
                    ));
                }
                let store_for = self.store_for.as_deref().unwrap_or("").trim();
                let storage_peer = self.storage_peer_id.as_deref().unwrap_or("").trim();
                if store_for.is_empty() || storage_peer.is_empty() {
                    return Err(HeaderError::InvalidMessageKind(
                        "store_for_peer requires store_for and storage_peer_id".to_string(),
                    ));
                }
                if storage_peer != self.recipient_id {
                    return Err(HeaderError::InvalidMessageKind(
                        "storage_peer_id must match recipient_id for store_for_peer messages"
                            .to_string(),
                    ));
                }
                Ok(())
            }
            _ => {
                if self.group_id.is_some() {
                    Err(HeaderError::InvalidMessageKind(
                        "group_id is only valid for friend_group messages".to_string(),
                    ))
                } else if self.store_for.is_some() || self.storage_peer_id.is_some() {
                    Err(HeaderError::InvalidMessageKind(
                        "store_for and storage_peer_id are only valid for store_for_peer messages"
                            .to_string(),
                    ))
                } else {
                    Ok(())
                }
            }
        }
    }
}

/// How an attachment's binary data is transported.
///
/// Defaults to `Inline` for backwards compatibility.  Larger files use
/// `RelayBlob` (Phase 1) or `PeerSeeded` (Phase 2, future).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AttachmentTransport {
    /// v1 behaviour — data is base64-encoded inside the HPKE-encrypted payload.
    Inline,
    /// Chunked blob stored on a relay (Option D Phase 1).
    ///
    /// Each chunk is independently encrypted with ChaCha20Poly1305 using
    /// `blob_key` and a per-chunk nonce derived as
    /// `SHA-256(blob_key || chunk_index_le64)[:12]`.
    /// `chunk_hashes` contains the SHA-256 (hex) of each **encrypted** chunk
    /// in order.  Recipients verify the hash before decrypting.
    RelayBlob {
        /// Base URL of the relay blob endpoint (e.g. `https://relay.example.com`).
        relay_url: String,
        /// SHA-256 hex hashes of the **encrypted** chunks, in order.
        chunk_hashes: Vec<String>,
        /// Base64 URL-safe no-pad encoded 32-byte symmetric blob key.
        blob_key: String,
    },
    /// Peer-seeded chunk exchange (Option D Phase 2, future).
    PeerSeeded {
        /// Root SHA-256 hash of the complete blob (hex).
        blob_id: String,
        /// SHA-256 hex hashes of the **encrypted** chunks, in order.
        chunk_hashes: Vec<String>,
        /// Peer IDs known to hold the blob.
        seeders: Vec<String>,
        /// Relay blob URL as fallback when no seeder is reachable.
        relay_fallback: String,
        /// Base64 URL-safe no-pad encoded 32-byte symmetric blob key.
        blob_key: String,
    },
}

/// Optional references to binary attachments stored by content hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AttachmentRef {
    pub content_id: ContentId,
    pub content_type: String,
    pub size: u64,
    /// Original filename, if provided by the sender.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub filename: Option<String>,
    /// Binary attachment data, base64 URL-safe no-pad encoded.
    /// Populated for `Inline` transport; absent for `RelayBlob` / `PeerSeeded`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub data: Option<String>,
    /// Transport mechanism.  `None` is equivalent to `Inline` for backwards
    /// compatibility with v1 senders.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub transport: Option<AttachmentTransport>,
}

/// Encrypted user data with optional attachment references.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Payload {
    pub id: ContentId,
    pub content_type: String,
    pub body: String,
    pub attachments: Vec<AttachmentRef>,
}

pub const PLAINTEXT_CONTENT_TYPE: &str = "text/plain";
pub const ENCRYPTED_CONTENT_TYPE: &str = "application/json;type=tenet.encrypted";
pub const META_CONTENT_TYPE: &str = "application/json;type=tenet.meta";
pub const GROUP_ENCRYPTED_CONTENT_TYPE: &str = "application/json;type=tenet.group_encrypted";

/// Metadata-only protocol messages (e.g., presence and recovery hints).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MetaMessage {
    Online {
        peer_id: String,
        timestamp: u64,
    },
    Ack {
        peer_id: String,
        online_timestamp: u64,
    },
    MessageRequest {
        peer_id: String,
        since_timestamp: u64,
    },
    /// Geographic variant of mesh discovery. Uses either geohash prefix OR
    /// country/region/city text filters.
    GeoMessageRequest {
        peer_id: String,
        since_timestamp: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        geohash_prefix: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        country_code: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        region: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        city: Option<String>,
    },
    FriendRequest {
        peer_id: String,
        signing_public_key: String,
        encryption_public_key: String,
        message: Option<String>,
    },
    FriendAccept {
        peer_id: String,
        signing_public_key: String,
        encryption_public_key: String,
    },
    /// Sent by a group creator to invite a prospective member.
    GroupInvite {
        peer_id: String,
        group_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        group_name: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// Sent by an invitee back to the creator to accept the group invite.
    GroupInviteAccept {
        peer_id: String,
        group_id: String,
    },
    /// Request that `for_peer_id` re-broadcast their public profile.
    ProfileRequest {
        peer_id: String,
        for_peer_id: String,
    },
    /// Response to `MessageRequest`: announces available public message IDs.
    MeshAvailable {
        peer_id: String,
        message_ids: Vec<String>,
        since_timestamp: u64,
    },
    /// Response to `GeoMessageRequest`.
    GeoMeshAvailable {
        peer_id: String,
        message_ids: Vec<String>,
        since_timestamp: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        geohash_prefix: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        country_code: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        region: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        city: Option<String>,
    },
    /// Request specific public messages by ID after deduplication.
    MeshRequest {
        peer_id: String,
        message_ids: Vec<String>,
    },
    /// Delivers batches of public envelopes plus sender signing keys for
    /// signature verification of messages from previously-unknown peers.
    MeshDelivery {
        peer_id: String,
        /// Serialised `Envelope` JSON values.
        envelopes: Vec<serde_json::Value>,
        /// Maps sender_id → signing_public_key_hex for each sender whose
        /// messages are included.  The receiver MUST validate the key by
        /// checking `SHA256(key_bytes) == sender_id` before trusting it.
        sender_keys: std::collections::HashMap<String, String>,
    },
}

#[derive(Debug)]
pub enum MetaMessageError {
    Serde(serde_json::Error),
    InvalidContentType { expected: String, actual: String },
}

impl std::fmt::Display for MetaMessageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MetaMessageError::Serde(error) => write!(f, "serde error: {error}"),
            MetaMessageError::InvalidContentType { expected, actual } => {
                write!(f, "invalid content type: expected {expected}, got {actual}")
            }
        }
    }
}

impl std::error::Error for MetaMessageError {}

impl From<serde_json::Error> for MetaMessageError {
    fn from(error: serde_json::Error) -> Self {
        MetaMessageError::Serde(error)
    }
}

pub fn build_meta_payload(meta: &MetaMessage) -> Result<Payload, serde_json::Error> {
    let body = serde_json::to_string(meta)?;
    let payload_id = ContentId::from_bytes(body.as_bytes());
    Ok(Payload {
        id: payload_id,
        content_type: META_CONTENT_TYPE.to_string(),
        body,
        attachments: Vec::new(),
    })
}

pub fn decode_meta_payload(payload: &Payload) -> Result<MetaMessage, MetaMessageError> {
    if payload.content_type != META_CONTENT_TYPE {
        return Err(MetaMessageError::InvalidContentType {
            expected: META_CONTENT_TYPE.to_string(),
            actual: payload.content_type.clone(),
        });
    }
    Ok(serde_json::from_str(&payload.body)?)
}

/// Build a plaintext payload with a salted content ID to avoid collisions.
pub fn build_plaintext_payload(body: impl Into<String>, salt: impl AsRef<[u8]>) -> Payload {
    let body = body.into();
    let mut bytes = Vec::with_capacity(body.len() + salt.as_ref().len());
    bytes.extend_from_slice(body.as_bytes());
    bytes.extend_from_slice(salt.as_ref());
    let id = ContentId::from_bytes(&bytes);
    Payload {
        id,
        content_type: PLAINTEXT_CONTENT_TYPE.to_string(),
        body,
        attachments: Vec::new(),
    }
}

/// Build an envelope for an existing payload, adding a signed header.
pub fn build_envelope_from_payload(
    sender_id: impl Into<String>,
    recipient_id: impl Into<String>,
    store_for: Option<String>,
    storage_peer_id: Option<String>,
    timestamp: u64,
    ttl_seconds: u64,
    message_kind: MessageKind,
    group_id: Option<String>,
    reply_to: Option<String>,
    payload: Payload,
    signing_private_key_hex: &str,
) -> Result<Envelope, EnvelopeBuildError> {
    let message_id = ContentId::from_value(&payload)?;
    let mut header = Header {
        sender_id: sender_id.into(),
        recipient_id: recipient_id.into(),
        store_for,
        storage_peer_id,
        timestamp,
        message_id,
        message_kind,
        group_id,
        ttl_seconds,
        payload_size: payload.body.len() as u64,
        payload_hash: None,
        signature: None,
        reply_to,
    };
    header.validate_message_kind()?;

    // Compute and store hash for V2
    header.payload_hash = Some(hex::encode(Sha256::digest(payload.body.as_bytes())));

    let signature =
        header.compute_signature(ProtocolVersion::V2, signing_private_key_hex, &payload.body)?;
    header.signature = Some(signature);

    Ok(Envelope {
        version: ProtocolVersion::V2,
        header,
        payload,
    })
}

/// Build a plaintext envelope using a derived payload and signed header.
pub fn build_plaintext_envelope(
    sender_id: impl Into<String>,
    recipient_id: impl Into<String>,
    store_for: Option<String>,
    storage_peer_id: Option<String>,
    timestamp: u64,
    ttl_seconds: u64,
    message_kind: MessageKind,
    group_id: Option<String>,
    reply_to: Option<String>,
    body: impl Into<String>,
    salt: impl AsRef<[u8]>,
    signing_private_key_hex: &str,
) -> Result<Envelope, EnvelopeBuildError> {
    let payload = build_plaintext_payload(body, salt);
    build_envelope_from_payload(
        sender_id,
        recipient_id,
        store_for,
        storage_peer_id,
        timestamp,
        ttl_seconds,
        message_kind,
        group_id,
        reply_to,
        payload,
        signing_private_key_hex,
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WrappedKeyPayload {
    pub enc_b64: String,
    pub ciphertext_b64: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncryptedPayload {
    pub nonce_b64: String,
    pub ciphertext_b64: String,
    pub wrapped_key: WrappedKeyPayload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupEncryptedPayload {
    pub nonce_b64: String,
    pub ciphertext_b64: String,
}

#[derive(Debug)]
pub enum PayloadCryptoError {
    Crypto(CryptoError),
    Serde(serde_json::Error),
    Hex(hex::FromHexError),
    Base64(base64::DecodeError),
    InvalidContentType { expected: String, actual: String },
}

impl std::fmt::Display for PayloadCryptoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PayloadCryptoError::Crypto(error) => write!(f, "crypto error: {error}"),
            PayloadCryptoError::Serde(error) => write!(f, "serde error: {error}"),
            PayloadCryptoError::Hex(error) => write!(f, "hex error: {error}"),
            PayloadCryptoError::Base64(error) => write!(f, "base64 error: {error}"),
            PayloadCryptoError::InvalidContentType { expected, actual } => {
                write!(f, "invalid content type: expected {expected}, got {actual}")
            }
        }
    }
}

impl std::error::Error for PayloadCryptoError {}

impl From<CryptoError> for PayloadCryptoError {
    fn from(error: CryptoError) -> Self {
        PayloadCryptoError::Crypto(error)
    }
}

impl From<serde_json::Error> for PayloadCryptoError {
    fn from(error: serde_json::Error) -> Self {
        PayloadCryptoError::Serde(error)
    }
}

impl From<hex::FromHexError> for PayloadCryptoError {
    fn from(error: hex::FromHexError) -> Self {
        PayloadCryptoError::Hex(error)
    }
}

impl From<base64::DecodeError> for PayloadCryptoError {
    fn from(error: base64::DecodeError) -> Self {
        PayloadCryptoError::Base64(error)
    }
}

pub fn build_encrypted_payload(
    plaintext: impl AsRef<[u8]>,
    recipient_public_key_hex: &str,
    aad: &[u8],
    hpke_info: &[u8],
    content_key: &[u8],
    nonce: &[u8],
    sender_seed: Option<[u8; 32]>,
) -> Result<Payload, PayloadCryptoError> {
    let (nonce_bytes, ciphertext) =
        encrypt_payload(content_key, plaintext.as_ref(), aad, Some(nonce))?;
    let recipient_public_key_bytes = hex::decode(recipient_public_key_hex)?;
    let wrapped = wrap_content_key(
        &recipient_public_key_bytes,
        content_key,
        hpke_info,
        sender_seed,
    )?;
    let encrypted_payload = EncryptedPayload {
        nonce_b64: URL_SAFE_NO_PAD.encode(&nonce_bytes),
        ciphertext_b64: URL_SAFE_NO_PAD.encode(&ciphertext),
        wrapped_key: WrappedKeyPayload {
            enc_b64: URL_SAFE_NO_PAD.encode(&wrapped.enc),
            ciphertext_b64: URL_SAFE_NO_PAD.encode(&wrapped.ciphertext),
        },
    };
    let payload_body = serde_json::to_string(&encrypted_payload)?;
    let payload_id = ContentId::from_bytes(payload_body.as_bytes());
    Ok(Payload {
        id: payload_id,
        content_type: ENCRYPTED_CONTENT_TYPE.to_string(),
        body: payload_body,
        attachments: Vec::new(),
    })
}

pub fn decrypt_encrypted_payload(
    payload: &Payload,
    recipient_private_key_hex: &str,
    aad: &[u8],
    hpke_info: &[u8],
) -> Result<Vec<u8>, PayloadCryptoError> {
    if payload.content_type != ENCRYPTED_CONTENT_TYPE {
        return Err(PayloadCryptoError::InvalidContentType {
            expected: ENCRYPTED_CONTENT_TYPE.to_string(),
            actual: payload.content_type.clone(),
        });
    }
    let encrypted: EncryptedPayload = serde_json::from_str(&payload.body)?;
    let wrapped = crate::crypto::WrappedKey {
        enc: URL_SAFE_NO_PAD.decode(encrypted.wrapped_key.enc_b64.as_bytes())?,
        ciphertext: URL_SAFE_NO_PAD.decode(encrypted.wrapped_key.ciphertext_b64.as_bytes())?,
    };
    let recipient_private_key_bytes = hex::decode(recipient_private_key_hex)?;
    let content_key = unwrap_content_key(&recipient_private_key_bytes, &wrapped, hpke_info)?;
    let nonce = URL_SAFE_NO_PAD.decode(encrypted.nonce_b64.as_bytes())?;
    let ciphertext = URL_SAFE_NO_PAD.decode(encrypted.ciphertext_b64.as_bytes())?;
    Ok(decrypt_payload(&content_key, &nonce, &ciphertext, aad)?)
}

/// Build a payload for a group message encrypted with a symmetric group key
pub fn build_group_message_payload(
    plaintext: impl AsRef<[u8]>,
    group_key: &[u8; 32],
    aad: &[u8],
) -> Result<Payload, PayloadCryptoError> {
    let (ciphertext, nonce) =
        crate::crypto::encrypt_group_payload(plaintext.as_ref(), group_key, aad)?;

    let group_encrypted = GroupEncryptedPayload {
        nonce_b64: URL_SAFE_NO_PAD.encode(&nonce),
        ciphertext_b64: URL_SAFE_NO_PAD.encode(&ciphertext),
    };

    let payload_body = serde_json::to_string(&group_encrypted)?;
    let payload_id = ContentId::from_bytes(payload_body.as_bytes());

    Ok(Payload {
        id: payload_id,
        content_type: GROUP_ENCRYPTED_CONTENT_TYPE.to_string(),
        body: payload_body,
        attachments: Vec::new(),
    })
}

/// Decrypt a group message payload using a symmetric group key
pub fn decrypt_group_message_payload(
    payload: &Payload,
    group_key: &[u8; 32],
    aad: &[u8],
) -> Result<Vec<u8>, PayloadCryptoError> {
    let group_encrypted: GroupEncryptedPayload = serde_json::from_str(&payload.body)?;
    let nonce = URL_SAFE_NO_PAD.decode(group_encrypted.nonce_b64.as_bytes())?;
    let ciphertext = URL_SAFE_NO_PAD.decode(group_encrypted.ciphertext_b64.as_bytes())?;

    Ok(crate::crypto::decrypt_group_payload(
        &ciphertext,
        &nonce,
        group_key,
        aad,
    )?)
}

/// Envelope binding a header to an encrypted payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Envelope {
    pub version: ProtocolVersion,
    pub header: Header,
    pub payload: Payload,
}

/// Store-and-forward wrapper for relays or transport-specific hops.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct RelayPacket {
    pub envelope_id: ContentId,
    pub sender_id: String,
    pub recipient_id: String,
    pub transport: String,
    pub body_b64: String,
}
