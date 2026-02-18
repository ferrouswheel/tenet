# How to Work with the Tenet Library

This document describes patterns and recommendations for building clients on top of the Tenet
library, with a focus on message processing, storage integration, and the `StorageMessageHandler`
design.

## Background: Message Processing Architecture

Message processing uses `RelayClient.sync_inbox()` (`src/client.rs`) as the central point for
fetching from the relay, verifying signatures, decrypting, and emitting typed `SyncEventOutcome`
events for all message kinds (`Direct`, `Public`, `FriendGroup`, `Meta`, `StoreForPeer`).

The web client's `sync_once()` (`src/web_client/sync.rs`) consumes these events and handles
storage writes plus WebSocket broadcasts. It does **not** yet use `StorageMessageHandler`
directly — it still has its own `process_meta_event`, `process_message_event`, etc. functions
that duplicate some of the storage logic. Migrating `sync.rs` to delegate to
`StorageMessageHandler` is planned but not yet done (see Migration Path below).

## Design: `MessageHandler` Trait

`sync_inbox()` emits typed `SyncEventOutcome` events for all message kinds. Callers can also
register a `MessageHandler` implementation on the client for a push-style integration where
storage writes happen during sync rather than after it returns.

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

`StorageMessageHandler` is implemented in `src/message_handler.rs` (part of the library, not
`web_client/`). It handles all standard protocol-level persistence automatically during sync.

### What it handles

- `Direct`, `Public`, `FriendGroup`, `StoreForPeer` message storage
- Inline attachment data (stored per `AttachmentRow`)
- Peer online status updates on `Meta::Online` / `Meta::Ack`
- Friend request state machine: insert, refresh, block/ignore checks, auto-accept on mutual request
- On `Meta::FriendAccept`: adds the new friend as a peer
- Reaction storage from raw meta payloads
- Profile updates (`tenet.profile` message type)
- Notifications for direct messages, replies, reactions, and group invites
- Group invite flow: **auto-accepts** incoming `GroupInvite` meta messages (sends back
  `GroupInviteAccept` immediately without user confirmation — see note below)
- Group key distribution: validates consent via accepted invite, stores group key

> **Note on group invites**: `StorageMessageHandler` auto-accepts all group invites. This is
> appropriate for automated clients (debugger, simulations). The web client's own `sync.rs`
> does NOT use `StorageMessageHandler` for this — it stores the invite as pending and shows a
> UI prompt, letting the user explicitly accept or ignore via `/api/group-invites/:id/accept`.

**NOT** its responsibility:
- WebSocket / push notification events (web client concern)
- Relay posting of outgoing envelopes (caller must drain and post them)

### Struct definition

```rust
pub struct StorageMessageHandler { /* ... */ }

impl StorageMessageHandler {
    /// Basic constructor; group key distribution disabled (no HPKE params).
    pub fn new(storage: Storage, keypair: StoredKeypair) -> Self { ... }

    /// Full constructor with HPKE params for group key distribution.
    pub fn new_with_crypto(
        storage: Storage,
        keypair: StoredKeypair,
        hpke_info: Vec<u8>,
        payload_aad: Vec<u8>,
    ) -> Self { ... }

    pub fn storage(&self) -> &Storage { ... }
    pub fn storage_mut(&mut self) -> &mut Storage { ... }
    pub fn into_storage(self) -> Storage { ... }

    /// Return any group keys received during sync (to apply to GroupManager).
    pub fn take_pending_group_keys(&mut self) -> Vec<(String, Vec<u8>)> { ... }
}
```

`on_meta` and `on_message` return `Vec<Envelope>` — outgoing envelopes the caller must post to
the relay (e.g. auto-accept friend request responses, group invite acceptances). The caller is
responsible for posting these via `relay_transport::post_envelope()`.

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
let handler = StorageMessageHandler::new(storage, keypair.clone());
client.set_handler(Box::new(handler));
```

Use `new_with_crypto(storage, keypair, hpke_info, payload_aad)` if you need the handler to send
group key distribution envelopes when processing `GroupInviteAccept` messages.

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

| Step | Status | Change |
|------|--------|--------|
| 1 | ✅ Done | `SyncOutcome` with `events: Vec<SyncEvent>`; `sync_inbox()` handles all message kinds |
| 2 | ✅ Done | `MessageHandler` trait and `RelayClient::set_handler()` |
| 3 | ✅ Done | `StorageMessageHandler` in `src/message_handler.rs` |
| 4 | ⬜ TODO | Refactor `web_client/sync.rs` to use `WebClientHandler` wrapping `StorageMessageHandler` |
| 5 | ⬜ TODO | Use `StorageMessageHandler` in the debugger (currently uses direct storage writes) |

Step 4 is the main remaining work: the web client's `sync.rs` still contains its own
`process_meta_event`, `process_message_event`, etc. that duplicate logic in
`StorageMessageHandler`. Once step 4 is complete, `sync.rs` shrinks to building a
`WebClientHandler` and calling `relay_client.sync_inbox()`.
