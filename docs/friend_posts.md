# Friends-Only Posts

This document describes the design for friends-only posts in Tenet: how they are created, how
encryption keys are distributed without revealing the friend list, how comments work across
mutually-unknown friends, and how the feature is implemented across all clients.

## Overview

Friends-only posts are messages broadcast to all of a peer's friends, but not to the wider public.
The core privacy challenge is that:

- Peer A's friends may not know each other.
- A friend commenting on A's post should **not** be able to discover the full list of other friends
  who have access, merely by virtue of having received the post.
- At the same time, anyone who can see the post implicitly accepts that other viewers can also see
  their comments (and their identity as the commenter).

The design solves this with **per-post ephemeral hidden groups**. Each friends-only post gets a
fresh symmetric key distributed individually to each friend via an encrypted Direct message. The
post and all its comments are encrypted with that key and broadcast to the relay. No membership
list is ever published — only the key is shared, one copy per friend.

Key properties:

- A new `MessageKind::FriendsPost` distinguishes friends-only posts from named `FriendGroup`
  messages, which carry an explicit persistent member list.
- Encryption reuses the same ChaCha20Poly1305 symmetric scheme already used by `FriendGroup`.
- Key delivery uses a new `MetaMessage::FriendsPostKey` variant, sent as a `Direct` (HPKE-
  encrypted) envelope to each friend individually.
- Commenters see each other's `sender_id` (needed for signature verification and attribution),
  but do **not** learn how many friends received the post or who they are.
- Adding new friends after a post is created requires an explicit re-share action by the creator.
- Key revocation is not possible once delivered; this is a documented limitation.

---

## Privacy Model

### Threat Model

The design protects against:

1. **Relay-level content access** — The relay stores encrypted ciphertexts and cannot read post
   bodies or comments.
2. **Non-friend peer discovery via public listing** — No broadcast enumerates who received the
   post; each friend gets only their own key copy.
3. **Comment-based friend-list inference** — A commenter cannot determine who else holds the key
   from the protocol alone. They see only the `sender_id` of peers who actually post comments.

The design does **not** protect against:

1. **Timing correlation at the relay** — When Alice creates a friends-only post, she immediately
   sends individual `Direct` key-delivery envelopes to each friend. A passive relay observer can
   correlate the timing of these Direct messages to estimate Alice's friend list size and
   potentially the recipient identities. This is a known metadata leak; see mitigations below.
2. **Key forwarding** — A friend who receives the key may share it with others. Trust in friends
   is a prerequisite, as with any symmetric key scheme.
3. **Post-comment commenter identity** — Because `sender_id` appears in the envelope header
   (plaintext, required for signature verification), all key-holders who receive a comment see
   the commenter's identity. This is intentional and matches the stated expectation.

### Timing Correlation Mitigations (Optional, Not Required for MVP)

- **Random delivery delay**: spread key deliveries over a randomised window (e.g., 0–30 minutes).
- **Decoy Direct messages**: send dummy encrypted envelopes to non-friends to obscure the fan-out.
- **Coalescing with other pending Directs**: piggyback key deliveries alongside unrelated Direct
  messages already queued for the same recipients.

---

## Protocol Changes

### New `MessageKind`: `FriendsPost`

```rust
pub enum MessageKind {
    Public,
    Meta,
    Direct,
    FriendGroup,
    StoreForPeer,
    FriendsPost,   // NEW
}
```

`FriendsPost` carries a symmetric-key-encrypted body (same scheme as `FriendGroup`) but with
ephemeral per-post key material. It is always broadcast to `recipient_id = "*"`.

**Validation rules** (extend `Header::validate_message_kind`):
- `group_id` must be non-empty (holds the ephemeral `post_group_id`).
- `store_for` and `storage_peer_id` are disallowed.
- `recipient_id` must be `"*"`.

### New `MetaMessage` Variant: `FriendsPostKey`

