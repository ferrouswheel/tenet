# Client-Managed Storage: Design Proposal

## Problem Statement

Today, message processing logic is split across two places:

1. **`RelayClient.sync_inbox()`** (`src/client.rs`) — fetches from relay, verifies signatures, decrypts `Direct` messages, deduplicates, and returns a `SyncOutcome`. Only handles `Direct` messages; returns an error for all other kinds.

2. **`sync_once()`** (`src/web_client/sync.rs`) — the web client's own sync loop. It bypasses `RelayClient` entirely, re-implementing envelope fetching, signature verification, and decryption from scratch, while also handling `Meta`, `Public`, `FriendGroup`, reactions, profile updates, and group key distribution. All storage writes and WebSocket events happen here.

This creates two problems:

- **Duplication**: `sync.rs` reimplements ~200 lines of logic already in `RelayClient` (fetch, verify, decrypt, deduplicate). Any fix or protocol change must be applied in both places.
- **High bar for new clients**: A new client (the debugger, a CLI, a mobile client) wanting more than `Direct` messages must either bypass `RelayClient` the same way the web client does, or accept incomplete message handling.

The question is: **can message processing — verification, decryption, kind dispatch — live entirely inside the library, with callers receiving structured events and deciding what to do with them?**

---

## Feasibility Assessment

Yes, this is feasible and relatively contained. The core change is making `sync_inbox()` emit richer, kind-aware output for all message types, and optionally calling caller-provided hooks. No new dependencies are needed.

The key constraint is that **storage must not move into `RelayClient`** directly. `Storage` wraps a `rusqlite::Connection`, which is not `Clone` or `Send`, and its schema is application-level. The web client uses `Arc<Mutex<AppState>>` around storage; the debugger may use it differently. The client library should remain storage-agnostic.

Instead, the design uses two complementary mechanisms:

1. **Richer `SyncOutcome`** — a pull API: callers iterate over typed events and decide how to handle each.
2. **`MessageHandler` trait** — an optional push API: callers implement a trait and register it on the client; the client calls hooks during sync. Useful for reactive integrations (e.g., broadcasting WebSocket events immediately on message arrival).

---

## Proposed Design

### 1. Typed sync events

Replace the current flat `SyncOutcome` with an event-per-envelope model:

```rust
pub struct SyncEvent {
    pub envelope: Envelope,
    pub outcome: SyncEventOutcome,
}

pub enum SyncEventOutcome {
    /// Verified and decrypted successfully. Ready to store.
    Message(ClientMessage),

    /// A structured meta-message received from any sender (including unknown).
    Meta(MetaMessage),

    /// A structured meta-message that couldn't be parsed as MetaMessage.
    /// Payload is passed through for custom handling (e.g., reactions).
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
    pub messages: Vec<ClientMessage>,      // all SyncEventOutcome::Message entries
    pub errors: Vec<String>,               // all rejection reasons
}
```

`RelayClient.sync_inbox()` processes all message kinds and populates this. Callers who only care about received messages use `outcome.messages` as before. Callers who want full visibility iterate `outcome.events`.

### 2. `MessageHandler` trait

For clients that want to react to events during sync (e.g., emit a WebSocket event immediately rather than waiting for `sync_inbox()` to return):

```rust
pub trait MessageHandler: Send {
    /// Called for each successfully verified and decrypted message.
    fn on_message(&mut self, envelope: &Envelope, message: &ClientMessage) {}

    /// Called for each structured meta-message (Online, Ack, FriendRequest, etc.).
    fn on_meta(&mut self, meta: &MetaMessage) {}

    /// Called for raw meta payloads that couldn't be parsed as MetaMessage.
    /// The body string is the raw payload for custom dispatch (reactions, etc.).
    fn on_raw_meta(&mut self, envelope: &Envelope, body: &str) {}

    /// Called when a message is rejected (bad signature, unknown sender, etc.).
    fn on_rejected(&mut self, envelope: &Envelope, reason: &str) {}
}
```

All methods have default no-op implementations so callers only override what they need.

`RelayClient` accepts an optional boxed handler:

```rust
pub struct RelayClient {
    // existing fields ...
    handler: Option<Box<dyn MessageHandler>>,
}

impl RelayClient {
    pub fn set_handler(&mut self, handler: Box<dyn MessageHandler>) { ... }
    pub fn clear_handler(&mut self) { ... }
}
```

The web client's `AppState` implements `MessageHandler`, writes to storage, and sends `WsEvent`s. The debugger's handler writes to its per-peer `Storage`. The simulation client has no handler (it continues using its current in-process routing).

