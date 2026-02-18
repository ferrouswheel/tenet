# Building Clients with the Tenet Library

This document describes how to build a client on top of the Tenet library, covering message
processing, the `MessageHandler` trait, `StorageMessageHandler`, and how to compose handlers
for application-specific behaviour.

## Sync and the `MessageHandler` Trait

`RelayClient::sync_inbox()` (`src/client.rs`) is the central sync entry point. It fetches
envelopes from the relay, verifies signatures, decrypts payloads, and dispatches to a registered
`MessageHandler` — one callback per envelope. It also returns a `SyncOutcome` for callers that
want to inspect results after the fact.

```rust
pub struct SyncOutcome {
    pub events: Vec<SyncEvent>,   // one per envelope, including errors/duplicates
    pub fetched: usize,
    pub messages: Vec<ClientMessage>,  // convenience: successfully decoded messages
    pub errors: Vec<String>,           // convenience: rejection reasons
}
```

### Registering a Handler

```rust
impl RelayClient {
    pub fn set_handler(&mut self, handler: Box<dyn MessageHandler>);
    pub fn clear_handler(&mut self);
}
```

The handler receives callbacks during `sync_inbox()`. Any `Vec<Envelope>` returned from
`on_message`, `on_meta`, or `on_raw_meta` is posted to the relay automatically by `sync_inbox`
before it returns — so auto-reply envelopes (friend-accept responses, group-invite accepts, etc.)
are sent without the caller needing to do anything extra.

### The Trait

```rust
pub trait MessageHandler: Send {
    /// Called for each successfully verified and decrypted message.
    /// Return any outgoing envelopes to send (e.g. acknowledgements).
    fn on_message(&mut self, envelope: &Envelope, message: &ClientMessage) -> Vec<Envelope>;

    /// Called for each structured meta-message (Online, Ack, FriendRequest, etc.).
    /// Return any outgoing envelopes to send (e.g. FriendAccept, GroupInviteAccept).
    fn on_meta(&mut self, meta: &MetaMessage) -> Vec<Envelope>;

    /// Called for raw meta payloads that couldn't be parsed as a MetaMessage variant.
    /// Used for reactions and other custom meta formats.
    fn on_raw_meta(&mut self, envelope: &Envelope, body: &str) -> Vec<Envelope>;

    /// Called when a message is rejected (bad signature, unknown sender, etc.).
    fn on_rejected(&mut self, envelope: &Envelope, reason: &str);

    /// Drain group keys received via group_key_distribution messages during this sync.
    /// The caller should apply these to the client's in-memory GroupManager.
    fn take_pending_group_keys(&mut self) -> Vec<(String, Vec<u8>)>;

    /// Called when the local peer creates a new group, so the handler can persist
    /// the group row and outgoing invite rows to storage.
    fn on_group_created(
        &mut self,
        group_id: &str,
        group_key: &[u8; 32],
        creator_id: &str,
        members: &[String],
    );
}
```

All methods have default no-op implementations (returning empty vecs / doing nothing), so
implementations only override what they need.

## `StorageMessageHandler`

`StorageMessageHandler` (`src/message_handler.rs`) implements `MessageHandler` and handles
all standard protocol-level persistence. It is part of the library (not tied to any specific
client binary), so any client can use it as-is or wrap it.

### What it handles

- **Messages**: `Direct`, `Public`, `FriendGroup`, `StoreForPeer` stored as `MessageRow`
- **Attachments**: inline attachment data decoded and stored as `AttachmentRow`
- **Presence**: peer `online` + `last_seen_online` updated on `Meta::Online` / `Meta::Ack`
- **Friend requests**: full state machine — insert new, refresh duplicate, block/ignore checks,
  auto-accept on mutual request (returns a `FriendAccept` envelope to send)
- **Friend accept**: adds the new friend as a peer row
- **Reactions**: stored from raw meta payloads; creates a notification if it targets own message
- **Profile updates**: `tenet.profile` message type stored (and avatar attachment data if present)
- **Notifications**: created for direct messages, replies, reactions, and group invites
- **Group invites**: stored as **pending** with a notification; does **not** auto-accept —
  the application layer is responsible for accepting or ignoring
- **Group key distribution**: validates consent via an accepted invite, stores the group key and
  self-membership, signals the caller via `take_pending_group_keys()`
- **Group creation**: `on_group_created` persists the group row, self-membership, and outgoing
  invite rows

**Not** its responsibility: WebSocket/push events, sending the initial invite envelopes,
anything UI- or transport-specific.