```rust
pub enum MetaMessage {
    // ... existing variants ...

    /// Delivers the decryption key for a friends-only post to a single friend.
    /// Sent as a Direct (HPKE-encrypted) envelope so only the intended recipient can read it.
    FriendsPostKey {
        /// `message_id` of the original `FriendsPost` envelope.
        post_id: String,
        /// The ephemeral group identifier embedded in the `FriendsPost` header's `group_id`.
        group_id: String,
        /// Base64 URL-safe no-pad encoded 32-byte ChaCha20Poly1305 symmetric key.
        post_key_b64: String,
        /// Unix timestamp (seconds) when the post was created.
        post_timestamp: u64,
    },
}
```

### New Content Type Constant

```rust
pub const FRIENDS_POST_CONTENT_TYPE: &str = "application/json;type=tenet.friends_post";
```

Encryption and decryption functions can be thin wrappers over the existing
`build_group_message_payload` / `decrypt_group_message_payload`, with the `group_id` used as AAD.

---

## Encryption

`FriendsPost` bodies are encrypted with **ChaCha20Poly1305** using the same scheme as
`FriendGroup`:

| Parameter | Value |
|-----------|-------|
| Key | 32-byte random `friends_post_key` (generated fresh per post) |
| Nonce | 12-byte random nonce (generated fresh per envelope, included in payload) |
| AAD | `post_group_id.as_bytes()` |
| Ciphertext | encrypted body bytes |

The resulting `Payload` has:
- `content_type`: `FRIENDS_POST_CONTENT_TYPE`
- `body`: JSON-encoded `GroupEncryptedPayload { nonce_b64, ciphertext_b64 }`

Comments on the post use **the same key and group_id** with a fresh nonce each time.

---

## Protocol Flows

### Creating a Friends-Only Post

```
Alice                          Relay                      Bob (Alice's friend)
  |                              |                              |
  | 1. gen post_group_id (rand)  |                              |
  | 2. gen friends_post_key (rand)|                             |
  | 3. encrypt body with key     |                              |
  | 4. build FriendsPost envelope|                              |
  |    kind=FriendsPost          |                              |
  |    group_id=post_group_id    |                              |
  |    recipient_id="*"          |                              |
  |------ POST envelope -------->|                              |
  | 5. store key locally         |                              |
  |    (friends_post_keys table) |                              |
  |                              |                              |
  | 6. for each friend F:        |                              |
  |    a. build MetaMessage::FriendsPostKey                     |
  |       { post_id, group_id, post_key_b64, post_timestamp }   |
  |    b. wrap in Direct envelope (HPKE-encrypted to F)         |
  |------ POST Direct to Bob --->|                              |
  |  (repeat for Carol, Dave...) |                              |
  |                              |                              |
  |                              |<-- Bob fetches inbox --------|
  |                              |--- deliver Direct to Bob --->|
  |                              |--- deliver FriendsPost "----->|
  |                              |                              | 7. decrypt Direct → FriendsPostKey
  |                              |                              | 8. store key in friends_post_keys
  |                              |                              | 9. decrypt FriendsPost with key
  |                              |                              | 10. display post
```

### Commenting on a Friends-Only Post

```
Bob (has key)                  Relay                   Carol (has key, unknown to Bob)
  |                              |                              |
  | 1. retrieve friends_post_key |                              |
  |    for group_id              |                              |
  | 2. encrypt comment with key  |                              |
  | 3. build FriendsPost envelope|                              |
  |    reply_to=post_id          |                              |
  |    group_id=post_group_id    |                              |
  |    recipient_id="*"          |                              |
  |------ POST envelope -------->|                              |
  |                              |                              |
  |                              |<-- Carol fetches inbox ------|
  |                              |--- deliver comment --------->|
  |                              |                              | 4. look up friends_post_keys
  |                              |                              |    by group_id → key found
  |                              |                              | 5. decrypt comment
  |                              |                              | 6. display as reply to post
  |                              |                              |    (can see Bob's sender_id)
```

