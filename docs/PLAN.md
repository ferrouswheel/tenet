# Tenet Web Application — Implementation Plan

This document describes the phased plan for building a web application that acts as a
full peer on the tenet network. The web app consists of a Rust server (using the tenet
library) that serves both an HTTP/WebSocket API and a bundled single-page application.

## Design Goals

- **Single binary distribution**: the SPA's static assets (HTML, CSS, JS) are embedded
  into the Rust executable at compile time, so the binary is self-contained.
- **Full peer**: the server is a first-class tenet peer — it holds its own keypair,
  connects to relays, sends and receives messages, and participates in store-and-forward.
- **Shared storage with CLI**: state is persisted in a SQLite database that both the web
  server and the existing CLI tool (`tenet`) can read and write, so a user can switch
  between interfaces without data loss.
- **Real-time updates**: WebSocket connections push new messages and presence changes to
  the browser immediately rather than requiring polling.

---

## Architecture Overview

```
┌──────────────────────────────────────────────────┐
│                   Browser (SPA)                  │
│  ┌────────────┐  ┌──────────┐  ┌──────────────┐ │
│  │  Timeline   │  │  Friends │  │  Compose /   │ │
│  │  View       │  │  List    │  │  Groups      │ │
│  └─────┬──────┘  └────┬─────┘  └──────┬───────┘ │
│        │              │               │          │
│        └──────────┬───┘───────────────┘          │
│              WebSocket + REST API                │
└──────────────────┬───────────────────────────────┘
                   │ HTTP / WS
┌──────────────────┴───────────────────────────────┐
│              tenet-web (Rust binary)             │
│  ┌────────────────────────────────────────────┐  │
│  │         Axum HTTP + WebSocket server       │  │
│  ├────────────────────────────────────────────┤  │
│  │  API layer  │  WS hub  │  Static assets    │  │
│  ├─────────────┴──────────┴───────────────────┤  │
│  │         Application / domain logic         │  │
│  ├────────────────────────────────────────────┤  │
│  │  tenet library (protocol, crypto, client)  │  │
│  ├────────────────────────────────────────────┤  │
│  │          SQLite (rusqlite / sqlx)          │  │
│  └────────────────────────────────────────────┘  │
│                      │                           │
│              Relay connection(s)                 │
│              (HTTP via ureq/reqwest)             │
└──────────────────────────────────────────────────┘
```

**Key technology choices:**

| Concern | Choice | Rationale |
|---------|--------|-----------|
| HTTP framework | Axum (already a dependency) | Consistent with relay server; mature WebSocket support |
| Asset embedding | `rust-embed` or `include_dir` | Compile-time embedding; no runtime file dependencies |
| Database | SQLite via `rusqlite` (with `bundled` feature) | Zero-config, single-file, embeddable; widely supported |
| SPA framework | Vanilla JS or lightweight (e.g., Preact, Svelte) | Small bundle size matters for embedding |
| WebSocket | `axum::extract::ws` | Built-in with Axum; tokio-native |
| Background sync | Tokio task | Polls relay(s) on a timer; pushes to WS hub |

---

## Shared SQLite Database

The CLI and web server share a single SQLite database at
`{TENET_HOME}/tenet.db`. The existing CLI tool's JSON/JSONL files are
migrated on first run.

### Schema (initial)