### Constructors

```rust
impl StorageMessageHandler {
    /// Basic constructor. Group key distribution is disabled (no crypto params).
    pub fn new(storage: Storage, keypair: StoredKeypair) -> Self;

    /// Full constructor. Required for sending group_key_distribution envelopes
    /// when processing GroupInviteAccept messages.
    pub fn new_with_crypto(
        storage: Storage,
        keypair: StoredKeypair,
        hpke_info: Vec<u8>,
        payload_aad: Vec<u8>,
    ) -> Self;

    pub fn storage(&self) -> &Storage;
    pub fn storage_mut(&mut self) -> &mut Storage;
    pub fn into_storage(self) -> Storage;
}
```

### Group keys after sync

`StorageMessageHandler` persists received group keys to SQLite but cannot update the client's
in-memory `GroupManager` (it has no reference to the client). After sync, the caller must reload
any new groups into the `GroupManager` so that `send_group_message` can encrypt with them:

```rust
client.sync_inbox(None)?;
client.clear_handler();

// Reload any newly received group keys into the in-memory GroupManager.
for row in storage.list_groups()? {
    if client.get_group(&row.group_id).is_none() {
        client.group_manager_mut().add_group_key(row.group_id, row.group_key);
    }
}
```

### Storage connections and concurrency

`StorageMessageHandler` takes ownership of a `Storage` connection. In async contexts (like the
web client) where the client and its `AppState` are shared across tasks, open a second WAL-mode
connection to the same database file for the handler — SQLite WAL mode allows concurrent reads
and one writer without blocking:

```rust
let handler_storage = Storage::open(&db_path)?;  // second connection, same file
let handler = StorageMessageHandler::new_with_crypto(handler_storage, keypair, info, aad);
client.set_handler(Box::new(handler));
client.sync_inbox(None)?;
```

In single-threaded contexts (like the debugger), the same pattern works fine.

## Composing Handlers

Wrap `StorageMessageHandler` to add application-specific side effects. Call `inner` first so
storage is written before your side effects fire.

**Web client** — adds WebSocket broadcasting:

```rust
struct WebClientHandler {
    inner: StorageMessageHandler,
    ws_tx: broadcast::Sender<WsEvent>,
    my_peer_id: String,
}

impl MessageHandler for WebClientHandler {
    fn on_message(&mut self, envelope: &Envelope, message: &ClientMessage) -> Vec<Envelope> {
        let result = self.inner.on_message(envelope, message);
        let _ = self.ws_tx.send(WsEvent::NewMessage { ... });
        result
    }

    fn on_meta(&mut self, meta: &MetaMessage) -> Vec<Envelope> {
        let result = self.inner.on_meta(meta);
        if let MetaMessage::Online { peer_id, .. } = meta {
            let _ = self.ws_tx.send(WsEvent::PeerOnline { peer_id: peer_id.clone() });
        }
        result
    }

    // delegate the rest ...
}
```

**Debugger** — auto-accepts group invites (appropriate for automated testing):

```rust
struct DebuggerMessageHandler {
    inner: StorageMessageHandler,
    my_peer_id: String,
    signing_private_key_hex: String,
}

impl MessageHandler for DebuggerMessageHandler {
    fn on_meta(&mut self, meta: &MetaMessage) -> Vec<Envelope> {
        let mut outgoing = self.inner.on_meta(meta);  // stores invite as "pending"
        if let MetaMessage::GroupInvite { peer_id: inviter_id, group_id, .. } = meta {
            // Find the invite that inner just stored, mark it accepted, and
            // return a GroupInviteAccept envelope for sync_inbox to post.
            if let Ok(Some(invite)) = self.inner.storage_mut()
                .find_group_invite(group_id, inviter_id, &self.my_peer_id, "incoming")
            {
                if invite.status == "pending" {
                    let _ = self.inner.storage_mut()
                        .update_group_invite_status(invite.id, "accepted");
                    // build and push GroupInviteAccept envelope onto outgoing ...
                }
            }
        }
        outgoing
    }

    // delegate the rest ...
}
```

## What Stays App-Specific

| Concern | Where it lives |
|---------|---------------|
| Envelope fetch, verify, decrypt, dispatch | `RelayClient` — all clients |
| Standard protocol persistence | `StorageMessageHandler` — all clients |
| WebSocket / push events | Wrapper handler — web client only |
| Auto-accept group invites | Wrapper handler — debugger only |
| UI state, notifications display | Application layer |
