# Web Client

The `tenet-web` binary is a full Tenet peer that serves a browser-based single-page application
(SPA) alongside a REST/WebSocket API. It holds its own keypair, connects to relays, and
participates in store-and-forward — a first-class participant in the Tenet network.

## Running

```bash
# Defaults: data dir ~/.tenet, listen on 127.0.0.1:3000
cargo run --bin tenet-web

# With configuration
TENET_HOME=~/.tenet \
TENET_WEB_BIND=127.0.0.1:3000 \
TENET_RELAY_URL=http://relay.example.com:8080 \
  cargo run --bin tenet-web
```

## Architecture

```
┌──────────────────────────────────────────────────┐
│                   Browser (SPA)                  │
│  ┌────────────┐  ┌──────────┐  ┌──────────────┐ │
│  │  Timeline   │  │  Friends │  │  Compose /   │ │
│  │  View       │  │  List    │  │  Groups      │ │
│  └─────┬──────┘  └────┬─────┘  └──────┬───────┘ │
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
│  │  tenet library (protocol, crypto, client)  │  │
│  ├────────────────────────────────────────────┤  │
│  │              SQLite (rusqlite)             │  │
│  └────────────────────────────────────────────┘  │
│                      │                           │
│              Relay connection(s)                 │
│              (HTTP via ureq)                     │
└──────────────────────────────────────────────────┘
```

**Key design choices:**

| Concern | Choice |
|---------|--------|
| HTTP framework | Axum |
| Asset embedding | `rust-embed` (compile-time) |
| Database | SQLite via `rusqlite` (bundled) |
| WebSocket | `axum::extract::ws` |
| Background sync | Tokio task (polls relay on timer) |

## Build Process

The web UI is built automatically during `cargo build` via `build.rs`:

1. Source files live in `web/src/`:
   - `index.html` — HTML template with `{{STYLES}}` and `{{SCRIPTS}}` placeholders
   - `styles.css` — all CSS
   - `app.js` — all JavaScript
2. The build script inlines CSS and JS into the template and writes `web/dist/index.html`.
3. `web/dist/index.html` is excluded from git. **Never edit it directly.**

Always edit files in `web/src/`; the built file is regenerated on the next `cargo build`.

## Background Sync

A Tokio background task runs a sync loop that:
- Polls `GET /inbox/{my_peer_id}` on the relay periodically.
- Decrypts and validates incoming envelopes.
- Writes messages, reactions, profiles, and friend state to SQLite.
- Broadcasts WebSocket events to connected browser clients.

The sync loop is implemented in `src/web_client/sync.rs`.

## REST API

### Messages

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/api/messages` | GET | Paginated message list (`?kind=`, `?before=`, `?limit=`) |
| `/api/messages/:id` | GET | Single message |
| `/api/messages/public` | POST | Send a public message `{ body }` |
| `/api/messages/direct` | POST | Send a direct message `{ recipient_id, body }` |
| `/api/messages/group` | POST | Send a group message `{ group_id, body }` |
| `/api/messages/:id/read` | POST | Mark message as read |
| `/api/messages/:id/reactions` | GET | Get reactions |
| `/api/messages/:id/react` | POST/DELETE | Add or remove reaction `{ reaction: "upvote"|"downvote" }` |
| `/api/messages/:id/replies` | GET | Get replies |
| `/api/messages/:id/reply` | POST | Post a reply `{ body }` |

### Peers

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/api/peers` | GET | List all peers |
| `/api/peers/:id` | GET | Peer detail |
| `/api/peers` | POST | Add peer `{ peer_id, display_name, signing_public_key }` |
| `/api/peers/:id` | DELETE | Remove peer |
| `/api/peers/:id/profile` | GET | View peer's profile |

### Conversations

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/api/conversations` | GET | List DM conversations |
| `/api/conversations/:peer_id` | GET | Messages with a specific peer |

### Friend Requests

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/api/friend-requests` | GET | List all requests |
| `/api/friend-requests` | POST | Send a request `{ peer_id, message? }` |
| `/api/friend-requests/:id/accept` | POST | Accept |
| `/api/friend-requests/:id/ignore` | POST | Ignore |
| `/api/friend-requests/:id/block` | POST | Block sender |

