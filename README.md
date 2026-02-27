tenet
=====

A protocol concept for a peer-to-peer social network and a Rust reference implementation.

## Documentation

| Document | Description |
|----------|-------------|
| [docs/architecture.md](docs/architecture.md) | Protocol design, cryptographic model, threat model, message flows |
| [docs/friend_requests.md](docs/friend_requests.md) | Friend request protocol and key exchange |
| [docs/groups.md](docs/groups.md) | Group creation, invite flow, and key distribution |
| [docs/relay.md](docs/relay.md) | Relay server operation and HTTP API |
| [docs/clients/web.md](docs/clients/web.md) | Web client (`tenet-web`): API, WebSocket events, SPA |
| [docs/clients/cli.md](docs/clients/cli.md) | CLI tool (`tenet`): key management and direct messaging |
| [docs/clients/identity.md](docs/clients/identity.md) | Multi-identity management and device setup |
| [docs/clients/debugger.md](docs/clients/debugger.md) | Interactive REPL debugger reference |
| [docs/clients/howto.md](docs/clients/howto.md) | How to build clients; `StorageMessageHandler` design |
| [docs/sim/simulation.md](docs/sim/simulation.md) | Simulation execution model, metrics, TUI |
| [docs/sim/scenarios.md](docs/sim/scenarios.md) | Scenario TOML format reference |

## Build

```bash
cargo build
```

The build process includes an automatic step that generates the web UI's `index.html` file. The
`build.rs` script reads source files from `web/src/` (index.html template, styles.css, app.js)
and inlines them into a single `web/dist/index.html` file. This happens automatically during
`cargo build` and requires no manual intervention.

## Relay

The relay is a store-and-forward server that sits between peers. Peers submit encrypted envelopes
to the relay and poll their inbox to fetch incoming ones. The relay never holds keys — it stores
opaque encrypted blobs and only inspects the header metadata needed for routing and TTL enforcement.

**Design principles:**
- **Untrusted by design**: contents are end-to-end encrypted; the relay can see sender/recipient IDs and timestamps but not message content.
- **No persistence across restarts**: envelopes are held in memory. Restarting the relay drops all queued messages.
- **TTL enforcement**: envelopes expire after a sender-specified TTL (1 second minimum, 7 days maximum). The relay caps TTLs at its configured maximum.
- **Deduplication**: repeated submissions of the same message ID are silently dropped.
- **WebSocket push**: clients can open a WebSocket connection (`/ws/{recipient_id}`) to receive envelopes in real time, eliminating the need to poll.

Run the relay (defaults to `0.0.0.0:8080`):

```bash
cargo run --bin tenet-relay
```

Configure via environment variables:

```bash
TENET_RELAY_BIND=127.0.0.1:8080 \
TENET_RELAY_TTL_SECS=3600 \
TENET_RELAY_MAX_MESSAGES=1000 \
TENET_RELAY_MAX_BYTES=5242880 \
TENET_RELAY_PEER_LOG_WINDOW_SECS=60 \
TENET_RELAY_PEER_LOG_INTERVAL_SECS=30 \
  cargo run --bin tenet-relay
```

The relay also serves a built-in status dashboard at `/` showing queued envelope counts, total
stored bytes, and per-peer activity. See [docs/relay.md](docs/relay.md) for the full HTTP API
reference.

## Clients

### Web client

`tenet-web` is a full peer that serves a browser-based SPA alongside a REST/WebSocket API. It
holds its own keypair, connects to the relay, and participates in store-and-forward — it is a
first-class network participant, not a thin proxy.

```bash
# Defaults: data dir ~/.tenet, listen on 127.0.0.1:3000
cargo run --bin tenet-web

# With configuration
TENET_HOME=~/.tenet \
TENET_WEB_BIND=127.0.0.1:3000 \
TENET_RELAY_URL=http://relay.example.com:8080 \
  cargo run --bin tenet-web
```

The SPA supports public timelines, direct messages, friend requests, group messaging, reactions,
replies, profiles, and real-time updates over WebSocket. See [docs/clients/web.md](docs/clients/web.md)
for the full API and feature reference.

### CLI

`tenet` is a lightweight command-line client for key management, messaging, and local database
inspection. It shares the same SQLite database as `tenet-web`, so peers, messages, and identities
are visible in both clients.

```bash
cargo run --bin tenet -- <command> [--identity <id>]
```

Key management:

```bash
cargo run --bin tenet -- init                                 # create (or show) identity
cargo run --bin tenet -- export-key --public                  # print X25519 public key (share with peers)
cargo run --bin tenet -- export-key --private                 # print private key
cargo run --bin tenet -- import-key <pub_hex> <priv_hex>      # import an existing keypair
cargo run --bin tenet -- rotate-key                           # generate new keypair
```

Peer management:

```bash
cargo run --bin tenet -- add-peer <name> <public_key_hex>     # register a peer
cargo run --bin tenet -- list-peers                           # list known peers
cargo run --bin tenet -- list-peers --sort name --limit 10
cargo run --bin tenet -- list-friends                         # friends only
cargo run --bin tenet -- peer alice                           # detail view for one peer
```

Messaging:

```bash
cargo run --bin tenet -- send alice "hello" --relay http://...   # send direct message
cargo run --bin tenet -- post "hello world"                      # broadcast public message
cargo run --bin tenet -- sync                                    # fetch and decrypt direct messages
cargo run --bin tenet -- receive-all                             # fetch all message kinds
cargo run --bin tenet -- feed                                    # show public message feed (local)
```

The `--relay` flag defaults to `http://127.0.0.1:8080`. Set `TENET_RELAY_URL` to avoid repeating
it. See [docs/clients/cli.md](docs/clients/cli.md) for the full command reference.

### Android client

The `android/` directory contains a Jetpack Compose Android app backed by a Rust FFI bridge
(`android/tenet-ffi/`). The Android client uses UniFFI to call the same Tenet library crate used
by the other clients, so all protocol logic is shared. It is not part of the main Cargo workspace
and has its own build system (`android/build.sh`).

### Interactive debugger

The debugger is a self-contained REPL that spawns N in-process peers and an in-process relay. It
is useful for exploring protocol behaviour interactively without needing a running relay or real
peers.

```bash
cargo run --bin tenet-debugger -- --peers 4

# Or against an external relay:
cargo run --bin tenet-debugger -- --peers 4 --relay http://127.0.0.1:8080
```

Once running, try:

```
add-all-peers
send peer-1 peer-2 hello there
sync peer-2
feed peer-2
mesh-query peer-1 peer-2
offline peer-3
online peer-3
```

See [docs/clients/debugger.md](docs/clients/debugger.md) for the full command reference.

## Tests

```bash
cargo test
```

## Simulation

Scenario-driven simulation runs are documented in [docs/sim/simulation.md](docs/sim/simulation.md)
with scenario format details in [docs/sim/scenarios.md](docs/sim/scenarios.md).
Sample scenarios live in `scenarios/`. Run one with:

```bash
cargo run --bin tenet-sim -- scenarios/small_dense_6.toml
```

The simulation harness is also implemented as an integration test:

```bash
cargo test --test simulation_harness_tests
```