Note: Carol now knows Bob's `sender_id` (because he commented) but **not** that Dave or Eve also
hold the key, since they haven't commented.

### Receiving a FriendsPost Before the Key Arrives

Because Alice posts the `FriendsPost` envelope and the `Direct` key-delivery envelopes in rapid
succession, there is a race: Bob may receive the `FriendsPost` before the `Direct`. The receiving
client should:

1. Recognise `MessageKind::FriendsPost` with an unknown `group_id`.
2. Store the raw envelope in the `messages` table with `body = NULL` (pending decryption).
3. When the `FriendsPostKey` meta arrives later, query `messages` for all rows with
   `message_kind = 'friends_post'` and `group_id = post_group_id` where `body IS NULL`.
4. Decrypt and update those rows in place.

---

## Storage Schema

### New Table: `friends_post_keys`

```sql
CREATE TABLE friends_post_keys (
    group_id     TEXT PRIMARY KEY,          -- ephemeral post_group_id (matches envelope group_id)
    post_id      TEXT NOT NULL,             -- message_id of the original FriendsPost
    post_key     BLOB NOT NULL,             -- 32-byte symmetric key
    creator_id   TEXT NOT NULL,             -- peer_id of the post author
    received_at  INTEGER NOT NULL,          -- unix timestamp when we got this key
    is_own_post  INTEGER NOT NULL DEFAULT 0 -- 1 if the local peer authored the post
);

CREATE INDEX idx_friends_post_keys_post_id ON friends_post_keys(post_id);
```

### Messages Table Extension

No new columns are required. The existing `messages` schema already supports:
- `message_kind = 'friends_post'`
- `group_id` to store the `post_group_id`
- `body = NULL` for pending-decryption envelopes
- `reply_to` for comment threading
- `raw_envelope` to hold the ciphertext until the key arrives

---

## Implementation Plan

### Phase 1 — Protocol Layer (`src/protocol.rs`)

1. Add `FriendsPost` variant to `MessageKind`.
2. Add `FriendsPostKey` variant to `MetaMessage`.
3. Add `FRIENDS_POST_CONTENT_TYPE` constant.
4. Extend `Header::validate_message_kind` with `FriendsPost` rules:
   - `group_id` non-empty
   - `recipient_id == "*"`
   - `store_for` / `storage_peer_id` absent
5. Add `build_friends_post_payload(plaintext, key, group_id)` and
   `decrypt_friends_post_payload(payload, key, group_id)` as wrappers over the existing
   `build_group_message_payload` / `decrypt_group_message_payload` (same crypto, different
   content type string for future protocol extensibility).

**Tests** (`tests/protocol_tests.rs`):
- Round-trip: encrypt then decrypt a `FriendsPost` payload with known key.
- Validation: `FriendsPost` without `group_id` is rejected.
- Validation: `FriendsPost` with `store_for` set is rejected.
- `MetaMessage::FriendsPostKey` serialises/deserialises correctly.

### Phase 2 — Storage Layer (`src/storage.rs`)

1. Define `FriendsPostKeyRow` struct.
2. Add migration (Phase N+1) to create the `friends_post_keys` table.
3. Implement storage methods:
   - `store_friends_post_key(row: &FriendsPostKeyRow) → Result<(), StorageError>`
   - `get_friends_post_key(group_id: &str) → Result<Option<FriendsPostKeyRow>, StorageError>`
   - `list_pending_friends_posts(group_id: &str) → Result<Vec<MessageRow>, StorageError>`
     (returns messages with `message_kind = 'friends_post'` and `group_id = X` and `body IS NULL`)
   - `decrypt_pending_friends_posts(group_id: &str, key: &[u8; 32]) → Result<u32, StorageError>`
     (decrypts and updates matching pending rows; returns count updated)

**Tests** (embedded unit tests in `storage.rs`):
- Store a key, retrieve it by `group_id`.
- Insert a pending FriendsPost row with `body = NULL`, then call `decrypt_pending_friends_posts`
  and confirm the row is updated.

