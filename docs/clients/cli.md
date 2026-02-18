# `tenet` CLI

The `tenet` binary is a lightweight command-line client for key management and basic messaging.
It stores its peer list as a `peers.json` file and appends messages to JSONL files. It does
**not** use the web client's SQLite database for peer state — the two clients have separate peer
registries.

For a richer experience (groups, friend requests, reactions, profiles), use `tenet-web` instead.

## Running

```bash
cargo run --bin tenet -- <command> [args] [--identity <id>]
```

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `TENET_HOME` | `.tenet` | Data directory containing identities |
| `TENET_RELAY_URL` | — | Default relay URL for `send` and `sync` |
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

```bash
cargo run --bin tenet -- add-peer alice abc123...
# peer saved: alice (<peer_id>)
```

Peers are stored in `{TENET_HOME}/identities/{short_id}/peers.json`.

### `send <peer_name> <message> [--relay <url>]`

Encrypt and send a Direct message to a named peer via relay.

```bash
cargo run --bin tenet -- send alice "hello there" --relay http://localhost:8080

# Or use environment variable:
TENET_RELAY_URL=http://localhost:8080 cargo run --bin tenet -- send alice "hello"
```

The sent envelope is appended to `outbox.jsonl` in the identity directory.

### `sync [--relay <url>]`

Fetch and decrypt incoming Direct messages from the relay inbox.

```bash
cargo run --bin tenet -- sync --relay http://localhost:8080
# from <peer_id>: hello there
# synced 1 envelopes
```

Received messages are appended to `inbox.jsonl` in the identity directory.

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
automatically (signing keys are not part of the importable key format).

```bash
cargo run --bin tenet -- import-key <pub_hex> <priv_hex>
```

### `rotate-key`

Generate a new keypair as a new identity and set it as default. The old identity remains in
`{TENET_HOME}/identities/` and can be selected with `--identity`. Peers are copied to the new
identity directory.

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
cargo run --bin tenet -- init  # creates another identity

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
      tenet.db              <- SQLite (identity + relay config)
      peers.json            <- peer registry (CLI-managed, NOT shared with tenet-web)
      inbox.jsonl           <- received messages (appended by sync)
      outbox.jsonl          <- sent envelopes (appended by send)
```

## Limitations

- **Peers are not shared with `tenet-web`**: the CLI stores peers in `peers.json`; the web
  client uses the SQLite `peers` table. Adding a peer in the CLI does not make them visible in
  the web UI, and vice versa.
- **Direct messages only**: the CLI sends and receives `Direct` messages only. Public posts,
  group messages, friend requests, reactions, and profiles require `tenet-web`.
- **No SQLite message storage**: received messages are appended to `inbox.jsonl` (plain text),
  not into the structured SQLite `messages` table used by `tenet-web`.
- **Relay URL required for send/sync**: the CLI does not discover relays; a URL must be given
  via `--relay`, `TENET_RELAY_URL`, or stored in the identity's relay table by `tenet-web`.
