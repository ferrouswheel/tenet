# Web Client Module (`src/web_client/`)

The `tenet-web` binary delegates to `tenet::web_client::run()`. This module provides a REST API + WebSocket server with an embedded SPA for interacting with the Tenet protocol.

## Module Files

| File | Purpose |
|------|---------|
| `mod.rs` | Entry point `run()`: CLI parsing, identity resolution, server startup |
| `config.rs` | `Cli` (clap), `Config`, constants (`DEFAULT_TTL_SECONDS`, etc.) |
| `state.rs` | `AppState`, `SharedState` (`Arc<Mutex<AppState>>`), `WsEvent` enum |
| `router.rs` | `build_router()` — all Axum route definitions |
| `sync.rs` | Background relay sync loop, envelope processing, online announcements |
| `static_files.rs` | Embedded SPA serving via `rust-embed` |
| `utils.rs` | `api_error()`, `message_to_json()`, `link_attachments()`, `now_secs()`, `SendAttachmentRef` |

## Route Handlers (`handlers/`)

| File | Endpoints |
|------|-----------|
| `health.rs` | `GET /api/health`, `POST /api/sync` (manual sync trigger) |
| `messages.rs` | Message CRUD + send direct/public/group, mark read |
| `peers.rs` | Peer CRUD + activity tracking, blocking, muting |
| `friends.rs` | Friend request lifecycle (send, list, accept, ignore, block) |
| `groups.rs` | Group CRUD + symmetric key distribution |
| `group_invites.rs` | Group invite lifecycle (list, send, accept, reject) |
| `attachments.rs` | Multipart upload + content-addressed download |
| `conversations.rs` | Direct message conversation listing |
| `profiles.rs` | Profile management + broadcasting to friends |
| `notifications.rs` | Notification listing and mark-read |
| `reactions.rs` | Upvote/downvote reactions |
| `replies.rs` | Threaded replies to public/group messages |
| `websocket.rs` | WebSocket upgrade + broadcast connection |

## Key Constants (`config.rs`)

- `DEFAULT_TTL_SECONDS` — 3600
- `SYNC_INTERVAL_SECS` — 30
- `DEFAULT_MESH_WINDOW_SECS` — 7200
- `MESH_QUERY_INTERVAL_SECS` — 600
- `WS_CHANNEL_CAPACITY` — 256
- `MAX_WS_CONNECTIONS` — 8
- `MAX_ATTACHMENT_SIZE` — 10 MB

## Key Types

**`AppState`** (in `state.rs`):
- `storage: Storage` — SQLite database
- `keypair: StoredKeypair` — server identity
- `relay_url: Option<String>` — relay server URL
- `relay_client: RelayClient` — HTTP relay communication
- `group_manager: GroupManager` — in-memory group state
- `ws_tx: broadcast::Sender<WsEvent>` — WebSocket broadcast channel

**`WsEvent`** enum — events broadcast to WebSocket clients:
`NewMessage`, `MessageRead`, `PeerAdded`, `PeerOnline`, `PeerOffline`,
`FriendRequestReceived`, `FriendRequestAccepted`, `Notification`,
`ProfileUpdated`, `GroupInviteReceived`, `GroupMemberJoined`, `RelayStatus`

## CLI Options (`config.rs`)

| Flag | Default | Purpose |
|------|---------|---------|
| `--bind / -b` | `127.0.0.1:3000` | HTTP bind address |
| `--data-dir / -d` | `~/.tenet` | Identity data directory |
| `--relay-url / -r` | — | Relay server URL |
| `--identity / -i` | — | Identity selection (short ID prefix) |
