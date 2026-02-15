# Group Invite Flow — Design & Implementation Plan

## 1. Root Cause of the Bug

When a peer receives a `group_key_distribution` Direct message, `process_message_event` in
`src/web_client/sync.rs:379–381` silently discards it:

```rust
if msg_type == Some("group_key_distribution") {
    return false;  // dropped — group key never stored
}
```

Consequently the recipient's `GroupManager` stays empty.  On the _next_ sync, when a
`FriendGroup` envelope arrives for that group, the decryption call fails with:

```
unknown group: Batman
```

because `sync_once` only loads groups from storage, and none were ever written.

The fix is not simply to store the key on receipt — that would silently auto-join users to
groups they were never asked about.  Instead, the correct design is a full consent-based
**invite flow**: the group key is only distributed after the recipient explicitly accepts.

---

## 2. Target Behaviour (End-to-End Flow)

```
Creator                        Relay                       Recipient
  |                              |                              |
  | createGroup(id, [bob, ...])  |                              |
  |----------------------------->|                              |
  | -- GroupInvite meta msg --> bob                             |
  |                              |--- deliver to bob ---------->|
  |                              |                              | store GroupInviteRow
  |                              |                              | WsEvent::GroupInviteReceived
  |                              |                              | UI: notification + Accept/Ignore
  |                              |                              |
  |                              |   (bob clicks Accept)        |
  |                              |                              | POST /api/group-invites/:id/accept
  |                              |<-- GroupInviteAccept meta ---|
  |<-- deliver to creator -------|                              |
  | store acceptance                                            |
  | send group_key_distribution to bob                          |
  |----------------------------->|--- deliver to bob ---------->|
  |                              |                              | store GroupRow + GroupMemberRow
  |                              |                              | can now decrypt FriendGroup msgs
```

---

## 3. What Changes Where

### 3.1 Storage Layer (`src/storage.rs`)

#### New table: `group_invites`

Mirrors the `friend_requests` table structure.

```sql
CREATE TABLE IF NOT EXISTS group_invites (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    group_id        TEXT NOT NULL,
    from_peer_id    TEXT NOT NULL,
    to_peer_id      TEXT NOT NULL,
    status          TEXT NOT NULL DEFAULT 'pending',  -- pending | accepted | ignored
    message         TEXT,
    direction       TEXT NOT NULL,                    -- incoming | outgoing
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_group_invites_to
    ON group_invites(to_peer_id, status);
CREATE INDEX IF NOT EXISTS idx_group_invites_from
    ON group_invites(from_peer_id, status);
```

Added as a `CREATE TABLE IF NOT EXISTS` in `init_db()` (no migration guard needed for new
deployments; add an `IF NOT EXISTS` guard for existing DBs).

#### New struct

```rust
pub struct GroupInviteRow {
    pub id: i64,
    pub group_id: String,
    pub from_peer_id: String,
    pub to_peer_id: String,
    pub status: String,        // "pending" | "accepted" | "ignored"
    pub message: Option<String>,
    pub direction: String,     // "incoming" | "outgoing"
    pub created_at: u64,
    pub updated_at: u64,
}
```

#### New storage methods

```rust
pub fn insert_group_invite(&self, row: &GroupInviteRow) -> Result<i64, StorageError>;
pub fn get_group_invite(&self, id: i64) -> Result<Option<GroupInviteRow>, StorageError>;
pub fn list_group_invites(&self, status_filter: Option<&str>) -> Result<Vec<GroupInviteRow>, StorageError>;
pub fn update_group_invite_status(&self, id: i64, status: &str) -> Result<bool, StorageError>;
pub fn find_group_invite(&self, group_id: &str, from: &str, to: &str, direction: &str)
    -> Result<Option<GroupInviteRow>, StorageError>;
```

The `group_invites` table also needs a unique index on `(group_id, from_peer_id, to_peer_id,
direction)` to allow upsert/dedup.

