# Relay Server

The Tenet relay is a store-and-forward server that acts as an intermediary between peers. Peers
that are behind NAT, on mobile networks, or intermittently connected submit and fetch encrypted
envelopes through the relay. The relay never has the keys to read envelope contents.

## Running the Relay

```bash
# Default: listens on 0.0.0.0:8080
cargo run --bin tenet-relay

# With environment variables
TENET_RELAY_BIND=127.0.0.1:8080 \
TENET_RELAY_TTL_SECS=3600 \
TENET_RELAY_MAX_MESSAGES=1000 \
TENET_RELAY_MAX_BYTES=5242880 \
TENET_RELAY_PEER_LOG_WINDOW_SECS=60 \
TENET_RELAY_PEER_LOG_INTERVAL_SECS=30 \
  cargo run --bin tenet-relay
```

## Configuration

| Environment variable | Default | Description |
|----------------------|---------|-------------|
| `TENET_RELAY_BIND` | `0.0.0.0:8080` | Listen address |
| `TENET_RELAY_TTL_SECS` | `3600` | Maximum TTL for stored envelopes (seconds) |
| `TENET_RELAY_MAX_MESSAGES` | `1000` | Maximum number of stored envelopes |
| `TENET_RELAY_MAX_BYTES` | `5242880` | Maximum total storage (bytes) |
| `TENET_RELAY_PEER_LOG_WINDOW_SECS` | `60` | Peer activity log window |
| `TENET_RELAY_PEER_LOG_INTERVAL_SECS` | `30` | Peer activity summary interval |

TTL values must be within protocol bounds: **1 second minimum, 7 days (604800 seconds) maximum**.

## HTTP API

The relay exposes these endpoints (separate from the web client API):

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/health` | GET | Health check; returns relay status |
| `/envelopes` | POST | Store a single envelope |
| `/envelopes/batch` | POST | Store multiple envelopes |
| `/inbox/{recipient_id}` | GET | Fetch stored envelopes for a peer |
| `/inbox/batch` | POST | Fetch envelopes for multiple peers |
| `/ws/{recipient_id}` | WebSocket | Real-time envelope delivery |

### Envelope Format

Envelopes submitted to the relay are JSON objects with a signed header and an encrypted payload.
The relay stores them opaquely — it only inspects the header metadata needed for routing and TTL
enforcement.

### Inbox Polling

Clients poll `GET /inbox/{recipient_id}` to fetch envelopes addressed to them. The relay returns
all non-expired envelopes for that recipient ID. After successful delivery, the relay does not
automatically delete envelopes (they expire via TTL).

### WebSocket Push

`GET /ws/{recipient_id}` upgrades to a WebSocket connection. The relay pushes envelopes to the
connected client in real time as they arrive, eliminating the need to poll. The web client
connects to this endpoint via `relay_ws_listen_loop` and uses a `Notify` signal to wake the sync
loop immediately when an envelope arrives, giving near-real-time message delivery.

## Dashboard

The relay serves a built-in HTML dashboard at `/` showing:
- Number of queued envelopes per recipient
- Total stored bytes
- Per-peer activity log

## Relay Design Principles

- **Untrusted by design**: the relay stores opaque encrypted blobs. It can see sender/recipient
  IDs and timestamps, but not message contents.
- **No persistence across restarts**: the relay holds messages in memory. Restarting the relay
  drops all queued envelopes.
- **TTL enforcement**: envelopes with expired TTLs are dropped. TTLs are set by senders and
  capped by the relay's configured maximum.
- **Size limits**: per-recipient storage is capped; the relay rejects envelopes when limits are
  reached.
- **Deduplication**: the relay deduplicates by message ID, so retried submissions do not result
  in duplicate deliveries.

## How Peers Connect

Peers connect to the relay using the `RelayClient` (HTTP). The connection flow is:

1. **Announce online**: on startup, a peer sends `MetaMessage::Online` to known peers via the
   relay. This lets friends know the peer is available.
2. **Poll inbox**: the background sync loop periodically calls `GET /inbox/{my_peer_id}` to
   fetch pending envelopes.
3. **Send messages**: outgoing envelopes are `POST`ed to `/envelopes`. The relay stores them in
   the recipient's inbox until fetched or TTL expires.
4. **WebSocket push**: the web client also opens a WebSocket at `/ws/{recipient_id}` to receive
   envelopes in real time. When the relay pushes a new envelope over this connection, the web
   client wakes its sync loop immediately rather than waiting for the next polling interval.

The web client's sync loop (`src/web_client/sync.rs`) runs as a background Tokio task and
handles inbox polling, envelope decryption, and storage writes.

## Using the In-Process Relay (Debugger)

The `tenet-debugger` starts its own in-process relay on a random port (`127.0.0.1:0`) backed by
the real Axum relay code. This exercises the full relay HTTP path with no external dependency.

```bash
# In-process relay (default)
cargo run --bin tenet-debugger -- --peers 4

# External relay
cargo run --bin tenet-debugger -- --peers 4 --relay http://127.0.0.1:8080
```

See [clients/debugger.md](clients/debugger.md) for more.

## Relay in the Simulation

The simulation harness uses `SimulationClient` rather than `RelayClient`, so messages are routed
in-process. However, scenario files include a `[relay]` block that configures relay-equivalent
behavior (TTL, capacity, retry backoff):

```toml
[relay]
ttl_seconds = 3600
max_messages = 1000
max_bytes = 5242880
retry_backoff_seconds = [1, 2, 4]
```

See [sim/scenarios.md](sim/scenarios.md) for the full relay configuration reference.