### Phase 3 — Message Handler (`src/message_handler.rs`)

Extend `StorageMessageHandler`:

1. **Incoming `FriendsPost` envelope**:
   - Look up `friends_post_keys` by `group_id`.
   - If key found: decrypt and store with `body` populated.
   - If key not found: store raw envelope with `body = NULL`.

2. **Incoming `FriendsPostKey` meta** (via `on_meta`):
   - Store the key in `friends_post_keys`.
   - Call `decrypt_pending_friends_posts(group_id, &key)` to retroactively decrypt any buffered
     envelopes.
   - Optionally emit a notification that a new friends-only post is now readable.

**Tests** (`tests/client_sync_tests.rs`):
- Send a FriendsPost envelope before the key delivery; verify `body = NULL` in storage.
- Send the key delivery; verify pending posts are decrypted retroactively.
- Send the key before the post arrives; verify the post is decrypted immediately on receipt.

### Phase 4 — Web Client (`src/web_client/`)

#### New API Endpoints (`handlers/friends_posts.rs`)

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/api/messages/friends-post` | Create a friends-only post |
| `GET` | `/api/messages/friends-posts` | List friends-only posts (own + received) |
| `POST` | `/api/messages/:id/reply` | Extended to handle `friends_post` parent |

**`POST /api/messages/friends-post`** request body:
```json
{
  "body": "Hello friends!",
  "attachments": []
}
```

Handler logic:
1. Generate `post_group_id` (16 random bytes → base64url) and `friends_post_key` (32 random bytes).
2. Encrypt body → `FriendsPost` envelope, post to relay.
3. Store in `messages` and `friends_post_keys` (own post).
4. Fetch all friends from `peers` where `is_friend = 1`.
5. For each friend: build `MetaMessage::FriendsPostKey`, wrap in a `Direct` envelope, post to
   relay. Queue in `outbox` for retry if relay is offline.
6. Broadcast `WsEvent::NewMessage` with `message_kind = "friends_post"`.

**`GET /api/messages/friends-posts`** query params:
- `before` (timestamp), `limit` (default 50, max 200)

Returns all `FriendsPost` messages (own and received), ordered by `timestamp DESC`. Messages with
`body = NULL` are returned with a `"pending_decryption": true` flag so the UI can render a
placeholder.

**Reply support**: Extend `handlers/replies.rs` to recognise `message_kind == "friends_post"` and
look up the key from `friends_post_keys` instead of `groups`. Comment envelopes use the same
`post_group_id` and key.

#### Sync Loop (`sync.rs`)

In the existing inbox sync, add handling for:
- `MessageKind::FriendsPost` envelopes: delegate to `StorageMessageHandler` logic described above.
- `MetaMessage::FriendsPostKey` in Direct envelopes: store key, trigger retroactive decryption,
  emit `WsEvent::NewMessage` for newly decryptable posts.

#### WebSocket Events

Reuse the existing `WsEvent::NewMessage` with `message_kind = "friends_post"`. Add
`"pending_decryption": bool` to the JSON shape so the UI can handle the race condition.

#### Web UI (`web/src/app.js`, `web/src/styles.css`)

- Visibility selector on post composer: Public / Friends Only (new).
- `FriendsPost` timeline entries rendered with a lock icon and "Friends only" label.
- Comment box on a FriendsPost post works identically to public post comments.
- Pending-decryption posts shown as a greyed placeholder: "Waiting for decryption key…".
- Re-share panel: for own posts, list friends who can be added after post creation.

### Phase 5 — CLI (`src/bin/tenet.rs`)

New subcommands:

```
tenet post-friends <message> [--relay <url>]
    Creates a friends-only post and delivers keys to all known friends.

tenet list-friends-posts [--limit N]
    Lists received (and own) friends-only posts, newest first.
    Pending-decryption posts are shown with a [LOCKED] marker.

