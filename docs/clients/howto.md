# How to Work with the Tenet Library

This document describes patterns and recommendations for building clients on top of the Tenet
library, with a focus on message processing, storage integration, and the `StorageMessageHandler`
design.

## The Core Problem: Split Message Processing

Message processing is currently split across two places:

1. **`RelayClient.sync_inbox()`** (`src/client.rs`) — fetches from relay, verifies signatures,
   decrypts `Direct` messages, deduplicates, and returns a `SyncOutcome`. Only handles `Direct`
   messages; returns an error for all other kinds.

2. **`sync_once()`** (`src/web_client/sync.rs`) — the web client's sync loop. It bypasses
   `RelayClient` entirely, re-implementing envelope fetching, signature verification, and
   decryption, while also handling `Meta`, `Public`, `FriendGroup`, reactions, profile updates,
   and group key distribution.

This creates two problems:

- **Duplication**: `sync.rs` reimplements logic already in `RelayClient`. Any protocol change
  must be applied in both places.
- **High bar for new clients**: a new client (CLI, mobile) wanting more than `Direct` messages
  must bypass `RelayClient` the same way the web client does.

## Proposed Design: `MessageHandler` Trait

The recommended direction is to make `sync_inbox()` emit richer, kind-aware output for all
message types via two complementary mechanisms:

1. **Richer `SyncOutcome`** — a pull API: callers iterate over typed events.
2. **`MessageHandler` trait** — an optional push API: callers implement a trait and register it
   on the client; the client calls hooks during sync.

### Typed Sync Events

```rust
pub struct SyncEvent {
    pub envelope: Envelope,
    pub outcome: SyncEventOutcome,
}

pub enum SyncEventOutcome {
    /// Verified and decrypted successfully. Ready to store.
    Message(ClientMessage),

    /// A structured meta-message (Online, Ack, FriendRequest, etc.).
    Meta(MetaMessage),

    /// A raw meta payload that couldn't be parsed as MetaMessage.
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

pub struct SyncOutcome {
    pub events: Vec<SyncEvent>,
    pub fetched: usize,

    // Convenience views (filtered from events):
    pub messages: Vec<ClientMessage>,
    pub errors: Vec<String>,
}
```

### `MessageHandler` Trait

```rust
pub trait MessageHandler: Send {
    /// Called for each successfully verified and decrypted message.
    fn on_message(&mut self, envelope: &Envelope, message: &ClientMessage) {}

    /// Called for each structured meta-message (Online, Ack, FriendRequest, etc.).
    fn on_meta(&mut self, meta: &MetaMessage) {}

    /// Called for raw meta payloads that couldn't be parsed as MetaMessage.
    fn on_raw_meta(&mut self, envelope: &Envelope, body: &str) {}

    /// Called when a message is rejected (bad signature, unknown sender, etc.).
    fn on_rejected(&mut self, envelope: &Envelope, reason: &str) {}
}
```

All methods have default no-op implementations so callers only override what they need.

`RelayClient` accepts an optional boxed handler:

```rust
impl RelayClient {
    pub fn set_handler(&mut self, handler: Box<dyn MessageHandler>) { ... }
    pub fn clear_handler(&mut self) { ... }
}
```

## `StorageMessageHandler`

The recommended shared persistence layer for any full client. It lives in
`src/message_handler.rs` (part of the library, not `web_client/`).

### What it handles

**Protocol-level persistence** — nothing web-specific; any full client would want this:
- `storage.insert_message()` for Direct, Public, FriendGroup messages
- `storage.insert_attachment()` / `storage.insert_message_attachment()` for inline attachments
- `storage.update_peer_online()` on `Meta::Online` / `Meta::Ack`
- `storage.upsert_reaction()` for reaction meta messages
- `storage.upsert_profile()` for profile update messages
- `storage.insert_notification()` for direct messages and replies targeting our own messages
- Friend request state machine: insert, refresh, block/ignore checks, auto-accept on mutual request
- On `Meta::FriendAccept`: `storage.insert_peer()` to add the new friend

**NOT** its responsibility:
- WebSocket / push notification events (web client concern)
- Group key distribution (currently in API handlers)

