Simulation
==========

Tenet's simulator (`tenet-sim`) models a peer-to-peer network using an **event-based execution
model**: all activity — message sends, online/offline transitions, inbox polls, and store-and-forward
operations — is represented as discrete events scheduled at continuous simulated timestamps and
processed in chronological order.

Each simulation run consumes a TOML scenario file that describes the network topology, node
behaviour, and relay configuration. Ready-made scenarios are in `scenarios/`.

## Running the simulator

```bash
# Non-interactive (fast-forward, prints a JSON report at the end)
cargo run --bin tenet-sim -- scenarios/basic.toml

# Interactive TUI
cargo run --bin tenet-sim -- --tui scenarios/basic.toml
```

## Execution model

### Event-based loop

Simulation time is a continuous `f64` (seconds since the start of the run). At initialisation the
scenario builder populates a priority queue with scheduled events. The main loop dequeues the
earliest event, advances the simulation clock to that event's time, processes it, and records it to
an append-only event log. Processing one event may schedule further events (e.g. a
`MessageDeliver` event is created when a `MessageSend` is processed).

Event types:

| Event | What it does |
|-------|-------------|
| `MessageSend` | Builds an envelope for the sender, posts it to the relay (or attempts direct delivery), then schedules a `MessageDeliver` event at `send_time + latency`. |
| `MessageDeliver` | Adds the envelope to the recipient's inbox and records the end-to-end latency. |
| `OnlineTransition` | Flips a client's online state. When going online, schedules an `InboxPoll` and an `OnlineAnnounce`. |
| `InboxPoll` | Fetches the relay inbox for an online client and processes any waiting messages. |
| `OnlineAnnounce` | Broadcasts the client's online status to peers and triggers store-and-forward delivery. |

### Time control

In fast-forward mode (the default) the clock jumps directly to each event's timestamp — no real
time is consumed. In real-time mode the loop sleeps between events to maintain a configured
`speed_factor` (simulated seconds per wall-clock second). The TUI exposes `+`/`-` keys to adjust
the speed factor at runtime.

## Scenario format

```toml
[simulation]
# ... required simulation fields ...

[relay]
# ... relay settings ...

# direct_enabled = true
```

`direct_enabled` is optional. When omitted the simulation uses direct delivery links in addition to
the relay.

## Store-and-forward example

To exercise peer-assisted store-and-forward, use a small encrypted scenario with three nodes.
When node B is offline, node A wraps the message for storage at node C and C forwards it once B
comes online.

```toml
[simulation]
node_ids = ["node-a", "node-b", "node-c"]
steps = 20
duration_seconds = 6000
seed = 7

[simulation.simulated_time]
seconds_per_step = 300
default_speed_factor = 1.0

[simulation.friends_per_node]
type = "uniform"
min = 2
max = 2

[simulation.post_frequency]
type = "weighted_schedule"
weights = [0.2, 0.15, 0.1, 0.1, 0.15, 0.3, 0.5, 0.7, 0.9, 1.0, 1.0, 0.95, 0.9, 0.8, 0.75, 0.8, 0.9, 1.0, 0.95, 0.8, 0.6, 0.45, 0.3, 0.25]
total_posts = 6

[[simulation.cohorts]]
name = "always-online"
type = "always_online"
share = 0.25

[[simulation.cohorts]]
name = "rarely-online"
type = "rarely_online"
share = 0.35
online_probability = 0.2

[[simulation.cohorts]]
name = "diurnal"
type = "diurnal"
share = 0.4
online_probability = 0.75
timezone_offset_hours = -5
hourly_weights = [0.2, 0.15, 0.1, 0.1, 0.15, 0.3, 0.5, 0.7, 0.9, 1.0, 1.0, 0.95, 0.9, 0.8, 0.75, 0.8, 0.9, 1.0, 0.95, 0.8, 0.6, 0.45, 0.3, 0.25]

[simulation.message_size_distribution]
type = "uniform"
min = 20
max = 80

[simulation.encryption]
type = "encrypted"

[relay]
ttl_seconds = 3600
max_messages = 200
max_bytes = 1048576
retry_backoff_seconds = [1, 2, 4]
```

The full scenario is available as `scenarios/store_and_forward_3.toml`.

## `[simulation]` fields

- `node_ids` (array of strings): IDs to use for simulated peers.
- `steps` (integer): number of simulation steps when `duration_seconds` is not provided.
- `duration_seconds` (integer, optional): total simulated duration in seconds. When provided, the
  simulation derives `steps` from `duration_seconds / simulated_time.seconds_per_step` (rounded up)
  and uses that value for schedules and planned sends.
