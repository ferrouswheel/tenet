# Implementation Review — Phases 1–5

This document reviews the implementation of phases 1 through 5 of [PLAN.md](PLAN.md)
and proposes ten improvements ranked from most to least important. The first
five include enough detail that they can be implemented directly.

---

## Scope reviewed

| Phase | Files examined |
|-------|---------------|
| 1 — Foundation | `src/storage.rs`, `src/bin/tenet-web.rs` (startup, config, relay sync), `build.rs` |
| 2 — Core API & Timeline | `src/bin/tenet-web.rs` (message handlers, WS hub), `web/src/app.js` (timeline) |
| 3 — Peers & Presence | `src/bin/tenet-web.rs` (peer handlers, presence tracking), `web/src/app.js` (friends list) |
| 4 — Groups | `src/bin/tenet-web.rs` (group handlers, key distribution) |
| 5 — Direct Messaging | `src/bin/tenet-web.rs` (conversation handlers), `web/src/app.js` (DM views) |

---

## Improvement suggestions (ranked)

### 1. Extract a relay-posting helper and report failures to the caller

**Problem.** Every handler that sends a message to a relay repeats the same
pattern and silently discards errors:

```rust
// src/bin/tenet-web.rs — six occurrences, e.g. line 430
if let Some(ref relay_url) = st.relay_url {
    let url = format!("{}/envelopes", relay_url.trim_end_matches('/'));
    if let Ok(json_val) = serde_json::to_value(&envelope) {
        let _ = ureq::post(&url).send_json(json_val);
    }
}
```

There are six copies of this block
(lines 427–432, 513–518, 614–620, 991–996, 1110–1116, 1508–1512).
Because the `Result` from `ureq::post(…).send_json(…)` is discarded with
`let _ =`, the caller never knows the relay rejected the message. The user sees
"sent" in the UI while the envelope may not have reached the relay. The
serialisation step (`serde_json::to_value`) is also silently dropped.

**Why it matters.** This is both a robustness and a UX issue. Users will believe
messages were delivered when they were not, with no indication of failure. It
also makes debugging relay connectivity problems harder.

**Proposed change.**

1. Create a helper function in `tenet-web.rs`:

```rust
/// Post an envelope to the relay. Returns Ok(()) on success, or an error
/// string describing what went wrong.
fn post_to_relay(relay_url: &str, envelope: &Envelope) -> Result<(), String> {
    let url = format!("{}/envelopes", relay_url.trim_end_matches('/'));
    let json_val = serde_json::to_value(envelope)
        .map_err(|e| format!("failed to serialize envelope: {e}"))?;
    ureq::post(&url)
        .send_json(json_val)
        .map_err(|e| format!("relay POST failed: {e}"))?;
    Ok(())
}
```

2. Replace all six occurrences of the inline relay-posting block with a call to
   this helper.

3. In the message-sending handlers (`send_direct_handler`,
   `send_public_handler`, `send_group_handler`), propagate relay errors back to
   the API response. When a relay post fails, the handler should still persist
   the message locally (the outbox exists for this purpose) but the JSON
   response should include a field like `"relay_delivered": false` alongside a
   `"relay_error"` string so the frontend can inform the user.

4. In fire-and-forget contexts (key distribution during group creation, online
   announcements), log the error with `eprintln!` instead of discarding it.

5. Update the frontend's `sendMessage()` function in `web/src/app.js` to check
   the `relay_delivered` field and display a warning toast when it is `false`.

---

### 2. Wrap group creation and key distribution in a transaction

**Problem.** `create_group_handler` (line 864) performs three sequential
database operations — insert group, insert members, then distribute the group
key to each member via the relay. If any step fails partway through, the
database is left in an inconsistent state: the group may exist without all its
members, or members may be recorded without having received the key.

For example, if the relay is unreachable during key distribution, some members
will be added to `group_members` but will never receive the group key. The
handler returns `201 Created` regardless. The same problem exists in
`add_group_member_handler` (line 1014).