### Struct definition

```rust
pub struct StorageMessageHandler {
    storage: Storage,
    keypair: StoredKeypair,    // needed to sign outgoing envelopes (e.g. auto-accept)
    relay_url: Option<String>, // needed to post protocol replies
    my_peer_id: String,
}

impl StorageMessageHandler {
    pub fn new(storage: Storage, keypair: StoredKeypair, relay_url: Option<String>) -> Self { ... }
    pub fn storage(&self) -> &Storage { ... }
    pub fn into_storage(self) -> Storage { ... }
}

impl MessageHandler for StorageMessageHandler {
    fn on_message(&mut self, envelope: &Envelope, message: &ClientMessage) {
        // insert_message, inline attachments, insert_notification if DM/reply to us
    }

    fn on_meta(&mut self, meta: &MetaMessage) {
        // update_peer_online, friend request state machine,
        // insert_peer on FriendAccept, post auto-accept envelope if mutual request
    }

    fn on_raw_meta(&mut self, envelope: &Envelope, body: &str) {
        // upsert_reaction, upsert_profile
    }
}
```

## Composing Handlers

The web client wraps `StorageMessageHandler` and adds WebSocket broadcasting:

```rust
struct WebClientHandler {
    inner: StorageMessageHandler,
    ws_tx: tokio::sync::broadcast::Sender<WsEvent>,
}

impl MessageHandler for WebClientHandler {
    fn on_message(&mut self, envelope: &Envelope, message: &ClientMessage) {
        self.inner.on_message(envelope, message);
        let _ = self.ws_tx.send(WsEvent::NewMessage { ... });
    }

    fn on_meta(&mut self, meta: &MetaMessage) {
        self.inner.on_meta(meta);
        match meta {
            MetaMessage::Online { peer_id, .. } => {
                let _ = self.ws_tx.send(WsEvent::PeerOnline { peer_id: peer_id.clone() });
            }
            // ...
        }
    }
}
```

With this composition, `sync.rs` shrinks to: build a `WebClientHandler`, call
`relay_client.sync_inbox()`, done.

## Debugger Usage

```rust
let storage = Storage::open(&data_dir.join("tenet.db"))?;
let handler = StorageMessageHandler::new(storage, keypair.clone(), Some(relay_url.clone()));
client.set_handler(Box::new(handler));
```

After `client.sync_inbox()` returns, storage is already populated — no manual write calls needed.
The full friend request state machine, presence tracking, and reaction storage all work
identically to the web client.

## What Stays App-Specific

| Logic | Location | Used by |
|-------|----------|---------|
| Envelope processing (verify, decrypt, dispatch) | `RelayClient` | all clients |
| Standard persistence | `StorageMessageHandler` | web client, debugger, future CLI |
| WebSocket broadcast events | `WebClientHandler` (wraps `StorageMessageHandler`) | web client only |
| Push notifications, custom UI events | app-specific | each app |

The following are explicitly **not** moved into `StorageMessageHandler`:
- **WebSocket / push notification events** — transport-layer concerns.
- **Group key distribution** — currently done in API handlers; can become a first-class
  `MetaMessage` variant in a future revision.
- **Profile updates** — a convention on top of `Public`/`Meta` payloads; stored by
  `StorageMessageHandler` but meaning is app-defined.

## Migration Path

This design is backward-compatible from the caller's perspective if `SyncOutcome.messages` and
`SyncOutcome.errors` are kept as convenience fields.

| Step | Change |
|------|--------|
| 1 | Expand `SyncOutcome` with `events: Vec<SyncEvent>`; update `sync_inbox()` to handle all message kinds |
| 2 | Add `MessageHandler` trait and `RelayClient::set_handler()` |
| 3 | Introduce `StorageMessageHandler` in `src/message_handler.rs`; move protocol-level persistence out of `sync.rs` |
| 4 | Refactor `web_client/sync.rs` to use `WebClientHandler` wrapping `StorageMessageHandler` |
| 5 | Use `StorageMessageHandler` in the debugger |

**Note**: As of this writing, this design is a proposal. The current implementation has the split
described at the top of this document. See `src/web_client/sync.rs` for the current state.