- `seed` (integer): RNG seed.
- `simulated_time` (optional): simulated-time settings (see below).
- `friends_per_node`: friend graph distribution (see below).
- `post_frequency`: how often each node posts (see below).
- `availability` (optional): legacy online/offline behavior when no cohorts are defined.
- `cohorts` (optional): cohort definitions for online behavior (see below).
- `message_size_distribution`: message size generator (see below).
- `message_type_weights` (optional): relative weights for direct, public, and group messages (see below).
- `clustering` (optional): cluster layout for dense subgraphs with sparse cross-links.
- `encryption` (optional): message payload handling (`plaintext` or `encrypted`).
- `groups` (optional): group creation and membership configuration (see below).
- `reaction_config` (optional): probability and delay for reply reactions (see below).

### `friends_per_node`

```toml
[simulation.friends_per_node]
type = "uniform"
min = 2
max = 6
```

```toml
[simulation.friends_per_node]
type = "poisson"
lambda = 3.5
```

```toml
[simulation.friends_per_node]
type = "zipf"
max = 12
exponent = 1.2
```

### `post_frequency`

```toml
[simulation.post_frequency]
type = "poisson"
lambda_per_hour = 0.4
```

`lambda_per_step` is still supported for legacy scenarios without simulated-time settings.

```toml
[simulation.post_frequency]
type = "weighted_schedule"
weights = [1, 1, 2, 3, 5]
# total posts per node
total_posts = 20
```

When `weights` contains 24 entries, they are treated as hourly weights over a simulated day.

### `availability`

```toml
[simulation.availability]
type = "bernoulli"
p_online = 0.75
```

```toml
[simulation.availability]
type = "markov"
p_online_given_online = 0.92
p_online_given_offline = 0.25
start_online_prob = 0.8
```

### `simulated_time` (optional)

```toml
[simulation.simulated_time]
seconds_per_step = 300
default_speed_factor = 1.0
```

### `cohorts` (optional)

```toml
[[simulation.cohorts]]
name = "always-online"
type = "always_online"
share = 0.2
```

```toml
[[simulation.cohorts]]
name = "rarely-online"
type = "rarely_online"
share = 0.5
online_probability = 0.1
```

```toml
[[simulation.cohorts]]
name = "diurnal"
type = "diurnal"
share = 0.3
online_probability = 0.7
timezone_offset_hours = 2
hourly_weights = [0.2, 0.15, 0.1, 0.1, 0.15, 0.3, 0.5, 0.7, 0.9, 1.0, 1.0, 0.95, 0.9, 0.8, 0.75, 0.8, 0.9, 1.0, 0.95, 0.8, 0.6, 0.45, 0.3, 0.25]
```

### `message_size_distribution`

```toml
[simulation.message_size_distribution]
type = "uniform"
min = 20
max = 300
```

```toml
[simulation.message_size_distribution]
type = "normal"
mean = 120
std_dev = 30
min = 40
max = 400
```

```toml
[simulation.message_size_distribution]
type = "log_normal"
mean = 3.2
std_dev = 0.7
min = 40
max = 400
```

### `message_type_weights` (optional)

Controls the relative proportion of direct, public, and group messages. Values are normalised at
runtime so only relative magnitudes matter. Defaults to direct-only.

```toml
[simulation.message_type_weights]
direct = 3.0
public = 1.0
group = 1.0
```

### `encryption` (optional)

```toml
[simulation.encryption]
type = "plaintext"
```

```toml
[simulation.encryption]
type = "encrypted"
```

### `clustering` (optional)

```toml
[simulation.clustering]
cluster_sizes = [8, 8, 8, 12]
inter_cluster_friend_probability = 0.02
```

Clusters are assigned in order from `node_ids`. The `cluster_sizes` list is consumed in order,
with any remaining nodes appended as a final cluster. `friends_per_node` is sampled within each
cluster. `inter_cluster_friend_probability` controls the chance of creating a cross-cluster
connection for each node pair from different clusters.

### `groups` (optional)

Configures automatic group creation and membership assignment across nodes.

```toml
[simulation.groups]
count = 3

[simulation.groups.size_distribution]
type = "fraction_of_nodes"
min_fraction = 0.2
max_fraction = 0.5

[simulation.groups.memberships_per_node]
type = "fixed"
count = 1
```