**Why it matters.** Users who are added to a group but never receive the key
will silently fail to decrypt group messages. There is no mechanism to retry
key distribution later.

**Proposed change.**

1. Use `rusqlite`'s transaction support to make group creation atomic. Add a
   method to `Storage`:

```rust
pub fn transaction(&self) -> Result<rusqlite::Transaction<'_>, StorageError> {
    Ok(self.conn.transaction()?)
}
```

The caller would also need methods that accept a `&rusqlite::Transaction`
instead of `&self` for the inner operations, or the simplest path is to
start a transaction, perform the raw SQL, and commit. However, since `Storage`
currently owns the `Connection` privately, the cleanest approach is:

2. Add `insert_group_with_members` to `Storage` that wraps the group insert
   and all member inserts in a single `BEGIN … COMMIT` block:

```rust
pub fn insert_group_with_members(
    &self,
    group: &GroupRow,
    members: &[GroupMemberRow],
) -> Result<(), StorageError> {
    let tx = self.conn.unchecked_transaction()?;
    // insert group
    tx.execute(
        "INSERT OR REPLACE INTO groups ...",
        params![...],
    )?;
    // insert members
    for m in members {
        tx.execute(
            "INSERT OR IGNORE INTO group_members ...",
            params![...],
        )?;
    }
    tx.commit()?;
    Ok(())
}
```

3. Track which members failed key distribution. After the transaction commits,
   attempt key distribution for each member. Collect the IDs of members for
   whom distribution failed and include them in the API response:

```json
{
    "group_id": "...",
    "members": ["a", "b", "c"],
    "key_distribution_failed": ["c"]
}
```

4. Store a record of pending key distributions (e.g. a new
   `pending_key_distributions` table, or a flag on `group_members`) so a
   background task or manual retry can complete them later.

---

### 3. Extract the 1 500-line `tenet-web.rs` into modules

**Problem.** `src/bin/tenet-web.rs` is 1 529 lines in a single file. It
contains configuration, shared state types, all API handlers (messages, peers,
groups, conversations), WebSocket logic, background sync, online announcement,
static asset serving, and `main()`. This makes the file hard to navigate and
difficult to test individual handlers in isolation.

**Why it matters.** Modularity directly affects maintainability. As phases 6–11
add more endpoints, this file will continue to grow. Handler functions cannot
be unit-tested without standing up the full `SharedState`; splitting them out
would allow lighter-weight test harnesses.

**Proposed change.**

Create a `src/web/` module directory (or `src/api/`) with the following structure:

```
src/web/
    mod.rs          — re-exports, SharedState type, AppState struct
    config.rs       — Config struct and from_env()
    state.rs        — AppState, SharedState, WsEvent enum
    handlers/
        mod.rs      — re-exports all handler modules
        health.rs   — health_handler
        messages.rs — list_messages_handler, get_message_handler,
                      send_direct_handler, send_public_handler,
                      send_group_handler, mark_read_handler
        peers.rs    — list_peers_handler, get_peer_handler,
                      add_peer_handler, delete_peer_handler
        groups.rs   — list_groups_handler, get_group_handler,
                      create_group_handler, add_group_member_handler,
                      remove_group_member_handler, leave_group_handler
        conversations.rs — list_conversations_handler, get_conversation_handler
    ws.rs           — ws_handler, ws_connection
    sync.rs         — relay_sync_loop, sync_once, announce_online
    assets.rs       — Assets struct, static_handler
    helpers.rs      — api_error, post_to_relay, now_secs
```

Then `src/bin/tenet-web.rs` would shrink to roughly:

```rust
use tenet::web::{config::Config, build_router};

#[tokio::main]
async fn main() {
    let config = Config::from_env();
    let (app, _background_handles) = build_router(config).await;
    // bind and serve
}
```

Each handler module only needs to import `SharedState` and `api_error`, keeping
coupling low.

**Steps:**