```sql
-- Identity and key material
CREATE TABLE identity (
    id          TEXT PRIMARY KEY,  -- user id (SHA256 of public key)
    public_key  TEXT NOT NULL,     -- X25519 hex
    private_key TEXT NOT NULL,     -- X25519 hex (encrypted at rest later)
    signing_pub TEXT NOT NULL,     -- Ed25519 hex
    signing_priv TEXT NOT NULL,    -- Ed25519 hex
    created_at  INTEGER NOT NULL
);

-- Known peers / friends
CREATE TABLE peers (
    peer_id             TEXT PRIMARY KEY,
    display_name        TEXT,
    signing_public_key  TEXT NOT NULL,
    encryption_public_key TEXT,
    added_at            INTEGER NOT NULL,
    is_friend           INTEGER NOT NULL DEFAULT 1,
    last_seen_online    INTEGER,          -- unix timestamp
    online              INTEGER NOT NULL DEFAULT 0
);

-- Messages (all kinds)
CREATE TABLE messages (
    message_id      TEXT PRIMARY KEY,     -- ContentId
    sender_id       TEXT NOT NULL,
    recipient_id    TEXT NOT NULL,         -- peer id, group id, or "*"
    message_kind    TEXT NOT NULL,         -- public, direct, friend_group, meta
    group_id        TEXT,
    body            TEXT,                  -- decrypted plaintext
    timestamp       INTEGER NOT NULL,
    received_at     INTEGER NOT NULL,
    ttl_seconds     INTEGER NOT NULL,
    is_read         INTEGER NOT NULL DEFAULT 0,
    raw_envelope    TEXT                   -- original JSON for forwarding
);

CREATE INDEX idx_messages_recipient ON messages(recipient_id, timestamp);
CREATE INDEX idx_messages_sender ON messages(sender_id, timestamp);
CREATE INDEX idx_messages_kind ON messages(message_kind, timestamp);
CREATE INDEX idx_messages_group ON messages(group_id, timestamp);

-- Groups
CREATE TABLE groups (
    group_id    TEXT PRIMARY KEY,
    group_key   BLOB NOT NULL,            -- 32-byte symmetric key
    creator_id  TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    key_version INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE group_members (
    group_id    TEXT NOT NULL REFERENCES groups(group_id),
    peer_id     TEXT NOT NULL,
    joined_at   INTEGER NOT NULL,
    PRIMARY KEY (group_id, peer_id)
);

-- Sent messages (outbox)
CREATE TABLE outbox (
    message_id  TEXT PRIMARY KEY,
    envelope    TEXT NOT NULL,             -- serialized Envelope JSON
    sent_at     INTEGER NOT NULL,
    delivered   INTEGER NOT NULL DEFAULT 0
);

-- Relay configuration
CREATE TABLE relays (
    url         TEXT PRIMARY KEY,
    added_at    INTEGER NOT NULL,
    last_sync   INTEGER,
    enabled     INTEGER NOT NULL DEFAULT 1
);
```

### Migration from JSON/JSONL

On startup, if `tenet.db` does not exist but `identity.json` does, the server
runs a one-time migration:

1. Read `identity.json` -> insert into `identity` table.
2. Read `peers.json` -> insert into `peers` table.
3. Read `inbox.jsonl` -> parse each envelope, decrypt, insert into `messages`.
4. Read `outbox.jsonl` -> insert into `outbox`.
5. Rename old files to `*.migrated` as backup.

After migration, both the CLI and web server use SQLite exclusively. The CLI
tool gains a `--db` flag (or respects `TENET_DB` env var) to use the shared
database instead of JSON files.

---

## Phase 1 — Foundation ✅ COMPLETE

**Goal**: A working web server binary that serves a minimal SPA, connects to
the tenet network as a peer, and persists state in SQLite.

### 1.1 SQLite storage layer

- Add `rusqlite` (with `bundled` feature) to `Cargo.toml`.
- Create `src/storage.rs` module with:
  - Schema creation / migration logic.
  - CRUD operations for identity, peers, messages, groups, outbox, relays.
  - One-time JSON-to-SQLite migration.
- Ensure the storage layer is usable from both the web server and CLI.
- Unit tests for all storage operations.

### 1.2 Web server binary scaffold

- Create `src/bin/tenet-web.rs` as a new binary target.
- Axum server with:
  - Static asset serving (placeholder `index.html` for now).
  - Health check endpoint (`GET /api/health`).
  - Startup: load or generate identity from SQLite, configure relay URL(s).
- Configuration via environment variables and/or CLI flags:
  - `TENET_HOME` — data directory (default `~/.tenet`).
  - `TENET_WEB_BIND` — listen address (default `127.0.0.1:3000`).
  - `TENET_RELAY_URL` — relay to connect to.

### 1.3 Background relay sync

- Tokio task that periodically polls the relay for new messages.
- Decrypt incoming envelopes using the local identity keypair.
- Persist decrypted messages to SQLite.
- Process `Meta` messages (online announcements, ACKs) to update peer presence.

### 1.4 Minimal SPA shell

- Create `web/` directory for frontend source.
- Build tooling (e.g., `npm` + bundler, or hand-rolled) that produces
  `dist/` output.
- Use `rust-embed` to embed `web/dist/` into the binary at compile time.
- Serve embedded assets at `/` with correct MIME types.
- Minimal HTML page that confirms connectivity to the API.

---

## Phase 2 — Core API & Timeline ✅ COMPLETE

**Goal**: REST API endpoints for reading and composing messages, plus a
timeline view in the SPA.

### 2.1 Message API

```
GET    /api/messages?kind=...&group=...&before=...&limit=...
GET    /api/messages/:message_id
POST   /api/messages/direct    { recipient_id, body }
POST   /api/messages/public    { body }
POST   /api/messages/group     { group_id, body }
```

