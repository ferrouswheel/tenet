# Payload Authentication Gap (Protocol V2)

## The Problem

`Header.verify_signature` verifies the Ed25519 signature over `canonical_signing_bytes`,
which is a JSON-serialised `CanonicalHeader` struct. That struct includes `payload_size`
(the byte-length of `payload.body`) but **not the payload content itself**.

```rust
// protocol.rs – what is actually signed
struct CanonicalHeader<'a> {
    version, sender_id, recipient_id, store_for, storage_peer_id,
    timestamp, message_id, message_kind, group_id, ttl_seconds,
    payload_size,   // ← byte count only
    reply_to,
    // payload.body is NOT here
}
```

An attacker who can modify a stored or forwarded envelope can:

1. Change `payload.body` to any string of the **same byte length** → signature still
   verifies, body undetectably replaced.
2. Change `payload.body` to a different length AND update `header.payload_size` to
   match → same result, because `verify_signature` takes no payload input; it just
   re-serialises whatever `self.payload_size` currently says.

## Why Encrypted Messages Are Not Affected

`Direct`, `Meta`, and `FriendGroup` messages have an AEAD-encrypted payload
(HPKE + ChaCha20Poly1305). Any modification to the ciphertext fails decryption with
an authentication tag error, regardless of the header signature. The signature gap
only matters where `payload.body` is **plaintext** — currently only `MessageKind::Public`.

## Proposed Fix — Protocol V2

Replace `payload_size` in `CanonicalHeader` with `payload_hash`:
`SHA256(payload.body.as_bytes())`, hex-encoded. The hash implies the size, removes the
redundant field, and makes the payload content part of the signed data.

Verification then checks two things:
1. `SHA256(actual_payload.body) == header.payload_hash` — body matches the committed hash.
2. Ed25519 signature over canonical bytes (which now include the hash) is valid.

---

## Changes Required

### 1. `src/protocol.rs` — `ProtocolVersion`

Add `V2`:

```rust
pub enum ProtocolVersion {
    V1,
    V2,
}

impl ProtocolVersion {
    pub fn is_supported(self) -> bool {
        matches!(self, ProtocolVersion::V1 | ProtocolVersion::V2)
    }
}
```

### 2. `src/protocol.rs` — `Header` struct

Add an optional `payload_hash` field (absent in V1 messages):

```rust
pub struct Header {
    // ... existing fields ...
    pub payload_size: u64,        // keep for stream sizing; no longer signed in V2
    pub payload_hash: Option<String>,  // hex SHA256 of payload.body; required for V2
    // ...
}
```

Annotate with `#[serde(default)]` so V1 messages deserialise without error.

### 3. `src/protocol.rs` — `CanonicalHeader`

Split into a V1 and V2 form, or use an enum inside `canonical_signing_bytes`:

```rust
// V1 (existing) — kept for backward-compatible verification of old messages
struct CanonicalHeaderV1<'a> { ..., payload_size: u64, ... }

// V2 — payload_size replaced by payload_hash
struct CanonicalHeaderV2<'a> { ..., payload_hash: &'a str, ... }
```

### 4. `src/protocol.rs` — `canonical_signing_bytes`

Dispatch on version:

```rust
pub fn canonical_signing_bytes(&self, version: ProtocolVersion, payload_body: Option<&str>)
    -> Result<Vec<u8>, serde_json::Error>
{
    match version {
        ProtocolVersion::V1 => {
            serde_json::to_vec(&CanonicalHeaderV1 { ..., payload_size: self.payload_size, ... })
        }
        ProtocolVersion::V2 => {
            let hash = hex::encode(Sha256::digest(
                payload_body.unwrap_or("").as_bytes()
            ));
            serde_json::to_vec(&CanonicalHeaderV2 { ..., payload_hash: &hash, ... })
        }
    }
}
```

`sha2` is already a dependency (`protocol.rs` uses `Sha256` for `ContentId`).

### 5. `src/protocol.rs` — `compute_signature` and `verify_signature`

Both methods need the payload body for V2. Signature update:

```rust
pub fn compute_signature(
    &self,
    version: ProtocolVersion,
    signing_private_key_hex: &str,
    payload_body: &str,         // new parameter
) -> Result<String, HeaderError>

pub fn verify_signature(
    &self,
    version: ProtocolVersion,
    signing_public_key_hex: &str,
    payload_body: &str,         // new parameter
) -> Result<(), HeaderError>
```

`verify_signature` for V2 additionally checks:

```rust
if version == ProtocolVersion::V2 {
    let expected = hex::encode(Sha256::digest(payload_body.as_bytes()));
    let declared = self.payload_hash.as_deref().ok_or(HeaderError::MissingPayloadHash)?;
    if expected != declared {
        return Err(HeaderError::PayloadHashMismatch);
    }
}
```

New `HeaderError` variants needed: `MissingPayloadHash`, `PayloadHashMismatch`.

### 6. `src/protocol.rs` — `build_envelope_from_payload`

Compute the hash and store it in the header before signing:

```rust
header.payload_hash = Some(hex::encode(Sha256::digest(payload.body.as_bytes())));
let signature = header.compute_signature(ProtocolVersion::V2, signing_private_key_hex, &payload.body)?;
```

All new envelopes would be created as V2. (Or introduce a `CURRENT_VERSION` constant
that callers pass — easier to change later.)

### 7. Call sites for `verify_signature`

Every call to `header.verify_signature(...)` must be updated to pass `&envelope.payload.body`.
Current locations (non-exhaustive):

| File | Location |
|------|----------|
| `src/message_handler.rs` | `try_backfill_signatures` |
| `src/message_handler.rs` | `verify_mesh_envelope` |
| `src/protocol.rs` tests | `header_roundtrips_and_validates_message_kinds` etc. |
| `tests/protocol_tests.rs` | signature round-trip tests |

---

## Migration Strategy

- **V1 messages remain valid**: `is_supported` returns true for both versions; V1
  verification path ignores payload body as today.
- **New messages are V2**: `build_envelope_from_payload` always produces V2. No flag needed.
- **V1 messages received via mesh**: stored and treated as unverified (`signature_verified = false`)
  until the sender is seen producing a V2 message, or treated as a trust-level downgrade.
  Simplest policy: accept V1 public mesh messages with a warning log, same as today.
- **Existing stored messages**: already stored; no retroactive re-signing possible.
  `signature_verified` stays true for messages received before the upgrade.

---

## What Is NOT Changing

- Encryption scheme (HPKE + ChaCha20Poly1305) — unaffected.
- `ContentId` hashing — unaffected.
- Relay wire format — relays are transport-agnostic; they forward opaque envelopes.
- Encrypted message kinds (Direct, Meta, FriendGroup, StoreForPeer) — AEAD already
  provides payload authentication; V2 is belt-and-suspenders for them but not urgent.

---

## Status

- [ ] Implement in `src/protocol.rs`
- [ ] Update all `verify_signature` call sites
- [ ] Update `build_envelope_from_payload` to produce V2
- [ ] Update tests in `tests/protocol_tests.rs` and `tests/crypto_tests.rs`
- [ ] Update `docs/architecture.md` protocol description