1. Create `src/web/mod.rs` with `pub mod config; pub mod state; ...` etc.
2. Move each logical group of functions into its own file.
3. Update `src/lib.rs` with `pub mod web;`.
4. Move `src/bin/tenet-web.rs` to use the new modules for its router setup.
5. Verify with `cargo build --bin tenet-web` and `cargo test`.

---

### 4. Avoid holding the `Mutex` across blocking I/O in handlers

**Problem.** Most handlers acquire `state.lock().await` at the top and hold it
for the entire function body, including synchronous `ureq` HTTP calls to the
relay. For example, `send_direct_handler` (line 363) locks the state, does a
database lookup, builds cryptographic material, and then calls
`ureq::post(…).send_json(…)` — all while holding the `Mutex`.

Because `ureq` is a synchronous HTTP client, this blocks the Tokio thread and
prevents *any other handler* from accessing state until the relay responds (or
times out). Under normal conditions this is ~100 ms, but if the relay is slow
or unreachable, every API request queues behind the lock.

**Why it matters.** This is a concurrency bottleneck. The relay sync loop
(line 1332) already demonstrates the correct pattern: extract what you need
under a short lock, then release it before doing I/O. The handlers should
follow the same pattern.

**Proposed change.**

For each message-sending handler:

1. Acquire the lock.
2. Extract (clone) everything needed: keypair, relay_url, peer/group data.
3. Drop the lock (explicitly with `drop(st)` or by ending the block).
4. Perform cryptographic operations and relay posting *outside* the lock.
5. Re-acquire the lock briefly to persist to the database and send the WS event.

Example transformation for `send_direct_handler`:

```rust
async fn send_direct_handler(
    State(state): State<SharedState>,
    axum::Json(req): axum::Json<SendDirectRequest>,
) -> Response {
    if req.body.trim().is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "body cannot be empty");
    }

    // --- short lock: extract data ---
    let (keypair, relay_url, recipient_enc_key) = {
        let st = state.lock().await;
        let peer = match st.storage.get_peer(&req.recipient_id) {
            Ok(Some(p)) => p,
            Ok(None) => return api_error(StatusCode::NOT_FOUND, "recipient peer not found"),
            Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        };
        let enc_key = match peer.encryption_public_key {
            Some(k) => k,
            None => return api_error(StatusCode::BAD_REQUEST, "recipient has no encryption key"),
        };
        (st.keypair.clone(), st.relay_url.clone(), enc_key)
    };
    // lock is now released

    // --- build envelope (CPU + no I/O) ---
    let now = now_secs();
    let envelope = build_direct_envelope(/* ... */)?;

    // --- relay post (blocking I/O, no lock held) ---
    let relay_delivered = if let Some(ref url) = relay_url {
        post_to_relay(url, &envelope).is_ok()
    } else {
        false
    };

    // --- short lock: persist + broadcast ---
    {
        let st = state.lock().await;
        let _ = st.storage.insert_message(/* ... */);
        let _ = st.ws_tx.send(/* ... */);
    }

    // ... return response
}
```

Apply this pattern to `send_direct_handler`, `send_public_handler`,
`send_group_handler`, `create_group_handler`, and `add_group_member_handler`.

The read-only handlers (`list_messages_handler`, `list_peers_handler`, etc.)
already just do a DB query and return, so the lock duration is fine there.

---

### 5. Add integration tests for the web API endpoints

**Problem.** The storage layer has thorough unit tests (lines 1105–1521 of
`storage.rs`), but the 1 529-line web server has zero test coverage. None of
the API handlers, WebSocket logic, or background sync behaviour is tested.
The only way to verify the API works is to run the binary and make manual
requests.

**Why it matters.** Untested code is fragile code. The message-sending handlers
contain complex multi-step logic (validation, crypto, relay posting, DB writes,
WS broadcast). A regression in any step would go undetected. Axum provides
first-class test utilities that make this straightforward.

**Proposed change.**

Create `tests/web_api_tests.rs` with integration tests using Axum's test
client. The tests should cover:

1. **Health check**: `GET /api/health` returns 200 with expected fields.
2. **Message listing**: `GET /api/messages` with various filter combinations.
3. **Send direct message**: `POST /api/messages/direct` succeeds when peer
   exists with encryption key; returns 404 when peer not found; returns 400
   when body is empty.
4. **Send public message**: `POST /api/messages/public` creates a message
   and returns it in subsequent list calls.
5. **Peer CRUD**: add, list, get, delete peers.
6. **Group creation**: create a group, verify members, verify group appears
   in list.
7. **Conversations**: send a direct message and verify it appears in
   `GET /api/conversations` and `GET /api/conversations/:peer_id`.
8. **Mark as read**: `POST /api/messages/:id/read` flips `is_read`.

Test setup approach:

```rust
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt; // for oneshot()

fn build_test_app() -> Router {
    // Create in-memory storage
    let storage = Storage::open_in_memory().unwrap();
    // Insert a test identity
    let keypair = generate_keypair();
    storage.insert_identity(&IdentityRow::from(&keypair)).unwrap();

    let (ws_tx, _) = broadcast::channel(16);
    let state: SharedState = Arc::new(Mutex::new(AppState {
        storage,
        keypair,
        relay_url: None, // no relay in tests
        ws_tx,
    }));

    // Build the same router as main() but with test state
    Router::new()
        .route("/api/health", get(health_handler))
        // ... all routes ...
        .with_state(state)
}

#[tokio::test]
async fn test_health() {
    let app = build_test_app();
    let resp = app
        .oneshot(Request::get("/api/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}
```

This requires that the handler functions and the `AppState`/`SharedState`
types be accessible from outside the binary. The module extraction from
suggestion 3 would enable this. As a simpler intermediate step, the test
module can be placed inside `src/bin/tenet-web.rs` itself as a `#[cfg(test)]
mod tests { ... }` block, since test modules in binaries can access the
binary's private items.

---

### 6. Validate hex-encoded public keys on the add-peer endpoint

**Problem.** `add_peer_handler` (line 746) validates that `peer_id` and
`signing_public_key` are non-empty strings, but does not check that they are
valid hex-encoded keys of the correct length. Invalid key material is stored
in the database and only causes a cryptographic failure later when the user
tries to send a message, at which point the error message is a confusing
low-level crypto error rather than a clear validation error.

**Why it matters.** Validating inputs at the API boundary produces clear error
messages and prevents corrupt data from entering the database.

**Proposed change.** Add a helper that checks a hex string decodes to the
expected byte length (32 bytes for X25519/Ed25519 keys). Call it in
`add_peer_handler` before inserting the peer:

```rust
fn validate_hex_key(hex: &str, expected_len: usize, field_name: &str) -> Result<(), String> {
    let bytes = hex::decode(hex)
        .map_err(|_| format!("{field_name} is not valid hex"))?;
    if bytes.len() != expected_len {
        return Err(format!("{field_name} must be {expected_len} bytes, got {}", bytes.len()));
    }
    Ok(())
}
```

Return a 400 error with the validation message if it fails.

---

### 7. Replace `Arc<Mutex<AppState>>` with finer-grained sharing

**Problem.** The entire `AppState` (storage handle, keypair, relay URL, WS
sender) is behind a single `Arc<Mutex<…>>`. Every handler — even read-only
ones like `list_messages_handler` — must acquire this exclusive lock. This
means two simultaneous GET requests serialize against each other unnecessarily.

**Why it matters.** As the number of WebSocket clients or API consumers grows,
the single mutex becomes a bottleneck. The `ws_tx` (broadcast sender) is
already `Clone + Send` and does not need mutex protection. The keypair and
relay URL are read-only after startup.

**Proposed change.** Split the state:

```rust
struct AppState {
    storage: Arc<Mutex<Storage>>,  // only storage needs exclusion
    keypair: Arc<StoredKeypair>,   // immutable after startup
    relay_url: Option<String>,     // immutable after startup
    ws_tx: broadcast::Sender<WsEvent>,  // already thread-safe
}
```

