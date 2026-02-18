# Simulation

Tenet's simulator (`tenet-sim`) models a peer-to-peer network using an **event-based execution
model**: all activity — message sends, online/offline transitions, inbox polls, and
store-and-forward operations — is represented as discrete events scheduled at continuous
simulated timestamps and processed in chronological order.

Each simulation run consumes a TOML scenario file. See [scenarios.md](scenarios.md) for the
full scenario format reference. Ready-made scenarios live in `scenarios/`.

## Running the Simulator

```bash
# Non-interactive (fast-forward, prints a JSON report at the end)
cargo run --bin tenet-sim -- scenarios/small_dense_6.toml

# Interactive TUI
cargo run --bin tenet-sim -- --tui scenarios/small_dense_6.toml
```

The simulation harness is also available as an integration test:

```bash
cargo test --test simulation_harness_tests
```

## Protocol Feature Coverage

| Feature | Covered | Notes |
|---------|---------|-------|
| `MessageKind::Direct` | ✅ | All scenarios; default message type |
| `MessageKind::Public` | ✅ | `public_messages_test.toml`, `group_messages_test.toml` |
| `MessageKind::FriendGroup` | ✅ | `group_messages_test.toml` |
| `MessageKind::StoreForPeer` | ✅ | `store_and_forward_3.toml` |
| `MetaMessage::Online/Ack/MessageRequest` | ✅ | Generated on every online transition |
| `MetaMessage::FriendRequest/FriendAccept` | ✅ | `friend_discovery.toml`; configured via `friend_request_config` |
| Reply threads (`reply_to` field) | ✅ | `reaction_config` schedules same-kind replies |
| Network degradation (drops, latency) | ✅ | `degraded_network.toml`; configured via `[network_conditions]` |
| Profiles | ❌ | No profile broadcasts simulated |
| Attachments | ❌ | Messages are plain text only |

### Ready-Made Scenarios

All scenarios use HPKE encryption for Direct messages, matching real protocol behaviour. Public
and Group message payloads are always plaintext by design (no single recipient to encrypt to).

| Scenario | Message types | Reply prob. | Groups | Purpose |
|----------|--------------|-------------|--------|---------|
| `small_dense_6.toml` | Direct | 25% | No | Baseline: 6 peers, three cohorts |
| `public_messages_test.toml` | Direct + Public | 30% | No | Public broadcasting |
| `group_messages_test.toml` | Direct + Public + Group | 30% | Yes (3 groups) | Group messaging |
| `reply_chains.toml` | Direct | 50% | No | Reply-chain focus |
| `store_and_forward_3.toml` | Direct | — | No | Peer-assisted store-and-forward |
| `large_clustered_100.toml` | Direct | 15% | No | 100 peers; scale stress test |
| `degraded_network.toml` | Direct | — | No | 15% message-drop rate |
| `friend_discovery.toml` | Direct | — | No | Dynamic friend graph via FriendRequest |

### Known Gaps

- **Profiles** — profile creation and broadcast are not scheduled (out of scope for the
  transport-layer harness).
- **Attachments** — simulated messages are plain text; no binary attachment payloads.
- **TTL expiry** — no scenario uses a short enough relay TTL with sufficiently offline recipients
  to specifically exercise the expired-message path. A scenario with `ttl_seconds = 60` and
  `online_probability = 0.05` would cover this.

## Execution Model

### Event-Based Loop

Simulation time is a continuous `f64` (seconds since the start of the run). At initialisation
the scenario builder populates a priority queue with scheduled events. The main loop dequeues the
earliest event, advances the simulation clock to that event's time, processes it, and records it
to an append-only event log. Processing one event may schedule further events (e.g. a
`MessageDeliver` event is created when a `MessageSend` is processed).

| Event | What it does |
|-------|-------------|
| `MessageSend` | Builds an envelope, posts to relay (or attempts direct delivery), schedules `MessageDeliver` |
| `MessageDeliver` | Adds envelope to recipient's inbox and records end-to-end latency |
| `OnlineTransition` | Flips a client's online state; going online schedules `InboxPoll` and `OnlineAnnounce` |
| `InboxPoll` | Fetches the relay inbox for an online client |
| `OnlineAnnounce` | Broadcasts online status to peers and triggers store-and-forward delivery |

### Time Control

In fast-forward mode (the default) the clock jumps directly to each event's timestamp — no real
time is consumed. In real-time mode the loop sleeps between events to maintain a configured
`speed_factor`. The TUI exposes `+`/`-` keys to adjust the speed factor at runtime.

## Simulation Metrics

Running `tenet-sim` prints a `SimulationReport` JSON blob with both real-time counters
(`metrics`) and aggregate summaries (`report`).

### Counter Fields (`SimulationMetrics`)

| Field | Incremented when |
|-------|-----------------|
| `planned_messages` | Set to `plan.len()` at run start |
| `sent_messages` | Sender is online and envelope is posted to relay |
| `direct_deliveries` | `route_message` delivers directly to an online recipient |
| `inbox_deliveries` | Node pulls relay inbox and accepts a direct message envelope |
| `store_forwards_stored` | Storage peer receives a `StoreForPeer` envelope |
| `store_forwards_forwarded` | Storage peer forwards stored message to relay |
| `store_forwards_delivered` | Forwarded message is delivered to the recipient |

Common relationships:
- `planned_messages` ≥ `sent_messages` (offline senders skip their scheduled step)
- `store_forwards_stored` ≥ `store_forwards_forwarded` ≥ `store_forwards_delivered`

### Aggregate Fields (`SimulationMetricsReport`)

| Field | Description |
|-------|-------------|
| `peer_feed_messages` | min/avg/max feed message counts per peer |
| `stored_unforwarded_by_peer` | min/avg/max stored-but-not-forwarded messages per peer |
| `stored_forwarded_by_peer` | min/avg/max stored-and-forwarded messages per peer |
| `sent_messages_by_kind` | per message kind min/avg/max sent counts per peer |
| `message_size_by_kind` | per message kind min/avg/max payload sizes (bytes) |

If a message kind shows `no samples`, the scenario did not generate that kind. Use
`public_messages_test.toml` or `group_messages_test.toml` to see public/group kinds. If the
`meta` kind is zero, that indicates a simulation bug.

## TUI Controls

When running `tenet-sim --tui`:

| Key | Action |
|-----|--------|
| `a` | Add a peer (type an id, or Enter for auto-generated) |
| `f` | Add a friendship (enter two peer ids separated by space or comma) |
| `+` / `=` | Speed up simulated time |
| `-` | Slow down simulated time |
| `↑` / `↓` | Scroll activity log one line |
| `PgUp` / `PgDn` | Scroll activity log ten lines |
| `q` | Quit once simulation has finished |
