# Tenet Debugger

The `tenet-debugger` binary is an interactive REPL for exploring and testing the Tenet protocol
in a fully self-contained environment. It:

- Spawns N in-process peers, each with their own SQLite-backed `Storage`.
- Starts an in-process relay on a random port — no external relay needed.
- Exposes the full `RelayClient` and `GroupManager` APIs via a readline REPL.

## Running

```bash
# Default: 3 peers, in-process relay
cargo run --bin tenet-debugger

# Custom peer count
cargo run --bin tenet-debugger -- --peers 5

# Use an external relay instead of the in-process one
cargo run --bin tenet-debugger -- --relay http://127.0.0.1:8080
# or via env var
TENET_RELAY_URL=http://127.0.0.1:8080 cargo run --bin tenet-debugger
```

## Architecture

### In-process relay

On startup the debugger binds `127.0.0.1:0` (random port) and runs the real Axum relay server
in a background Tokio runtime. All peers point their `RelayClient` at this URL. This exercises
the real HTTP relay code path with no external dependency.

### Storage per peer

Each peer gets a `tempfile::TempDir` subdirectory:

```
<tmpdir>/
  peer-1/
    tenet.db      ← SQLite database
    attachments/
  peer-2/
    tenet.db
    attachments/
  ...
```

The `TempDir` is owned by the process and deleted on exit.

### `DebugPeer`

```rust
struct DebugPeer {
    name: String,
    client: RelayClient,    // in-memory state + HTTP transport
    storage: Storage,       // SQLite-backed persistence
    data_dir: PathBuf,      // peer's temp subdirectory
}
```

After each `sync`, received messages are written to `storage.insert_message()`.
After each send, the outgoing envelope is written to `storage.insert_outbox()`.

### Group key distribution

Groups are created and distributed entirely in-process:

1. `create-group peer1 mygroup peer2 peer3` generates a random 32-byte symmetric key in
   `peer1`'s `GroupManager`.
2. The key is copied directly into each member's `GroupManager` (no relay round-trip).
3. Each member's group entry is populated with the full membership set so that
   `send_group_message` membership checks pass.

This is a deliberate in-process shortcut for the debugger; production key distribution
uses the consent-based invite flow (see [groups.md](../groups.md)).

## Commands

### Peer management

| Command | Description |
|---------|-------------|
| `peers` | List all peers with name, ID prefix, and online/offline status |
| `keys <peer>` | Show a peer's signing public key and encryption public key (hex) |
| `add-peer <peer> <target>` | Register `target` in `peer`'s peer registry (needed before messaging) |
| `add-all-peers` | Register every peer with every other peer (convenience shortcut) |
| `remove-peer <peer> <target>` | Unregister `target` from `peer`'s registry |
| `online <peer>` | Mark peer as online |
| `offline <peer>` | Mark peer as offline |

### Messaging

| Command | Description |
|---------|-------------|
| `send <from> <to> <message>` | Send an encrypted direct message; persists to sender's outbox |
| `broadcast <peer> <message>` | Send a public (broadcast) message |

### Sync

| Command | Description |
|---------|-------------|
| `sync <peer\|all> [limit]` | Fetch envelopes from relay inbox; persists decoded messages to storage |

### Feeds & storage

| Command | Description |
|---------|-------------|
| `feed <peer>` | Show the in-memory direct+group message feed |
| `public-feed <peer>` | Show the in-memory public message cache |
| `messages <peer> [--kind direct\|public\|friend_group] [--limit N]` | Query stored messages from SQLite |
| `conversations <peer>` | Show the DM conversation list from SQLite |

### Groups

| Command | Description |
|---------|-------------|
| `create-group <peer> <group-id> [member1 …]` | Create an encrypted group; distribute symmetric key to all members |
| `groups <peer>` | List groups that `peer` belongs to |
| `group-info <peer> <group-id>` | Show group members and key version |
| `send-group <peer> <group-id> <message>` | Send an encrypted group message |
| `add-member <peer> <group-id> <new-member>` | Add a peer to an existing group |

### Relay

| Command | Description |
|---------|-------------|
| `relay-status` | Show relay port, total queued messages, and per-inbox counts |

### Inspection

| Command | Description |
|---------|-------------|
| `inspect <peer>` | Full peer state: id, online, feed size, public cache, group count, stored message count, data dir |

## Typical demo session

```
tenet-debugger> add-all-peers
registered all 3 peers with each other

tenet-debugger> send peer-1 peer-2 hello
sent peer-1 -> peer-2 (id: a1b2c3d4…)

tenet-debugger> sync peer-2
peer-2 synced 1 envelopes (1 decoded, 0 errors)

tenet-debugger> feed peer-2
Feed for peer-2:
  [1718000000] peer-1 → hello

tenet-debugger> create-group peer-1 team peer-2 peer-3
created group 'team' with 3 members

tenet-debugger> send-group peer-1 team good morning
sent group message peer-1 → 'team' (id: deadbeef…)

tenet-debugger> sync all
peer-1 inbox is empty
peer-2 synced 1 envelopes (1 decoded, 0 errors)
peer-3 synced 1 envelopes (1 decoded, 0 errors)

tenet-debugger> messages peer-2 --kind friend_group
Messages for peer-2 (1 shown):
  [friend_group] 1718000001 a1b2c3d4…→*… | good morning

tenet-debugger> relay-status
Relay: http://127.0.0.1:54321
  queued messages: 0
  known peers:     3

tenet-debugger> inspect peer-1
Peer: peer-1
  id:              a1b2c3d4…(full hex)
  online:          true
  feed size:       0
  public cache:    0
  groups:          1
  known peers:     2
  stored messages: 1
  data dir:        /tmp/xyz/peer-1
```