tenet reshare-friends-post <post-id> <peer-name>
    Sends the decryption key for an existing own post to a newly added friend.
```

**Tests** (`tests/cli_tests.rs` or inline):
- `post-friends` creates a FriendsPost envelope and Direct key-delivery envelopes.
- `list-friends-posts` returns the posted message.

### Phase 6 — Android FFI (`android/tenet-ffi/`)

#### `types.rs`

`FfiMessage.kind` already uses a String; add `"friends_post"` as a valid value. No struct changes
are required.

Add `FfiFriendsPostKey`:

```rust
pub struct FfiFriendsPostKey {
    pub group_id: String,
    pub post_id: String,
    pub creator_id: String,
    pub received_at: i64,
    pub is_own_post: bool,
}
```

#### `lib.rs` (new FFI functions)

```rust
fn post_friends_only(client: &TenetClient, body: String) -> Result<String, FfiError>;
// Returns the new message_id.

fn get_friends_posts(client: &TenetClient, before: Option<i64>, limit: u32)
    -> Result<Vec<FfiMessage>, FfiError>;
// Includes pending-decryption posts (FfiMessage.body == "" if body IS NULL).

fn reply_to_friends_post(client: &TenetClient, post_id: String, body: String)
    -> Result<String, FfiError>;
// Encrypts a comment with the post's key and broadcasts it.

fn reshare_friends_post_key(client: &TenetClient, post_id: String, peer_id: String)
    -> Result<(), FfiError>;
// Sends FriendsPostKey to a friend not yet in the distribution list.
```

#### UniFFI Interface (`tenet_ffi.udl`)

Expose the four new functions and `FfiFriendsPostKey` type in the UDL file.

#### Kotlin Layer (`app/`)

- `TenetRepository.kt`: wrap the four new FFI calls.
- `ui/timeline/`: display FriendsPost items with lock badge. Show loading shimmer for
  pending-decryption posts; refresh when a `FriendsPostKey` sync event arrives.
- `ui/compose/`: add a visibility toggle (Public / Friends Only).
- `data/SyncWorker.kt`: the existing sync loop handles `FriendsPostKey` meta delivery through the
  updated `StorageMessageHandler`; no separate sync path needed.

### Phase 7 — Simulation (`src/simulation/`)

#### `SimulationClient` Extensions (`src/client.rs`)

- Add `friends_post_keys: HashMap<String, [u8; 32]>` (group_id → key) alongside the existing
  `public_message_cache`.
- Add `pending_friends_posts: HashMap<String, Vec<Envelope>>` (group_id → buffered envelopes
  awaiting key delivery).
- Implement sending:
  - `send_friends_post(body, friends)` → builds one `FriendsPost` envelope + one `Direct`
    `FriendsPostKey` per friend.
- Implement receiving:
  - `receive_friends_post(envelope)` → if key known, decrypt and cache; else buffer.
  - `receive_friends_post_key(key_meta, envelope)` → store key, decrypt buffered posts.

#### Scenario Config (`simulation/config.rs`)

Extend `MessageTypeWeights`:

```toml
[simulation.message_type_weights]
direct       = 0.3
public       = 0.2
group        = 0.3
friends_post = 0.2   # NEW
```

`friends_post` messages target the sender's friend list (drawn from the simulation's
`friends_per_node` distribution). The simulation creates the key fan-out to each friend as
individual synthetic Direct envelopes routed through the relay.

#### Metrics (`simulation/metrics.rs`)

Track per-`FriendsPost` delivery:
- Key delivery latency to each friend (time from post to key receipt).
- Post decryption latency (time from post created to post decrypted on each friend's client).
- Comment propagation latency (time from comment sent to comment received by all key-holders).
- Pending-decryption buffer depth (how many posts await keys at each step).

### Phase 8 — Debugger (`src/bin/tenet-debugger.rs`)

New REPL commands:

```
friends-post <peer> <message>
    Peer <peer> creates a friends-only post with <message>.
    Keys are immediately distributed to all of <peer>'s simulated friends (in-process).

