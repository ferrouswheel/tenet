# Debugger Extension Plan

## Overview

Extend `src/bin/toy_ui.rs` (the `tenet-debugger` binary) to:

1. Use `Storage` per peer (backed by a temporary directory) so state persists within a session and exercises real persistence code paths.
2. Start an in-process relay on a random port so no external relay is needed.
3. Expose most `RelayClient` and `GroupManager` capabilities via REPL commands.

Make sure to rename `src/bin/toy_ui.rs` to `src/bin/debugger.rs`

---

## Current State

The debugger creates ephemeral `RelayClient` instances with no storage, and relies on an external relay (`--relay` flag or `TENET_RELAY_URL`). Commands are limited to:

```
peers | online | offline | send | sync | feed | exit
```

Gaps:
- No storage — messages lost between syncs; no access to `Storage` APIs.
- Requires external relay — makes standalone use awkward.
- No public messaging, group messaging, peer registration, or key inspection.

---

## Architecture Changes

### Temporary Data Directory

On startup, create a `tempfile::TempDir`. Under it, create one subdirectory per peer:

```
<tmpdir>/
  peer-1/
    tenet.db
    attachments/
  peer-2/
    tenet.db
    attachments/
  ...
```

Each peer gets a `Storage` instance opened from its subdirectory. The temp dir is owned by the process and deleted on exit (via `TempDir` RAII).

### In-Process Relay

Spawn a `tenet-relay` HTTP server (using `axum`) on a random port (bind to `127.0.0.1:0`) in a background Tokio thread. The relay binds and reports its actual port. All `RelayClient` instances point to `http://127.0.0.1:<port>`. No external relay is needed.

This keeps the debugger fully self-contained while exercising the real HTTP relay code path.

### `DebugPeer` struct

Replace the current `ToyPeer` with a richer struct:

```rust
struct DebugPeer {
    name: String,
    client: RelayClient,
    storage: Storage,        // sqlite-backed, in temp dir
    data_dir: PathBuf,       // peer's temp subdir
}
```

`RelayClient` already holds in-memory peer registry and group manager; `Storage` persists received messages, peers, groups, and outbox.

After each `sync_inbox`, store received messages via `Storage::insert_message()`. After each `send_*`, write to the outbox via `Storage::insert_outbox_message()`.

### Cargo dependency additions

```toml
[dependencies]
tempfile = "3"
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }  # already present for relay
```

---

## New Commands

### Peer Management

| Command | Description |
|---------|-------------|
| `peers` | List all peers (name, short id, online status) — existing, unchanged |
| `keys <peer>` | Show peer's signing public key and encryption public key (hex) |
| `add-peer <peer> <target>` | Register `target` in `peer`'s peer registry so they can exchange messages |
| `add-all-peers` | Register every peer with every other peer (convenience for demos) |
| `remove-peer <peer> <target>` | Unregister `target` from `peer`'s registry |

### Messaging

| Command | Description |
|---------|-------------|
| `send <from> <to> <msg>` | Send encrypted direct message — existing, improved to write to storage |
| `broadcast <peer> <msg>` | Send a public message from `peer` (calls `send_public_message`) |
| `forward <peer> <from>` | Forward a public message in `peer`'s cache that was originally from `from` |

### Sync

| Command | Description |
|---------|-------------|
| `sync <peer\|all> [limit]` | Sync inbox from relay — existing, improved to persist to storage |

### Feeds & Storage

| Command | Description |
|---------|-------------|
| `feed <peer>` | Show in-memory feed (direct + direct group messages) — existing |
| `public-feed <peer>` | Show public message cache for peer |
| `messages <peer> [--kind direct\|public\|group] [--limit N]` | Query stored messages from SQLite |
| `conversations <peer>` | Show direct message conversation list from storage |

### Groups