- Query parameters support filtering by `message_kind`, `group_id`, pagination
  via `before` (timestamp cursor) and `limit`.
- POST endpoints encrypt, sign, build envelopes, post to relay, and persist
  to SQLite.
- Return the created message with its `message_id`.

### 2.2 WebSocket hub

- `GET /api/ws` upgrades to a WebSocket connection.
- Server-side hub (broadcast channel) fans out events to all connected clients.
- Event types pushed from server to client:
  - `new_message` — a message was received or sent.
  - `peer_online` / `peer_offline` — presence change.
  - `message_read` — read status update.
- JSON-framed messages over the WebSocket.

### 2.3 Timeline UI

- SPA view showing a reverse-chronological feed of messages.
- Visual differentiation by message kind:
  - **Public** — open/globe icon, distinct background color.
  - **Direct** — lock icon, private styling.
  - **FriendGroup** — group icon, group name badge.
  - **Meta** — subtle/muted system message styling.
- Messages update in real time via WebSocket.
- Infinite scroll / "load more" pagination.

---

## Phase 3 — Peers & Presence

**Goal**: Friend/peer management UI and real-time online status.

### 3.1 Peer API

```
GET    /api/peers                         -- list all peers
GET    /api/peers/:peer_id               -- peer detail
POST   /api/peers                         -- add peer { peer_id, display_name, signing_public_key }
DELETE /api/peers/:peer_id               -- remove peer
```

- Adding a peer registers them in the `PeerRegistry` and SQLite.
- Removing a peer deletes from both and cleans up related state.
- Peer detail includes online status and `last_seen_online`.

### 3.2 Presence tracking

- Background task processes `MetaMessage::Online` and `MetaMessage::Ack`
  messages to update `peers.online` and `peers.last_seen_online` in SQLite.
- When presence changes, push `peer_online` / `peer_offline` over WebSocket.
- On startup, send `MetaMessage::Online` to known peers via relay.

### 3.3 Friends list UI

- Sidebar or dedicated view listing all peers.
- Each entry shows:
  - Display name (or peer ID if no name set).
  - Online/offline indicator (green dot / grey dot).
  - "Last seen" relative timestamp (e.g., "3 hours ago").
- Click a peer to view their profile or start a direct message.
- "Add friend" form: enter peer ID and public key (or scan/paste a share link).
- "Remove friend" confirmation dialog.

---

## Phase 4 — Groups

**Goal**: Group creation, membership management, and group messaging UI.

### 4.1 Group API

```
GET    /api/groups                        -- list groups
GET    /api/groups/:group_id             -- group detail + members
POST   /api/groups                        -- create group { group_id, member_ids }
POST   /api/groups/:group_id/join        -- join (if invited)
POST   /api/groups/:group_id/leave       -- leave group
POST   /api/groups/:group_id/members     -- add member { peer_id }
DELETE /api/groups/:group_id/members/:peer_id -- remove member
```

- Group creation generates a symmetric key via `GroupManager::create_group`.
- Key distribution to members happens via encrypted direct messages
  (the group key is sent in a `Direct` envelope to each member).
- Leaving a group triggers key rotation for remaining members.

### 4.2 Group messaging UI

- Group conversation view with message history.
- Compose box for posting to the group.
- Member list sidebar showing who is in the group and their online status.
- Group management: view members, invite new members, leave group.

---

## Phase 5 — Direct Messaging

**Goal**: Dedicated direct message conversation view.

### 5.1 Conversation API

```
GET    /api/conversations                 -- list DM conversations (grouped by peer)
GET    /api/conversations/:peer_id       -- messages with a specific peer
```

- Conversations aggregate `Direct` messages between the local user and a peer.
- Returns messages in chronological order with pagination.

### 5.2 DM UI

- Conversation list view showing recent DM threads.
- Each thread shows the peer name, last message preview, and unread count.
- Conversation detail view with message history and compose box.
- Messages sent via the existing `POST /api/messages/direct` endpoint.

---

## Phase 6 — Public Posting

**Goal**: Compose and view public posts with a clear distinct presentation.

### 6.1 Public feed view

- Filtered timeline showing only `Public` messages.
- Posts display sender identity, timestamp, and message body.
- Compose box for creating new public posts.

### 6.2 Public post detail

- Clicking a post opens a detail view (will later support replies and reactions).

---

## Phase 7 — Attachments

**Goal**: Support images and other file attachments on messages.