`size_distribution` variants: `uniform` (with `min`/`max` absolute member counts) or
`fraction_of_nodes` (with `min_fraction`/`max_fraction` relative to total node count).

`memberships_per_node` variants: `fixed` (with `count`) or `uniform` (with `min`/`max`).

### `reaction_config` (optional)

When set, receiving a message may trigger an automatic reply after a configurable delay.

```toml
[simulation.reaction_config]
reply_probability = 0.3

[simulation.reaction_config.reply_delay_distribution]
type = "uniform"
min = 30.0
max = 300.0
```

`reply_delay_distribution` uses the same variants as latency distributions: `fixed` (with
`seconds`), `uniform` (with `min`/`max`), `normal` (with `mean`/`std_dev`), and `log_normal`
(with `mean`/`std_dev`).

## `[relay]` fields

```toml
[relay]
ttl_seconds = 3600
max_messages = 1000
max_bytes = 5242880
retry_backoff_seconds = [1, 2, 4]
peer_log_window_seconds = 60
peer_log_interval_seconds = 30
```

The relay logging fields are optional; when omitted they default to a 60-second window with a
30-second summary interval. Relay TTLs are meant to be short-lived; the sample default is 3600s,
and values must stay within protocol bounds (1s minimum, 7 days maximum).

## Simulation metrics

Running `tenet-sim` prints a `SimulationReport` JSON blob that includes both real-time counters
(`metrics`) and aggregate summaries (`report`). The `metrics` section is a `SimulationMetrics`
struct with counters that track how messages move through the system. Each field increments in
specific parts of the simulation loop:

- `planned_messages`: set to the number of planned sends (`plan.len()`) at the start of a run, so
  it reflects how many messages the scenario intended to send.
- `sent_messages`: incremented when a planned message is actually sent (the sender is online at the
  scheduled step) and the simulator posts the envelope to the relay (and optionally attempts direct
  delivery).
- `direct_deliveries`: incremented when `route_message` succeeds in delivering a message directly
  to an online recipient during the same step.
- `inbox_deliveries`: incremented when a node pulls its relay inbox and accepts a direct message
  envelope (newly online or already online inbox polling).
- `store_forwards_stored`: incremented when a storage peer receives a `StoreForPeer` envelope and
  queues the inner message for later forwarding.
- `store_forwards_forwarded`: incremented when a storage peer is online, the intended recipient is
  online, and the stored message is forwarded back to the relay.
- `store_forwards_delivered`: incremented when a forwarded store-and-forward message is delivered
  to the recipient (tracked via the pending forwarded message set).

Common relationships to expect:

- `planned_messages` is greater than or equal to `sent_messages` because offline senders skip
  sending at their scheduled step.
- Store-and-forward counters typically decrease in order (`store_forwards_stored` ≥
  `store_forwards_forwarded` ≥ `store_forwards_delivered`) since forwarding and delivery depend on
  online peers and successful inbox delivery.

The `report` section (`SimulationMetricsReport`) adds aggregate summaries:

- `peer_feed_messages`: min/avg/max feed message counts per peer.
- `stored_unforwarded_by_peer`: min/avg/max counts of stored-but-not-forwarded messages per peer.
- `stored_forwarded_by_peer`: min/avg/max counts of stored-and-forwarded messages per peer.
- `sent_messages_by_kind`: per message kind min/avg/max sent counts per peer.
- `message_size_by_kind`: per message kind min/avg/max payload sizes in **bytes**.

If a message kind has `no samples`, it means the scenario did not generate that kind. The current
scenarios primarily emit direct messages and store-and-forward traffic, so `public` and
`friend_group` kinds will remain at zero until scenarios start scheduling those messages. Meta
messages are emitted for online broadcasts, acknowledgements, and missed-message requests; if the
`meta` kind remains at zero it indicates a simulation bug rather than an optional feature.

## TUI controls

When running `tenet-sim --tui`, use these keys to control the simulation:

- `a`: add a peer (type an id, or press Enter for an auto-generated id).
- `f`: add a friendship (enter two peer ids separated by space or comma).
- `+` / `=`: speed up simulated time.
- `-`: slow down simulated time.
- `q`: quit once the simulation has finished.

### Manual TUI test

1. Run a simulation with the TUI enabled: `cargo run --bin tenet-sim -- --tui scenarios/basic.toml`.
2. Wait for the simulation to complete and confirm the status panel displays the completion prompt.
3. Press `q` and confirm the TUI exits back to the terminal.