---

### 3.2 Protocol Layer (`src/protocol.rs`)

Extend `MetaMessage` with two new variants:

```rust
pub enum MetaMessage {
    // ... existing variants ...

    /// Sent by a group creator to a prospective member.
    GroupInvite {
        peer_id: String,           // sender's own peer ID
        group_id: String,
        group_name: Option<String>, // human-readable name (same as group_id for now)
        message: Option<String>,   // optional invite note
    },

    /// Sent by an invitee back to the creator to accept.
    GroupInviteAccept {
        peer_id: String,           // accepter's own peer ID
        group_id: String,
    },
}
```

`MetaMessage` uses `#[serde(tag = "type", rename_all = "snake_case")]`, so these serialise to
`"group_invite"` and `"group_invite_accept"` automatically.

No changes to `MessageKind`, content types, or cryptography.

---

### 3.3 WebSocket Events (`src/web_client/state.rs`)

Add two new variants to `WsEvent`:

```rust
pub enum WsEvent {
    // ... existing ...
    GroupInviteReceived {
        invite_id: i64,
        group_id: String,
        from_peer_id: String,
        message: Option<String>,
        created_at: u64,
    },
    GroupMemberJoined {
        group_id: String,
        peer_id: String,
    },
}
```

The WebSocket broadcast handler (`handlers/websocket.rs`) must serialise these to JSON in the
same snake_case style as the other events.  The frontend already dispatches on `event.type`, so
new types flow through automatically.

---

### 3.4 Backend: Group Creation Handler (`handlers/groups.rs`)

Replace the current key-distribution block with invite sending:

**Before** (current, broken):
```rust
// Encrypt group_key_distribution → post via relay  ← sends key immediately
```

**After**:
```rust
for (member_id, enc_key) in &member_enc_keys {
    // Build MetaMessage::GroupInvite
    let invite_meta = MetaMessage::GroupInvite {
        peer_id: keypair_id.clone(),
        group_id: req.group_id.clone(),
        group_name: Some(req.group_id.clone()),
        message: req.message.clone(),   // add optional `message` field to CreateGroupRequest
    };
    let payload = build_meta_payload(&invite_meta)?;
    let envelope = build_envelope_from_payload(
        keypair_id.clone(), member_id.clone(),
        ..., MessageKind::Meta, payload, &signing_key,
    )?;
    post_envelope(&relay_url, &envelope)?;

    // Record outgoing invite in storage
    st.storage.insert_group_invite(&GroupInviteRow {
        id: 0,
        group_id: req.group_id.clone(),
        from_peer_id: keypair_id.clone(),
        to_peer_id: member_id.clone(),
        status: "pending".to_string(),
        message: req.message.clone(),
        direction: "outgoing".to_string(),
        created_at: now,
        updated_at: now,
    });
}
```

The `group_members` rows are **not** inserted for invited-but-not-yet-accepted members.
The creator is always inserted as an active member immediately (unchanged).

---

### 3.5 Backend: New Group Invite Handlers (`handlers/group_invites.rs`)