### 3. Meta message handling moves into the client

Currently, `MetaMessage` parsing and dispatch (Online, Ack, FriendRequest, FriendAccept) is done entirely in `sync.rs`. The proposed change moves the parsing step into `RelayClient.sync_inbox()`, so all callers benefit. The business-logic response (e.g., "update the peer's online status in the DB") stays in the caller's `MessageHandler`.

### 4. `RelayClient.sync_inbox()` handles all message kinds

Current `decode_envelope()` returns an error for any non-`Direct` message kind. After the change, `sync_inbox()` internally routes:

- `Direct` → verify signature → decrypt → `SyncEventOutcome::Message`
- `Public` → verify signature → `SyncEventOutcome::Message` (body is plaintext)
- `FriendGroup` → verify signature → decrypt with group key → `SyncEventOutcome::Message`
- `Meta` → attempt `MetaMessage` parse → `SyncEventOutcome::Meta` or `SyncEventOutcome::RawMeta`
- `StoreForPeer` → not yet used; pass through as `SyncEventOutcome::Message`

### 5. Storage stays external

`RelayClient` never calls `Storage` directly. Storage writes are always the caller's responsibility, triggered by events or handler callbacks. This preserves:

- Clean separation between protocol and persistence
- Multiple storage backends (SQLite via `Storage`, in-memory for tests, no storage for simulations)
- No `rusqlite` coupling in the client library

---

## Shared Persistence Layer: `StorageMessageHandler`

The storage writes in `web_client/sync.rs` split cleanly into two categories:

**Protocol-level persistence** — nothing web-specific; any full client would want this:
- `storage.insert_message()` for Direct, Public, FriendGroup messages
- `storage.insert_attachment()` / `storage.insert_message_attachment()` for inline attachments
- `storage.update_peer_online()` on `Meta::Online` / `Meta::Ack`
- `storage.upsert_reaction()` for reaction meta messages
- `storage.upsert_profile()` for profile update messages
- `storage.insert_notification()` for direct messages and replies targeting our own messages
- Friend request state machine: insert, refresh, block/ignore checks, auto-accept on mutual request
- On `Meta::FriendAccept`: `storage.insert_peer()` to add the new friend

**Transport-level side effects** — web-specific, should not be shared:
- `ws_tx.send(WsEvent::NewMessage { ... })`
- `ws_tx.send(WsEvent::PeerOnline { ... })`
- `ws_tx.send(WsEvent::Notification { ... })`
- `ws_tx.send(WsEvent::FriendRequestReceived { ... })`

This means a `StorageMessageHandler` struct can encapsulate all the protocol-level persistence and be used by both the web client and the debugger. The web client wraps it inside its own handler and adds WebSocket broadcasting on top.

### `StorageMessageHandler` struct

Lives in a new module `src/message_handler.rs` (part of the library, not `web_client/`):

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
    // on_rejected: default no-op
}
```

The friend request auto-accept logic (currently ~150 lines in `process_incoming_friend_request`) moves entirely into `on_meta`. It needs `keypair` and `relay_url` for the auto-accept envelope — both are held by `StorageMessageHandler`.

### Web client composition

`web_client/sync.rs` defines a thin wrapper:

```rust
struct WebClientHandler {
    inner: StorageMessageHandler,
    ws_tx: tokio::sync::broadcast::Sender<WsEvent>,
}

impl MessageHandler for WebClientHandler {
    fn on_message(&mut self, envelope: &Envelope, message: &ClientMessage) {
        self.inner.on_message(envelope, message);
        // additionally fire WsEvent::NewMessage and WsEvent::Notification
        let _ = self.ws_tx.send(WsEvent::NewMessage { ... });
    }

    fn on_meta(&mut self, meta: &MetaMessage) {
        self.inner.on_meta(meta);
        // additionally fire WsEvent::PeerOnline, WsEvent::FriendRequestReceived, etc.
        match meta {
            MetaMessage::Online { peer_id, .. } => {
                let _ = self.ws_tx.send(WsEvent::PeerOnline { peer_id: peer_id.clone() });
            }
            ...
        }
    }