list-friends-posts <peer>
    Show all FriendsPost messages visible to <peer>, with decryption status.

friends-comment <peer> <post-id> <comment>
    Peer <peer> posts a comment on the given FriendsPost using the stored key.

reshare-key <author-peer> <post-id> <recipient-peer>
    Author sends the FriendsPostKey for <post-id> to <recipient-peer>.
    Useful for simulating late friend additions.

friends-post-info <post-id>
    Show which simulated peers hold the decryption key, which have received the post,
    and which have the post pending decryption.
```

---

## Simulation Scenario

Create `scenarios/friend_posts_test.toml` to verify:

1. Friends-only posts reach all of the creator's friends and are decrypted.
2. Non-friends cannot decrypt friends-only posts (checked by the metrics module).
3. Comments propagate to all key-holders.
4. The pending-decryption buffer resolves correctly when key delivery is delayed.
5. A late-joining friend (key re-shared manually) can retroactively decrypt the post.

```toml
# Scenario: Friends-Only Post Dynamics
# 10 peers in a sparse-friend graph; tests FriendsPost delivery and comment propagation.

[simulation]
node_ids = [
  "alice", "bob", "carol", "dave", "eve",
  "frank", "grace", "hank", "iris", "jack"
]
steps = 60
seed = 7777

[simulation.simulated_time]
seconds_per_step = 120
default_speed_factor = 1.0

[simulation.friends_per_node]
type = "uniform"
min = 2
max = 4

[simulation.post_frequency]
type = "poisson"
lambda_per_hour = 4.0

# All message weight goes to friends_post to focus the scenario
[simulation.message_type_weights]
direct       = 0.1
public       = 0.1
group        = 0.0
friends_post = 0.8

[simulation.groups]
count = 0

# Two cohorts to create the key-delivery race condition:
# "active" peers are online first and receive the post before the key.

[[simulation.cohorts]]
name = "active"
type = "always_online"
share = 0.4
[simulation.cohorts.message_type_weights]
direct       = 0.1
public       = 0.1
group        = 0.0
friends_post = 0.8

[[simulation.cohorts]]
name = "intermittent"
type = "rarely_online"
share = 0.6
online_probability = 0.35

[simulation.message_size_distribution]
type = "uniform"
min = 20
max = 100

[simulation.encryption]
type = "encrypted"

# 40 % of received FriendsPost messages trigger a comment reply.
[simulation.reaction_config]
reply_probability = 0.4

[simulation.reaction_config.reply_delay_distribution]
type = "log_normal"
mean  = 5.0
std_dev = 0.8

[relay]
ttl_seconds   = 3600
max_messages  = 2000
max_bytes     = 4194304
retry_backoff_seconds   = [1, 2, 4]
peer_log_window_seconds  = 30
peer_log_interval_seconds = 10