Three endpoints (all operate on the local identity's invite records):

#### `GET /api/group-invites`
Returns all incoming + outgoing invites (optionally filtered by `?status=pending`).

#### `POST /api/group-invites/:id/accept`

1. Look up `GroupInviteRow` by id; validate it is incoming + pending.
2. Update status to `"accepted"`.
3. Send `MetaMessage::GroupInviteAccept { peer_id, group_id }` to the inviter via relay.
4. Return `{ status: "accepted", group_id }`.

Note: the group key is **not** stored here — that happens when the creator receives the
acceptance and sends back the `group_key_distribution` message.

#### `POST /api/group-invites/:id/ignore`

1. Look up row; validate incoming + pending.
2. Update status to `"ignored"`.
3. Return `{ status: "ignored" }`.

---

### 3.6 Backend: Sync Handler (`src/web_client/sync.rs`)

#### Handle `GroupInvite` in `process_meta_event`

```rust
MetaMessage::GroupInvite {
    peer_id: from_peer_id,
    group_id,
    group_name,
    message,
} => {
    process_incoming_group_invite(state, from_peer_id, group_id, message, now).await;
}
```

```rust
async fn process_incoming_group_invite(
    state: &SharedState,
    from_peer_id: &str,
    group_id: &str,
    message: Option<String>,
    now: u64,
) {
    let st = state.lock().await;
    let my_id = st.keypair.id.clone();

    // Dedup: ignore if already have an invite for this (group_id, from, me)
    if st.storage.find_group_invite(group_id, from_peer_id, &my_id, "incoming")
        .unwrap_or(None).is_some() {
        return;
    }

    let invite = GroupInviteRow {
        id: 0,
        group_id: group_id.to_string(),
        from_peer_id: from_peer_id.to_string(),
        to_peer_id: my_id.clone(),
        status: "pending".to_string(),
        message: message.clone(),
        direction: "incoming".to_string(),
        created_at: now,
        updated_at: now,
    };

    if let Ok(invite_id) = st.storage.insert_group_invite(&invite) {
        // Notification
        let notif = NotificationRow {
            id: 0,
            notification_type: "group_invite".to_string(),
            message_id: format!("invite:{invite_id}"),
            sender_id: from_peer_id.to_string(),
            created_at: now,
            seen: false,
            read: false,
        };
        if let Ok(notif_id) = st.storage.insert_notification(&notif) {
            let _ = st.ws_tx.send(WsEvent::GroupInviteReceived {
                invite_id,
                group_id: group_id.to_string(),
                from_peer_id: from_peer_id.to_string(),
                message,
                created_at: now,
            });
            let _ = st.ws_tx.send(WsEvent::Notification {
                id: notif_id,
                notification_type: "group_invite".to_string(),
                message_id: format!("invite:{invite_id}"),
                sender_id: from_peer_id.to_string(),
                created_at: now,
            });
        }
    }
}
```

#### Handle `GroupInviteAccept` in `process_meta_event`

```rust
MetaMessage::GroupInviteAccept {
    peer_id: from_peer_id,
    group_id,
} => {
    process_group_invite_accept(state, &keypair, relay_url, from_peer_id, group_id, now).await;
}
```

```rust
async fn process_group_invite_accept(
    state: &SharedState,
    keypair: &StoredKeypair,
    relay_url: &str,
    from_peer_id: &str,
    group_id: &str,
    now: u64,
) {
    // Short lock to get group data and mark invite accepted
    let (group_key, key_version, recipient_enc_key) = {
        let st = state.lock().await;

        // Update outgoing invite status
        if let Ok(Some(invite)) = st.storage.find_group_invite(
            group_id, &keypair.id, from_peer_id, "outgoing"
        ) {
            let _ = st.storage.update_group_invite_status(invite.id, "accepted");
        }

        let group = match st.storage.get_group(group_id) {
            Ok(Some(g)) => g,
            _ => return,
        };

        let peer = match st.storage.get_peer(from_peer_id) {
            Ok(Some(p)) => p,
            _ => return,
        };

        let enc_key = match peer.encryption_public_key {
            Some(k) => k,
            None => return,
        };

        let gk: [u8; 32] = match group.group_key.try_into() {
            Ok(k) => k,
            Err(_) => return,
        };

        // Add as active group member
        let member = GroupMemberRow {
            group_id: group_id.to_string(),
            peer_id: from_peer_id.to_string(),
            joined_at: now,
        };
        let _ = st.storage.insert_group_member(&member);

        // Notify UI
        let _ = st.ws_tx.send(WsEvent::GroupMemberJoined {
            group_id: group_id.to_string(),
            peer_id: from_peer_id.to_string(),
        });

        (gk, group.key_version, enc_key)
    };
    // Lock released — now do crypto + I/O

    // Build and send group_key_distribution (existing logic from create_group_handler)
    let key_distribution = serde_json::json!({
        "type": "group_key_distribution",
        "group_id": group_id,
        "group_key": hex::encode(group_key),
        "key_version": key_version,
        "creator_id": keypair.id,
    });
    // ... encrypt as Direct message and post_envelope (same as current create_group_handler) ...
}
```

#### Handle `group_key_distribution` in `process_message_event`

Replace the current `return false` with:

```rust
if msg_type == Some("group_key_distribution") {
    drop(st);
    process_group_key_distribution(state, sender_id, &parsed, now).await;
    return false;
}
```

```rust
async fn process_group_key_distribution(
    state: &SharedState,
    sender_id: &str,
    payload: &serde_json::Value,
    now: u64,
) {
    let group_id = match payload.get("group_id").and_then(|v| v.as_str()) {
        Some(id) => id,
        None => return,
    };
    let group_key_hex = match payload.get("group_key").and_then(|v| v.as_str()) {
        Some(k) => k,
        None => return,
    };
    let key_version = payload.get("key_version")
        .and_then(|v| v.as_u64())
        .unwrap_or(1) as u32;
    let creator_id = payload.get("creator_id")
        .and_then(|v| v.as_str())
        .unwrap_or(sender_id)
        .to_string();

    let group_key = match hex::decode(group_key_hex) {
        Ok(k) if k.len() == 32 => k,
        _ => return,
    };

    let st = state.lock().await;

    // Only store if we have a pending invite for this group (consent check)
    let has_invite = st.storage
        .find_group_invite(group_id, sender_id, &st.keypair.id, "incoming")
        .unwrap_or(None)
        .map(|inv| inv.status == "accepted")
        .unwrap_or(false);

    // Also accept if we explicitly invited ourselves (shouldn't happen but be safe)
    if !has_invite {
        crate::tlog!(
            "sync: ignoring group_key_distribution for {} — no accepted invite",
            group_id
        );
        return;
    }

    let group_row = crate::storage::GroupRow {
        group_id: group_id.to_string(),
        group_key,
        creator_id,
        created_at: now,
        key_version,
    };

    if let Err(e) = st.storage.insert_group(&group_row) {
        crate::tlog!("sync: failed to store group key for {}: {}", group_id, e);
        return;
    }

    // Add ourselves as a member
    let member = crate::storage::GroupMemberRow {
        group_id: group_id.to_string(),
        peer_id: st.keypair.id.clone(),
        joined_at: now,
    };
    let _ = st.storage.insert_group_member(&member);

    crate::tlog!("sync: stored group key for {}", group_id);
}
```

---

### 3.7 REST API Routes (`src/web_client/router.rs`)

Add under group routes:

```rust
.route("/api/group-invites",              get(list_group_invites_handler))
.route("/api/group-invites/:id/accept",   post(accept_group_invite_handler))
.route("/api/group-invites/:id/ignore",   post(ignore_group_invite_handler))
```

The existing `GET /api/groups/:group_id` response should also include the list of members with
their invite status (active members from `group_members`, pending invites from `group_invites`).
The updated `get_group_handler` can join both tables:

```json
{
  "group_id": "Batman",
  "creator_id": "...",
  "members": [
    { "peer_id": "alice", "status": "active",  "joined_at": 12345 },
    { "peer_id": "bob",   "status": "invited", "invited_at": 12340 }
  ]
}
```

---

## 4. Frontend Changes (`web/src/app.js` + `index.html`)

### 4.1 Notification Panel

The existing notification panel already handles `notification_type` strings; add handling for
`"group_invite"` in `renderNotificationList()` and `handleWsEvent()`:

- Display: "**Alice** invited you to group **Batman**"
- Include **Accept** / **Ignore** buttons that call the new API endpoints
- On accept: reload groups + update compose group selector

### 4.2 WebSocket Event Handling

In the `handleWsEvent()` switch, handle:

```javascript
} else if (event.type === 'group_invite_received') {
    showToast('You were invited to group "' + event.group_id + '"');
    loadNotificationCount();
} else if (event.type === 'group_member_joined') {
    // Refresh group detail if we're viewing it
    if (currentGroupId === event.group_id) {
        loadGroupDetail(event.group_id);
    }
}
```

### 4.3 Group Member Management Panel

When in the Groups view, the existing **Manage Groups** button (or a per-group detail view)
should show, for each group the user owns:

- List of **active members** (status: active) with a Remove button
- List of **pending invitees** (status: pending from `group_invites`) labelled "Invited — awaiting acceptance"
- **+ Add Member** button → opens a picker from friends list → calls
  `POST /api/groups/:id/members`

The existing `get_group_handler` response is updated to include status (see §3.7 above).

### 4.4 Group Invites View

A new section in the Groups tab (or a "Pending" tab within groups):

- Shows all incoming group invites with status "pending"
- Each item: group name, inviter, optional message, **Accept** / **Ignore** buttons
- Data fetched from `GET /api/group-invites?status=pending`
- Refreshed on `group_invite_received` WsEvent

### 4.5 Notification Badge

The `count_unseen_notifications` query already covers all notification types; no change needed.
The notification panel item for `"group_invite"` needs a click handler that navigates to the
invite and marks it read.

---

## 5. Implementation Order

Work can proceed largely in sequence; each step is independently compilable and testable.

| Step | Component | What |
|------|-----------|------|
| 1 | `storage.rs` | Add `group_invites` table, `GroupInviteRow` struct, and CRUD methods |
| 2 | `protocol.rs` | Add `GroupInvite` and `GroupInviteAccept` to `MetaMessage` |
| 3 | `state.rs` | Add `GroupInviteReceived` and `GroupMemberJoined` to `WsEvent` |
| 4 | `sync.rs` | Add `process_incoming_group_invite`, `process_group_invite_accept`, `process_group_key_distribution`; wire into `process_meta_event` and `process_message_event` |
| 5 | `handlers/groups.rs` | Replace key-distribution with invite-sending in `create_group_handler`; update `get_group_handler` to include pending invitees; update `add_group_member_handler` similarly |
| 6 | `handlers/group_invites.rs` | New file: `list_group_invites_handler`, `accept_group_invite_handler`, `ignore_group_invite_handler` |
| 7 | `router.rs` | Wire new routes |
| 8 | `handlers/websocket.rs` | Serialise new WsEvent variants |
| 9 | `web/src/app.js` | Handle new WsEvent types; add invites view; group member management; notification panel entries |
| 10 | `web/src/index.html` | HTML for invite panel, member list |
| 11 | `web/src/styles.css` | Styles for invite list, member status badges |

---

## 6. Edge Cases & Open Questions

### Must Handle

- **Duplicate invites**: If the creator resends (e.g. retries after relay failure), the
  recipient should dedup via `find_group_invite`.
- **Key distribution without accepted invite**: The consent check in
  `process_group_key_distribution` guards against this, with a log warning.
- **Creator re-sends key after relay failure**: The `accept` endpoint should be idempotent;
  re-accepting a group the user already has the key for should be a no-op.
- **Group already exists locally when key arrives**: `insert_group` uses `INSERT OR REPLACE`
  (or check first) to handle re-key scenarios.

### Deferred (Future Work)

- **Key rotation on member removal**: The current code has a TODO comment; still deferred.
- **Inviting non-friends**: Currently requires the invitee to be in `peers`. Could add their
  public key as part of the invite meta message.
- **Group name vs group ID**: Today group_id doubles as the display name; a separate
  `group_name` field would improve UX but is not required for correctness.
- **Multi-device**: If a user accepts on one device, other devices will not automatically
  receive the group key. Out of scope for this change.