### Groups

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/api/groups` | GET | List groups |
| `/api/groups` | POST | Create group `{ group_id, member_ids, message? }` |
| `/api/groups/:id` | GET | Group detail + members + pending invites |
| `/api/groups/:id/members` | POST | Invite a member `{ peer_id }` |
| `/api/group-invites` | GET | List invites (`?status=pending`) |
| `/api/group-invites/:id/accept` | POST | Accept invite |
| `/api/group-invites/:id/ignore` | POST | Ignore invite |

### Profiles

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/api/profile` | GET | Own profile |
| `/api/profile` | PUT | Update own profile |

### Attachments

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/api/attachments` | POST | Upload file (multipart) |
| `/api/attachments/:hash` | GET | Download attachment |

### Notifications

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/api/notifications` | GET | List notifications (`?unread=true`) |
| `/api/notifications/count` | GET | Unread count |
| `/api/notifications/:id/read` | POST | Mark read |
| `/api/notifications/read-all` | POST | Mark all read |

### Health

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/api/health` | GET | Returns peer ID and relay connection status |
| `/api/ws` | WebSocket | Real-time event stream |

## WebSocket Events

The server pushes JSON events to connected browser clients:

| Event type | Payload | Description |
|------------|---------|-------------|
| `new_message` | message data | Message received or sent |
| `message_read` | `message_id` | Read status updated |
| `peer_online` | `peer_id` | Peer came online |
| `peer_offline` | `peer_id` | Peer went offline |
| `friend_request_received` | `from_peer_id` | New incoming friend request |
| `friend_request_accepted` | `from_peer_id` | Outgoing request accepted |
| `group_invite_received` | `invite_id, group_id, from_peer_id` | Group invite received |
| `group_member_joined` | `group_id, peer_id` | Member accepted a group invite |
| `notification` | `id, type, message_id, sender_id` | New notification |
| `relay_status` | `connected, relay_url` | Relay connectivity change |

## SPA Views

The SPA uses hash-based routing:

| Hash | View |
|------|------|
| `#/` or `#/timeline` | Public and group post feed |
| `#/post/{messageId}` | Single post with replies |
| `#/peer/{peerId}` | Direct message conversation |

## Database Schema

The web client and CLI share a single SQLite database at `{TENET_HOME}/tenet.db`.

Key tables:

| Table | Purpose |
|-------|---------|
| `identity` | Local keypair and peer ID |
| `peers` | Known peers/friends with online status |
| `messages` | All received and sent messages |
| `outbox` | Sent envelopes |
| `groups` | Group membership and symmetric keys |
| `group_members` | Active group membership |
| `group_invites` | Pending/accepted/ignored group invites |
| `friend_requests` | Friend request state machine |
| `attachments` | Content-addressed file storage |
| `message_attachments` | Message ↔ attachment join table |
| `reactions` | Upvote/downvote reactions |
| `profiles` | Peer profiles (public + friends-only fields) |
| `notifications` | Unread notification queue |

## Security Notes

- The web server binds to `127.0.0.1` by default (local-only access).
- No authentication on the API — single-user, local access assumed.
- Private key material is never exposed via the API.
- All relay communication uses HPKE + ChaCha20Poly1305 encryption.

## Feature Status

| Feature | Status |
|---------|--------|
| Core messaging + encryption | Implemented |
| Attachments (files/images) | Implemented |
| Reactions (upvote/downvote) | Implemented |
| Replies/threads | Implemented |
| Profiles | Implemented |
| Public message timeline | Implemented |
| Direct messages (per-friend) | Implemented |
| Friend management | Implemented |
| Group invite flow | Implemented |
| Real-time updates (WebSocket) | Implemented |
| Hash-based URL routing | Implemented |
| Friend Groups UI | Partial (protocol exists, basic filtering) |
