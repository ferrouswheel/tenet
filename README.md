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

## Run

Run the relay service (defaults to `0.0.0.0:8080`):

```bash
cargo run --bin tenet-relay
```

You can also configure the relay via environment variables:

```bash
TENET_RELAY_BIND=127.0.0.1:8080 \
TENET_RELAY_TTL_SECS=3600 \
TENET_RELAY_MAX_MESSAGES=1000 \
TENET_RELAY_MAX_BYTES=5242880 \
  cargo run --bin tenet-relay
```

Run the web client (defaults to `127.0.0.1:3000`):

```bash
TENET_RELAY_URL=http://127.0.0.1:8080 cargo run --bin tenet-web
```

### Utilities

Run the CLI to work on the same local database as the web

```bash
cargo run --bin tenet -- init
cargo run --bin tenet -- add-peer <name> <public_key_hex>
cargo run --bin tenet -- send <peer> <message> --relay http://...
```


Run the interactive debugger (spawns N peers, defaults to 3):

```bash
cargo run --bin tenet-debugger -- --peers 4
```

Once running, try:

```
add-all-peers
send peer-1 peer-2 hello there
sync peer-2
feed peer-2
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