| Command | Description |
|---------|-------------|
| `create-group <peer> <group-name> [<member1> <member2> ...]` | Create a group owned by `peer`; optionally add members immediately |
| `groups <peer>` | List groups that `peer` is a member of |
| `group-info <peer> <group-id>` | Show group members and key version |
| `send-group <peer> <group-id> <msg>` | Send an encrypted group message |
| `add-member <peer> <group-id> <new-member>` | Add a peer to an existing group |

### Relay

| Command | Description |
|---------|-------------|
| `relay-status` | Show relay port, number of queued envelopes per inbox, and total message count |

### Inspection

| Command | Description |
|---------|-------------|
| `inspect <peer>` | Show full peer state: id, keys, online status, feed size, public cache size, group count, storage message count |

---

## Implementation Phases

### Phase 1 — Temp dir + in-process relay

1. Add `tempfile` to `Cargo.toml` dev-dependencies (or normal dependencies).
2. In `run()`, create a `TempDir` and a Tokio runtime.
3. Start the relay in the background:
   ```rust
   let relay_addr = start_in_process_relay(&rt);  // returns SocketAddr
   let relay_url = format!("http://{relay_addr}");
   ```
4. Per peer: create `{tmpdir}/{name}/` subdirectory, open `Storage::open(path)`, and generate a keypair for the identity.
5. Replace `spawn_peers()` to return `Vec<DebugPeer>` with `storage` field.

### Phase 2 — Storage integration

1. After `sync_inbox`, iterate `SyncOutcome::messages` and call `storage.insert_message()` for each.
2. After `send_message`, call `storage.insert_outbox_message()`.
3. After `send_public_message` / `send_group_message`, write to outbox.

### Phase 3 — Peer registry commands

Add `keys`, `add-peer`, `add-all-peers`, `remove-peer`.

A common pattern: `add-peer alice bob` looks up Bob's id and public key from the `DebugPeer` list and calls `alice.client.add_peer_with_encryption(...)`.

### Phase 4 — Public messaging

Add `broadcast` and `public-feed`. Receiving public messages during sync already happens via `RelayClient::receive_public_message`; surface the cache with `public_message_cache()`.

### Phase 5 — Group messaging

Add `create-group`, `groups`, `group-info`, `send-group`, `add-member`.

Group key material lives in `GroupManager` (in `RelayClient`). Members need to know the group's symmetric key; distribute it by sending a `Meta` envelope to each member (same approach as the web client).

### Phase 6 — Storage queries

Add `messages` and `conversations` backed by `Storage::list_messages()` and `Storage::list_conversations()`.

### Phase 7 — Relay inspection + inspect

Add `relay-status` (calls relay's `GET /debug/status` or queries in-process state directly) and `inspect`.

---

## Open Questions / Decisions

1. **Relay implementation**: The cleanest approach is to call `tenet::relay::start_relay(addr)` (or equivalent) in a background thread. Check whether `relay.rs` exposes a public `start()` function or if one needs to be added.

2. **Tokio runtime**: The current debugger is synchronous. The relay requires async. Options:
   - Wrap the whole main loop in `tokio::main` and use `spawn_blocking` for readline.
   - Spin up a dedicated Tokio runtime just for the relay and keep main loop sync.
   - Preferred: dedicated background runtime for the relay; keep REPL synchronous.

3. **Group key distribution**: When creating a group with initial members, the debugger should automatically send `Meta` envelopes containing the group key to each member, mirroring the web client's `POST /api/groups` flow.

4. **Storage for sent messages**: `RelayClient` doesn't currently write to `Storage` — that's the web client's responsibility. The debugger will need to call storage methods explicitly after each send/sync. Consider whether to add a `Storage` parameter to `RelayClient` in a future refactor, or keep the explicit calls in the debugger.

---

## Files to Modify

| File | Change |
|------|--------|
| `src/bin/toy_ui.rs` | Main implementation — all phases |
| `Cargo.toml` | Add `tempfile` dependency |
| `src/relay.rs` | Possibly expose a `start_relay(addr: SocketAddr)` public function if not already available |

No new source files are required; all changes are self-contained in the binary.
