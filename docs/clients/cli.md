# `tenet` CLI

The `tenet` binary is a lightweight command-line client for key management, messaging, and
local database inspection. It uses the same SQLite database as `tenet-web`, so peers, messages,
and identities are shared between the two clients.

For a richer interactive experience (groups, friend requests, reactions, profiles), use
`tenet-web` instead.

## Running

```bash
cargo run --bin tenet -- <command> [args] [--identity <id>]
```

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `TENET_HOME` | `.tenet` | Data directory containing identities |
| `TENET_RELAY_URL` | — | Default relay URL (overrides the `http://127.0.0.1:8080` fallback) |
| `TENET_IDENTITY` | — | Select identity by short ID prefix |

## Commands

### `init`

Create a new identity (or show the existing one if already initialized).

```bash
cargo run --bin tenet -- init
# identity created: <peer_id>
```

With an explicit identity name:
```bash
cargo run --bin tenet -- --identity alice init
```

### `add-peer <name> <public_key_hex>`

Register a peer by their X25519 public key (hex). The peer's ID is derived from their key.
Writes directly to the SQLite `peers` table shared with `tenet-web`.

```bash
cargo run --bin tenet -- add-peer alice abc123...
# peer saved: alice (<peer_id>)
```

### `send <peer_name> <message> [--relay <url>]`

Encrypt and send a Direct message to a named peer via relay. Defaults to
`http://127.0.0.1:8080` if no relay URL is given.

```bash
cargo run --bin tenet -- send alice "hello there"
cargo run --bin tenet -- send alice "hello" --relay http://relay.example.com
TENET_RELAY_URL=http://relay.example.com cargo run --bin tenet -- send alice "hello"
```

The sent envelope is recorded in the SQLite `outbox` table.

### `sync [--relay <url>]`

Fetch and decrypt incoming Direct messages from the relay inbox. Defaults to
`http://127.0.0.1:8080` if no relay URL is given.

```bash
cargo run --bin tenet -- sync
# from <peer_id>: hello there
# synced 1 direct message(s)
```

Received messages are stored in the SQLite `messages` table.

### `post <message> [--relay <url>]`

Broadcast a Public message to the relay. Defaults to `http://127.0.0.1:8080`.

```bash
cargo run --bin tenet -- post "hello world"
```

### `receive-all [--relay <url>]`

Like `sync` but shows all message kinds (direct, public, group). Defaults to
`http://127.0.0.1:8080`.

```bash
cargo run --bin tenet -- receive-all
# [public] from <peer_id>: hello world
# [direct] from <peer_id>: hey
# received 2 message(s)
```

### `list-peers [--sort recent|name|added] [--limit N] [--friends]`

List known peers from the local database, sorted by most recent message by default.

```bash
cargo run --bin tenet -- list-peers
cargo run --bin tenet -- list-peers --sort name
cargo run --bin tenet -- list-peers --friends --limit 5
```

Output columns: name, peer ID prefix, friend status, last message age.

### `list-friends [--sort recent|name|added] [--limit N]`

Shortcut for `list-peers --friends`.

```bash
cargo run --bin tenet -- list-friends
```

### `peer <peer_id_or_name> [--limit N]`

Show detailed info for a single peer: metadata, profile bio (if stored), recent direct
messages, and recent public posts. The query matches peer ID (exact or prefix) or display
name (case-insensitive, substring).

```bash
cargo run --bin tenet -- peer alice
cargo run --bin tenet -- peer abc123def456  # peer_id prefix
cargo run --bin tenet -- peer alice --limit 20
```

### `feed [--limit N]`

Show the local public message feed (messages previously received via `sync`,
`receive-all`, or `post`). Defaults to 20 most recent messages.

```bash
cargo run --bin tenet -- feed
cargo run --bin tenet -- feed --limit 50
```

### `export-key [--public | --private]`

Print key material for the selected identity.

```bash
cargo run --bin tenet -- export-key           # full keypair JSON
cargo run --bin tenet -- export-key --public  # X25519 public key hex
cargo run --bin tenet -- export-key --private # X25519 private key hex
```

The public key is what you share with peers so they can `add-peer` you.

### `import-key <public_key_hex> <private_key_hex>`

Import an existing X25519 keypair as a new identity. New Ed25519 signing keys are generated
automatically.

```bash
cargo run --bin tenet -- import-key <pub_hex> <priv_hex>
```

### `rotate-key`

Generate a new keypair as a new identity and set it as default. The old identity remains in
`{TENET_HOME}/identities/` and can be selected with `--identity`.

```bash
cargo run --bin tenet -- rotate-key
# identity rotated from <old_id> to <new_id>
# (share the new public key with peers)
```

## Multi-Identity Usage

The CLI fully supports multiple identities. See [identity.md](identity.md) for the full
identity management model.

```bash
# Create a second identity
cargo run --bin tenet -- init

# Select by short ID prefix
cargo run --bin tenet -- --identity abc12 export-key

# Or via environment variable
TENET_IDENTITY=abc12 cargo run --bin tenet -- export-key
```

## File Layout

```
{TENET_HOME}/
  config.toml               <- default_identity setting
  identities/
    {short_id}/
      tenet.db              <- SQLite (identity, peers, messages, outbox, relay config)
  attachments/              <- shared attachment blobs
```

All state (identity, peers, sent and received messages, relays) lives in a single SQLite
database per identity. The CLI and `tenet-web` share this database when using the same
`TENET_HOME`.