# Assertions evaluated at scenario end (checked by the test harness).
[simulation.assertions]
# Every friend of a FriendsPost author must eventually decrypt the post.
friends_post_delivery_rate_min = 0.95
# Non-friends must have zero decrypted FriendsPost content from any peer they are not friends with.
non_friend_decryption_count_max = 0
# 95th-percentile comment round-trip (post → comment → all key-holders) ≤ 10 steps.
comment_propagation_p95_steps_max = 10
```

---

## Testing Strategy

### Unit Tests

| Location | Coverage |
|----------|----------|
| `src/protocol.rs` | `FriendsPost` validation rules, `FriendsPostKey` serde round-trip |
| `src/storage.rs` | `store_friends_post_key`, `get_friends_post_key`, `decrypt_pending_friends_posts` |
| `src/message_handler.rs` | Pending buffering, retroactive decryption on key arrival |

### Integration Tests (`tests/`)

**`tests/friends_post_tests.rs`** (new file):

1. **Basic delivery**: Alice creates a friends-only post, Bob (friend) receives the key and post
   via a simulated relay; verify Bob can decrypt.
2. **Key-before-post race**: inject the `FriendsPostKey` Direct before the `FriendsPost` envelope;
   verify no buffering needed and post is decrypted on first receipt.
3. **Post-before-key race**: inject the `FriendsPost` envelope first; verify `body = NULL` in
   storage; then inject the key; verify retroactive decryption.
4. **Non-friend cannot decrypt**: Eve (not Alice's friend) receives the `FriendsPost` envelope but
   has no key; verify `body = NULL` persists and no decryption attempt is made.
5. **Comment propagation**: Bob comments on Alice's post; Carol (also Alice's friend) receives and
   decrypts the comment; verify `reply_to` is set correctly.
6. **Re-share key**: Alice uses `reshare_friends_post_key` to share the key with a late-added
   friend Frank; verify Frank can decrypt existing post and subsequent comments.
7. **Attachments**: friends-only post with an inline attachment; verify attachment data is
   included in the encrypted payload and decrypted correctly.

### Simulation Test

Run `cargo run --bin tenet-sim -- scenarios/friend_posts_test.toml` and verify all three
assertions pass (delivery rate ≥ 95 %, zero non-friend decryptions, comment P95 ≤ 10 steps).

### Debugger Smoke Test

```
$ cargo run --bin tenet-debugger -- --peers 5
> friends-post peer-1 "hello friends"
  post created (id=<post_id>, group=<group_id>)
  keys delivered to: peer-2, peer-3, peer-4, peer-5
> list-friends-posts peer-2
  [DECRYPTED] <post_id> from peer-1: "hello friends"
> friends-comment peer-2 <post_id> "nice post"
  comment sent (id=<cmt_id>)
> list-friends-posts peer-3
  [DECRYPTED] <post_id> from peer-1: "hello friends"
  [REPLY]     <cmt_id>  from peer-2: "nice post"
> friends-post-info <post_id>
  key holders: peer-1 (author), peer-2, peer-3, peer-4, peer-5
  received:    peer-1, peer-2, peer-3, peer-4, peer-5
  pending:     (none)
```

---

## REST API Summary

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/api/messages/friends-post` | Create a friends-only post |
| `GET` | `/api/messages/friends-posts` | List friends-only posts |
| `POST` | `/api/messages/:id/reply` | Comment on a post (extended to handle `friends_post`) |
| `GET` | `/api/messages/:id/replies` | List comments on a post (no change required) |
| `POST` | `/api/messages/:id/reshare-key` | Re-send decryption key to a newly added friend |

---

## Known Limitations and Deferred Work

- **Key revocation** is not supported. Once a `FriendsPostKey` is delivered, the recipient can
  retain it indefinitely. A future `FriendsPostRevoke` MetaMessage variant could signal intent,
  but cooperative enforcement is inherently advisory.
- **Relay TTL vs key delivery**: If a relay evicts a `FriendsPost` envelope before a friend comes
  online to retrieve it, that friend will miss the post even after receiving the key. The
  `reshare-friends-post` mechanism can re-deliver the key but not resurrect an evicted envelope.
- **Multi-device key sync**: Keys received on one device are not automatically propagated to the
  same user's other devices. Each device must receive its own `FriendsPostKey` delivery, which
  requires Alice to enumerate all of a friend's devices — not currently supported in the protocol.
- **Timing metadata leak**: as described in the Privacy Model section, the relay can correlate
  Direct key-delivery envelopes with a preceding `FriendsPost` to infer friend list size. Timing
  mitigations are documented but deferred.
- **Post editing or deletion**: not supported; the relay stores immutable envelopes.
- **Searchability**: because `FriendsPost` bodies are encrypted, relay-side search is impossible.
  Client-side full-text search of the local decrypted message store would be required.
- **Group-membership vs friends-only**: `FriendsPost` targets all current friends at creation
  time. There is no mechanism to target a subset of friends short of using a named `FriendGroup`.