### 7.1 Attachment storage

- Extend SQLite schema:

```sql
CREATE TABLE attachments (
    content_hash    TEXT PRIMARY KEY,  -- SHA256 of content
    content_type    TEXT NOT NULL,     -- MIME type
    size_bytes      INTEGER NOT NULL,
    data            BLOB NOT NULL,
    created_at      INTEGER NOT NULL
);

CREATE TABLE message_attachments (
    message_id      TEXT NOT NULL REFERENCES messages(message_id),
    content_hash    TEXT NOT NULL REFERENCES attachments(content_hash),
    filename        TEXT,
    position        INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (message_id, content_hash)
);
```

- Attachments are content-addressed (stored by SHA256 hash, deduplicated).
- Size limits enforced per-attachment and per-message.

### 7.2 Attachment API

```
POST   /api/attachments                   -- upload file, returns content_hash
GET    /api/attachments/:content_hash    -- download attachment
```

- Upload returns the content hash; the client includes it when composing a
  message.
- The existing `AttachmentRef` in the protocol's `Payload` type is used to
  reference attachments in envelopes.
- Attachments are encrypted alongside the message payload before transmission.

### 7.3 Attachment UI

- Image attachments render inline in the timeline and conversation views.
- Non-image attachments show as downloadable links with filename and size.
- Compose views gain a file picker / drag-and-drop zone.
- Image previews shown before sending.

---

## Phase 8 — Reactions (Upvote / Downvote)

**Goal**: Allow reacting to public posts with upvotes and downvotes.

### 8.1 Reaction protocol

- Introduce a new payload content type: `application/json;type=tenet.reaction`.
- Reaction payload:

```json
{
  "target_message_id": "<ContentId>",
  "reaction": "upvote | downvote",
  "timestamp": 1234567890
}
```

- Reactions are sent as `Public` messages (for public posts) so they propagate
  through the network like any other public message.
- Each peer can have at most one reaction per target message (last one wins).

### 8.2 Reaction storage

```sql
CREATE TABLE reactions (
    message_id      TEXT NOT NULL,     -- the reaction message's own ID
    target_id       TEXT NOT NULL,     -- the post being reacted to
    sender_id       TEXT NOT NULL,
    reaction        TEXT NOT NULL,     -- 'upvote' or 'downvote'
    timestamp       INTEGER NOT NULL,
    PRIMARY KEY (target_id, sender_id)
);
```

### 8.3 Reaction API & UI

```
POST   /api/messages/:message_id/react   { reaction: "upvote" | "downvote" }
DELETE /api/messages/:message_id/react   -- remove reaction
GET    /api/messages/:message_id/reactions
```

- Timeline and post detail views show aggregated vote counts.
- Upvote/downvote buttons on each public post.
- User's own reaction state is highlighted.

---

## Phase 9 — Replies (Threads)

**Goal**: Support threaded replies on public and group posts.

### 9.1 Reply protocol

- Extend the message payload with an optional `reply_to` field:

```json
{
  "body": "reply text",
  "reply_to": "<ContentId of parent message>"
}
```

- Replies inherit the `message_kind` of the parent (Public replies to Public
  posts, FriendGroup replies to group posts).

### 9.2 Reply storage

- Add column to `messages` table:

```sql
ALTER TABLE messages ADD COLUMN reply_to TEXT;
CREATE INDEX idx_messages_reply_to ON messages(reply_to);
```

### 9.3 Reply API & UI

```
GET    /api/messages/:message_id/replies
POST   /api/messages/:message_id/reply   { body }
```

- Post detail view shows a threaded reply list beneath the original post.
- Reply compose box in post detail view.
- Reply counts shown on posts in the timeline.
- Clicking a reply navigates to the parent post's thread view.

---

## Phase 10 — User Profiles

**Goal**: Editable user profiles with separate public and friends-only variants.

### 10.1 Profile storage

```sql
CREATE TABLE profiles (
    user_id         TEXT PRIMARY KEY,
    display_name    TEXT,
    bio             TEXT,
    avatar_hash     TEXT,             -- references attachments table
    public_fields   TEXT NOT NULL,    -- JSON: fields visible to everyone
    friends_fields  TEXT NOT NULL,    -- JSON: fields visible only to friends
    updated_at      INTEGER NOT NULL
);
```

- `public_fields` and `friends_fields` are JSON objects with user-defined
  key-value pairs (e.g., location, interests, links).
- The local user's profile is stored locally and broadcast to peers.

### 10.2 Profile protocol