With this, the keypair, relay URL, and WS sender can be accessed without any
lock, and only storage operations take the mutex. A future improvement would
be to use a connection pool or `RwLock` for the storage layer.

---

### 8. Improve the relay sync loop with backoff and correct message-kind inference

**Problem A — No backoff.** `relay_sync_loop` (line 1320) polls the relay
every 30 seconds regardless of whether the relay is reachable. If the relay is
down, it logs an error every 30 seconds indefinitely. There is no exponential
backoff.

**Problem B — Hardcoded message kind.** In `sync_once` (line 1430), all
messages received from the relay are stored with `message_kind: "direct"`,
regardless of their actual kind. Public messages, group messages, and meta
messages all get recorded as "direct" in the database. This means the timeline
filter does not work correctly for incoming messages.

**Proposed changes.**

For backoff: track consecutive failures and increase the sleep interval
(capped at e.g. 5 minutes). Reset on success.

For message kind: use the envelope's `header.message_kind` (available from
the raw envelope fetch already performed in the function) to set the correct
kind string. The relay sync function already fetches raw envelopes for meta
processing; the same data can be cross-referenced with `outcome.messages` by
matching on message ID or timestamp.

---

### 9. Avoid duplicating JSON serialization logic for row types

**Problem.** Every handler manually constructs `serde_json::json!({…})` objects
from row fields. For instance, the `PeerRow` serialization pattern appears
three times (lines 693–706, 721–730, 776–784), and the `MessageRow` pattern
appears four times (lines 308–322, 337–347, 1214–1227, plus conversation
enrichment). If a field is added to a row type, every handler that serializes
it must be updated in lockstep.

**Why it matters.** DRY (Don't Repeat Yourself) violations increase maintenance
cost and risk inconsistency.

**Proposed change.** Since the row types already derive `Serialize`, use Axum's
`Json(row)` directly, or define dedicated API response types that derive
`Serialize` and implement `From<PeerRow>`, `From<MessageRow>`, etc. Then each
handler returns `(StatusCode::OK, Json(ApiMessage::from(row)))`. This
eliminates the manual `json!({})` construction entirely.

---

### 10. Harden the WebSocket connection against slow or malicious clients

**Problem.** The WebSocket handler (line 1243) subscribes each client to the
broadcast channel with a fixed capacity of 256. If a client reads slowly, it
silently skips events (`RecvError::Lagged`). There is no limit on the number
of concurrent WebSocket connections, and there is no timeout for idle
connections.

**Why it matters.** A malicious or misbehaving client could open many
connections and consume server memory (each subscription holds a buffer). Slow
clients silently miss events with no indication to the user. In a local-only
deployment this is low risk, but it becomes relevant if the bind address is
changed to `0.0.0.0`.

**Proposed changes.**

- Track active WS connection count and reject new connections above a
  configurable limit (e.g. 8).
- When `RecvError::Lagged` occurs, send a JSON message to the client indicating
  events were missed, so the frontend can do a full reload.
- Add an idle timeout: if no events are sent or received for N minutes,
  close the connection. The frontend already reconnects automatically on
  close.

---

## Summary table

| Rank | Suggestion | Category | Effort |
|------|-----------|----------|--------|
| 1 | Extract relay-posting helper and report failures | Robustness | Low |
| 2 | Wrap group creation + key distribution in a transaction | Robustness / Security | Medium |
| 3 | Extract `tenet-web.rs` into modules | Understandability | Medium |
| 4 | Avoid holding Mutex across blocking I/O | Robustness | Medium |
| 5 | Add integration tests for web API | Robustness | Medium |
| 6 | Validate hex-encoded keys on add-peer | Security | Low |
| 7 | Replace single `Arc<Mutex>` with finer-grained sharing | Robustness | Medium |
| 8 | Relay sync: add backoff + fix message-kind inference | Robustness | Low–Medium |
| 9 | Derive JSON serialization instead of manual `json!({})` | Understandability | Low |
| 10 | Harden WebSocket against slow/malicious clients | Security | Low |