    fn on_raw_meta(&mut self, envelope: &Envelope, body: &str) {
        self.inner.on_raw_meta(envelope, body);
        // fire WsEvent::Reaction if needed
    }
}
```

`sync.rs` shrinks to: build a `WebClientHandler`, call `relay_client.sync_inbox()`, done.

### Debugger usage

```rust
struct DebugPeer {
    name: String,
    client: RelayClient,   // has WebClientHandler or StorageMessageHandler registered
    data_dir: PathBuf,
}
```

At peer creation time:

```rust
let storage = Storage::open(&data_dir.join("tenet.db"))?;
let handler = StorageMessageHandler::new(storage, keypair.clone(), Some(relay_url.clone()));
client.set_handler(Box::new(handler));
```

After `client.sync_inbox()` returns, the debugger can access stored messages via `storage()` without any additional manual writes. The full friend request state machine, presence tracking, and reaction storage all work identically to the web client.

### What is shared vs. what is not

| Logic | Location | Used by |
|-------|----------|---------|
| Envelope processing (verify, decrypt, dispatch) | `RelayClient` | all clients |
| Standard persistence (messages, peers, friends, reactions, profiles) | `StorageMessageHandler` | web client, debugger, future CLI |
| WebSocket broadcast events | `WebClientHandler` (wraps `StorageMessageHandler`) | web client only |
| Push notifications, custom UI events | app-specific | each app |

---

## Migration Path

The change is backward-compatible from the caller's perspective if `SyncOutcome.messages` and `SyncOutcome.errors` are kept as convenience fields. The steps:

### Step 1: Expand `SyncOutcome` with events

Add the `events: Vec<SyncEvent>` field to the existing `SyncOutcome`. Keep `messages` and `errors` as derived views. Update `sync_inbox()` to handle all message kinds and populate `events`. Existing callers are unaffected.

### Step 2: Add `MessageHandler` trait and `set_handler()`

No callers need to use it yet; the old `SyncOutcome`-based approach still works.

### Step 3: Introduce `StorageMessageHandler`

Create `src/message_handler.rs`. Move all protocol-level persistence logic out of `sync.rs` into `StorageMessageHandler::on_message`, `on_meta`, and `on_raw_meta`. This includes:

- Message and attachment storage
- Peer online status updates
- Friend request state machine (including auto-accept envelope posting)
- Reaction and profile storage
- Notification creation for DMs and replies

No web-specific code moves here.

### Step 4: Refactor `web_client/sync.rs`

Replace the hand-rolled envelope processing loop with:

1. A `WebClientHandler` that wraps `StorageMessageHandler` and adds `WsEvent` broadcasting.
2. A call to `relay_client.sync_inbox()`.

`sync.rs` shrinks from ~400 lines to ~50.

### Step 5: Use `StorageMessageHandler` in the debugger

The debugger creates a `StorageMessageHandler` per peer at startup. After `client.sync_inbox()`, storage is already populated — no manual write calls needed.

---

## What Stays App-Specific

The following are explicitly **not** moved into `StorageMessageHandler` or `RelayClient`:

- **WebSocket / push notification events** — transport-layer concerns; web client keeps these in its `WebClientHandler` wrapper.
- **Group key distribution** — currently done manually in web client API handlers; can remain there or be encoded as a first-class `MetaMessage` variant in a future protocol revision.
- **Profile updates** — a convention on top of `Public`/`Meta` payloads, not a core protocol type yet; stored by `StorageMessageHandler` but the meaning is app-defined.

---

## Summary

| Concern | Today | Proposed |
|---------|-------|----------|
| Fetch envelopes from relay | `RelayClient` + duplicated in `sync.rs` | `RelayClient` only |
| Signature verification | `RelayClient` + duplicated in `sync.rs` | `RelayClient` only |
| Decryption (Direct) | `RelayClient` + duplicated in `sync.rs` | `RelayClient` only |
| Decryption (Group) | Only in `sync.rs` | `RelayClient` |
| Meta message parsing | Only in `sync.rs` | `RelayClient` |
| Deduplication | `RelayClient` + re-checked in `sync.rs` | `RelayClient` only |
| Message + attachment storage | Only in `sync.rs` | `StorageMessageHandler` (shared) |
| Peer presence tracking | Only in `sync.rs` | `StorageMessageHandler` (shared) |
| Friend request state machine | Only in `sync.rs` | `StorageMessageHandler` (shared) |
| Reaction + profile storage | Only in `sync.rs` | `StorageMessageHandler` (shared) |
| Notification creation | Only in `sync.rs` | `StorageMessageHandler` (shared) |
| WebSocket events | `sync.rs` | `WebClientHandler` (web only) |

The net result: one canonical place for "is this envelope valid and what does it mean" (`RelayClient`), one canonical place for "how does tenet persist received data" (`StorageMessageHandler`), and each application layer adds only what is specific to its transport or UI.