- Profile updates are sent as a structured payload type:
  `application/json;type=tenet.profile`.
- **Public profile**: sent as a `Public` message so anyone can see it.
- **Friends-only profile**: sent as `Direct` messages to each friend, encrypted
  per-recipient. This ensures only friends can see the friends-only fields.
- On receiving a profile message, update the local `profiles` table.
- Profiles are versioned by `updated_at` timestamp; only newer updates apply.

### 10.3 Profile API

```
GET    /api/profile                       -- own profile
PUT    /api/profile                       -- update own profile
GET    /api/peers/:peer_id/profile       -- view peer's profile
```

- When viewing a peer's profile, the server returns the public profile by
  default. If the peer is a friend, the friends-only fields are merged in.
- Profile updates trigger re-broadcast to peers.

### 10.4 Profile UI

- Profile edit page with sections for public and friends-only fields.
- Avatar upload (uses attachment system from Phase 7).
- Profile view accessible from:
  - Clicking a peer in the friends list.
  - Clicking a message sender's name/avatar in the timeline.
  - Clicking a reply author.
- Visual distinction between public and friends-only profile sections.

---

## Phase 11 — Notifications

**Goal**: In-app notification system for unread messages and replies.

### 11.1 Notification storage

```sql
CREATE TABLE notifications (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    type            TEXT NOT NULL,     -- 'direct_message', 'reply', 'reaction'
    message_id      TEXT NOT NULL,
    sender_id       TEXT NOT NULL,
    created_at      INTEGER NOT NULL,
    read            INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX idx_notifications_unread ON notifications(read, created_at);
```

### 11.2 Notification API

```
GET    /api/notifications?unread=true&limit=...
POST   /api/notifications/:id/read
POST   /api/notifications/read-all
GET    /api/notifications/count           -- { unread: N }
```

### 11.3 Notification generation

Notifications are created automatically when:

- A `Direct` message is received -> `direct_message` notification.
- A reply to one of the user's posts is received -> `reply` notification.
- A reaction to one of the user's posts is received -> `reaction` notification.

### 11.4 Notification UI

- Notification bell icon in the header with unread count badge.
- Dropdown or panel listing recent notifications.
- Each notification links to the relevant message/conversation.
- Unread direct messages show a badge on the conversation list and friend entry.
- "Mark all as read" action.
- WebSocket event `notification` pushes new notifications in real time.

---

## Cross-cutting Concerns

### Security

- The web server binds to `127.0.0.1` by default (local-only access).
- No authentication on the API in the initial phases — the server is
  single-user and assumed to be accessed only from the local machine.
- Future: optional authentication (passphrase, browser session cookie) for
  remote access or multi-user setups.
- Private key material is never exposed via the API.
- All relay communication uses the existing tenet encryption (HPKE +
  ChaCha20Poly1305).

### Error handling

- API errors return structured JSON: `{ "error": "message", "code": "..." }`.
- Use the existing error enum pattern from the tenet codebase.
- New error types: `StorageError`, `ApiError`.

### Testing

- Storage layer: unit tests against an in-memory SQLite database.
- API endpoints: integration tests using Axum's test utilities.
- WebSocket: integration tests for event delivery.
- Frontend: basic smoke tests (can be expanded later).

### Build process

- `cargo build --bin tenet-web` produces the self-contained binary.
- A build script or `Makefile` step runs the frontend build before
  `cargo build` so that `rust-embed` picks up the latest assets.
- CI runs both frontend and backend builds and tests.

---

## Phase Summary

| Phase | Scope | Depends on |
|-------|-------|------------|
| 1 | Foundation: SQLite, binary, relay sync, SPA shell | — |
| 2 | Core API, WebSocket hub, timeline UI | 1 |
| 3 | Peer management, presence, friends list UI | 1, 2 |
| 4 | Groups: API, key management, group chat UI | 1, 2, 3 |
| 5 | Direct messaging: conversations UI | 1, 2, 3 |
| 6 | Public posting: feed view, compose | 1, 2 |
| 7 | Attachments: storage, upload/download, inline display | 1, 2 |
| 8 | Reactions: upvote/downvote on public posts | 6, 7 (optional) |
| 9 | Replies: threaded discussions | 2, 6 |
| 10 | User profiles: public + friends-only, edit, view | 3, 7 |
| 11 | Notifications: unread badges, real-time alerts | 2, 5, 9 |

Phases 1-6 form the core experience. Phases 7-11 add richness and can be
developed in parallel once the foundation is stable.
