# Public Message Mesh Distribution

## Status

- [x] Plan written
- [x] `protocol.rs` — new MetaMessage variants
- [x] `storage.rs` — schema migration + new queries
- [x] `message_handler.rs` — mesh protocol logic + backfill
- [x] `web_client/` — sync trigger + constants + state
- [x] Tests passing (`cargo test` — 95 passed, 0 failed)

---

## Problem Statement

Peers who don't know each other can't exchange public messages directly. New
peers and peers returning from offline miss public messages posted while they
were away. The relay already distributes public messages to peers known at the
time of posting, but has no catch-up mechanism.

---

## Design

A pull-based 5-phase protocol. All messages are encrypted `MetaMessage`
envelopes exchanged via the relay; the relay sees only sender/recipient/kind
metadata.

```
A                              B
|                              |
|-- MessageRequest (since) -->|   Phase 1: Query (existing variant, now implemented)
|<-- MeshAvailable (ids) ----|   Phase 2: Announce IDs
|                              |
| (dedup against local DB)     |
|                              |
|-- MeshRequest (ids) ------->|   Phase 3: Request unknowns
|<-- MeshDelivery (envs) -----|   Phase 4: Deliver batches (+ sender keys for backfill)
```

### Why not re-post originals to the relay?

Public envelopes use `recipient_id = "*"`. Re-posting causes fan-out
amplification and is blocked by relay dedup for already-seen IDs. Wrapping in
Meta envelopes (which have unique IDs) avoids both problems.

### Overlap with StoreForPeer

`StoreForPeer` holds private messages at rest for offline peers. This feature
distributes **public** content via catch-up pull. They share relay transport
but serve different purposes and have different payload structures — no
unification needed.

### Trust model for unknown senders

Peer B forwards a message from C (unknown to A). A can't verify C's signature
without C's key. Approach:
- `MeshDelivery` includes a `sender_keys` map of sender_id → signing_public_key_hex for every sender B is forwarding on behalf of.
- A validates each key by checking `SHA256(key_bytes) == sender_id` (same derivation as `derive_user_id_from_public_key` in `crypto.rs:127`).
- If the key validates: verify signature, store with `signature_verified = true`.
- If no key or validation fails: store with `signature_verified = false` (unverified).
- **Backfill**: whenever A later learns a peer's key (FriendRequest, FriendAccept, peer add, MeshDelivery), re-verify any previously unverified messages from that sender.

---

## Changes

### 1. `src/protocol.rs` — 3 new `MetaMessage` variants

```rust
/// Response to MessageRequest: IDs of available public messages.
MeshAvailable {
    peer_id: String,
    message_ids: Vec<String>,
    since_timestamp: u64,
},
/// Request specific public messages by ID (sent after dedup).
MeshRequest {
    peer_id: String,
    message_ids: Vec<String>,
},
/// Batched delivery of public envelopes + sender keys for verification.
MeshDelivery {
    peer_id: String,
    envelopes: Vec<serde_json::Value>,
    sender_keys: std::collections::HashMap<String, String>,
},
```

`MessageRequest` already exists (`protocol.rs:311`) and is reused for Phase 1.

### 2. `src/storage.rs`

- **Schema migration**: add `signature_verified INTEGER NOT NULL DEFAULT 1` column to `messages`.
- `list_public_message_ids_since(since: u64, limit: u32) -> Result<Vec<String>, StorageError>` — query public message IDs in window.
- `list_unverified_messages_from(sender_id: &str) -> Result<Vec<MessageRow>, StorageError>` — messages with `signature_verified = 0`.
- `mark_messages_verified(sender_id: &str) -> Result<(), StorageError>` — bulk update.
- `delete_messages_from_sender(sender_id: &str) -> Result<(), StorageError>` — for tampered messages.

### 3. `src/message_handler.rs`

**3a. Store `raw_envelope` for public messages** — change `raw_envelope: None` to serialize the original envelope for `MessageKind::Public`. Required for forwarding.

**3b. Handle `MessageRequest`** — respond with `MeshAvailable` containing up to `MAX_MESH_IDS` public message IDs since the capped timestamp.

**3c. Handle `MeshAvailable`** — dedup against local DB, send `MeshRequest` for unknowns.

**3d. Handle `MeshRequest`** — fetch raw envelopes, batch into `MeshDelivery` responses (up to `MAX_MESH_BATCH` per message).

**3e. Handle `MeshDelivery`** — for each inner envelope: validate type/TTL/dedup, verify signature where possible (using `sender_keys`), store with appropriate `signature_verified` value.

**3f. `try_backfill_signatures(sender_id, signing_key_hex)`** — re-verify stored unverified messages when a key becomes available. Called from FriendRequest, FriendAccept, and MeshDelivery handling.

### 4. `src/web_client/sync.rs`

- After successful sync, call `send_mesh_queries` for each peer not queried within `MESH_QUERY_INTERVAL_SECS`.

### 5. `src/web_client/state.rs`

- Add `last_mesh_query_sent: std::collections::HashMap<String, u64>` to `AppState`.

### 6. `src/web_client/config.rs`

```rust
pub const DEFAULT_MESH_WINDOW_SECS: u64 = 7_200;     // 2 hours
pub const MAX_MESH_WINDOW_SECS: u64 = 86_400;         // 24-hour hard cap
pub const MAX_MESH_IDS: u32 = 500;
pub const MAX_MESH_REQUEST_IDS: u32 = 100;
pub const MAX_MESH_BATCH: usize = 10;
pub const MESH_QUERY_INTERVAL_SECS: u64 = 600;        // 10 minutes
```

### 7. `src/web_client/handlers/peers.rs`

- Trigger `try_backfill_signatures` after a peer is added via REST API.

---

## File Change Summary

| File | Change |
|------|--------|
| `src/protocol.rs` | 3 new `MetaMessage` variants |
| `src/storage.rs` | `signature_verified` column; 4 new queries |
| `src/message_handler.rs` | Raw envelope for public; 5 `on_meta` arms; backfill helper |
| `src/web_client/sync.rs` | `send_mesh_queries` after sync; delegate new variants |
| `src/web_client/state.rs` | `last_mesh_query_sent` field |
| `src/web_client/config.rs` | 6 new constants |
| `src/web_client/handlers/peers.rs` | Backfill on peer add |

---

## Out of Scope (for now)

- **Push-based availability**: Peers proactively sending `MeshAvailable` on connect.
- **Relay-side mesh routing**: All delivery via existing inbox mechanism.
- **Message pagination**: Only most recent `MAX_MESH_IDS` offered per window.

---

## Remaining Work (other subsystems)

### Simulation (`src/simulation/`, `src/client.rs`)

The simulation client (`client.rs`) has its own gossip path (`handle_message_request`, `forward_public_message_to_peers`, `MAX_PUBLIC_MESSAGE_HOPS`). The mesh protocol implemented here is for the web client only. The simulation would need a parallel implementation to test mesh catch-up in multi-peer scenarios. **Status: not done — tracked as future work.**

### Debugger (`src/bin/debugger.rs`)

The interactive REPL debugger uses the simulation harness, not the web client stack. Mesh queries are not wired into the debugger. **Status: not done — tracked as future work.**

### Client library documentation

`CLAUDE.md` describes the module structure but does not document the mesh protocol. The client library (`src/client.rs`) `MessageHandler` trait docs should mention the new `MetaMessage` variants and when `on_meta` is called for them. **Status: not done — update CLAUDE.md and doc comments when API stabilises.**
