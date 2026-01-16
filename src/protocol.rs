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
}

impl ProtocolVersion {
    pub fn is_supported(self) -> bool {
        matches!(self, ProtocolVersion::V1)
    }
}

/// High-level metadata that binds a payload to a sender and recipient.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Header {
    pub sender_id: String,
    pub recipient_id: String,
    pub timestamp: u64,
    pub message_id: ContentId,
    pub ttl_seconds: u64,
    pub payload_size: u64,
    pub signature: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
struct CanonicalHeader<'a> {
    version: ProtocolVersion,
    sender_id: &'a str,
    recipient_id: &'a str,
    timestamp: u64,
    message_id: &'a ContentId,
    ttl_seconds: u64,
    payload_size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeaderError {
    MissingSignature,
    InvalidSignature,
    UnsupportedVersion(ProtocolVersion),
    TtlOutOfRange { ttl_seconds: u64 },
}

impl Header {
    pub const MAX_TTL_SECONDS: u64 = 7 * 24 * 60 * 60;

    /// Canonical bytes for signing (JSON, deterministic field order).
    pub fn canonical_signing_bytes(
        &self,
        version: ProtocolVersion,
    ) -> Result<Vec<u8>, serde_json::Error> {
        let canonical = CanonicalHeader {
            version,
            sender_id: &self.sender_id,
            recipient_id: &self.recipient_id,
            timestamp: self.timestamp,
            message_id: &self.message_id,
            ttl_seconds: self.ttl_seconds,
            payload_size: self.payload_size,
        };
        serde_json::to_vec(&canonical)
    }

    /// Compute the expected signature for this header and version.
    pub fn expected_signature(
        &self,
        version: ProtocolVersion,
    ) -> Result<String, serde_json::Error> {
        let bytes = self.canonical_signing_bytes(version)?;
        let digest = Sha256::digest(&bytes);
        Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest))
    }

    /// Verify signature validity, version compatibility, and TTL bounds.
    pub fn verify_signature(&self, version: ProtocolVersion) -> Result<(), HeaderError> {
        if !version.is_supported() {
            return Err(HeaderError::UnsupportedVersion(version));
        }
        if self.ttl_seconds == 0 || self.ttl_seconds > Self::MAX_TTL_SECONDS {
            return Err(HeaderError::TtlOutOfRange {
                ttl_seconds: self.ttl_seconds,
            });
        }
        let signature = self.signature.as_deref().ok_or(HeaderError::MissingSignature)?;
        let expected = self
            .expected_signature(version)
            .map_err(|_| HeaderError::InvalidSignature)?;
        if signature == expected {
            Ok(())
        } else {
            Err(HeaderError::InvalidSignature)
        }
    }
}

/// Optional references to binary attachments stored by content hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AttachmentRef {
    pub content_id: ContentId,
    pub content_type: String,
    pub size: u64,
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
