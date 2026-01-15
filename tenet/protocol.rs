//! Tenet protocol message types (Rust).
//!
//! ## Spec summary
//! - Messages are serialized with serde-compatible formats (JSON, CBOR, etc.).
//! - Content-addressed IDs are derived from canonical serialization bytes and
//!   encoded as URL-safe base64 without padding.
//! - `Envelope` binds a `MessageHeader` to an encrypted `Payload`, while
//!   `RelayPacket` wraps an opaque envelope for store-and-forward delivery.
//! - Per-recipient key material is carried in `RecipientKey` entries, allowing
//!   a single payload to be encrypted once and shared with many recipients.
//!
//! These types are intentionally small and self-contained so they can be reused
//! across transport layers and storage backends.

use serde::{Deserialize, Serialize};
use base64::Engine as _;
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

/// High-level metadata that binds a payload to a sender and recipients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct MessageHeader {
    pub sender_id: String,
    pub timestamp: u64,
    pub payload_id: ContentId,
    pub recipients: Vec<RecipientKey>,
    pub signature: Option<String>,
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
    pub id: ContentId,
    pub header: MessageHeader,
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
