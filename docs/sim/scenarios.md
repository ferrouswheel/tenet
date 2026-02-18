# Scenario Format

Tenet simulation scenarios are TOML files that describe the network topology, node behaviour, and
relay configuration. Ready-made scenarios live in `scenarios/`.

See [simulation.md](simulation.md) for the execution model and how to run the simulator.

## Top-Level Structure

A scenario file has these top-level sections:

```toml
[simulation]
# ... node ids, timing, topology, message behaviour ...

[relay]
# ... relay settings ...

[network_conditions]   # optional
# ... network impairment ...
```

`direct_enabled` is an optional top-level boolean. When omitted the simulation uses direct
delivery links in addition to the relay.

## `[simulation]` Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `node_ids` | array of strings | Yes | IDs for simulated peers |
| `duration_seconds` | integer | Preferred | Total simulated duration; derives `steps` automatically |
| `steps` | integer | Fallback | Number of simulation steps (use `duration_seconds` instead) |
| `seed` | integer | Yes | RNG seed for reproducibility |
| `simulated_time` | table | Optional | Time scaling settings |
| `friends_per_node` | table | Yes | Friend graph distribution |
| `post_frequency` | table | Yes | How often each node posts |
| `availability` | table | Optional | Legacy online/offline behavior (use `cohorts` instead) |
| `cohorts` | array of tables | Optional | Per-cohort online behavior |
| `message_size_distribution` | table | Yes | Message size generator |
| `message_type_weights` | table | Optional | Relative weights for direct/public/group messages |
| `clustering` | table | Optional | Dense subgraphs with sparse cross-links |
| `encryption` | table | Optional | Payload encryption mode (default: encrypted) |
| `groups` | table | Optional | Group creation and membership |
| `reaction_config` | table | Optional | Reply/reaction probability and delay |
| `friend_request_config` | table | Optional | Dynamic friend-graph construction |

### `simulated_time`

```toml
[simulation.simulated_time]
seconds_per_step = 300
default_speed_factor = 1.0
```

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
total_posts = 20
```

When `weights` contains 24 entries, they are treated as hourly weights over a simulated day.

### `availability` (legacy)

Prefer `cohorts` instead. Still supported for backward compatibility.

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

### `cohorts`

Cohorts are the preferred way to model diverse online behaviour across the peer population.

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
hourly_weights = [0.2, 0.15, 0.1, 0.1, 0.15, 0.3, 0.5, 0.7, 0.9, 1.0, 1.0, 0.95,
                  0.9, 0.8, 0.75, 0.8, 0.9, 1.0, 0.95, 0.8, 0.6, 0.45, 0.3, 0.25]
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

### `message_type_weights`

Controls the relative proportion of direct, public, and group messages. Values are normalised at
runtime so only relative magnitudes matter. Defaults to direct-only.

```toml
[simulation.message_type_weights]
direct = 3.0
public = 1.0
group = 1.0
```

### `encryption`

Controls payload encryption for Direct messages. Defaults to `encrypted` when omitted, matching
real protocol behaviour.

```toml
[simulation.encryption]
type = "encrypted"   # default — HPKE per-recipient encryption
```

```toml
[simulation.encryption]
type = "plaintext"   # debug only
```

### `clustering`

```toml
[simulation.clustering]
cluster_sizes = [8, 8, 8, 12]
inter_cluster_friend_probability = 0.02
```

Clusters are assigned in order from `node_ids`. The `cluster_sizes` list is consumed in order,
with any remaining nodes appended as a final cluster. `friends_per_node` is sampled within each
cluster. `inter_cluster_friend_probability` controls the chance of creating a cross-cluster
connection for each node pair from different clusters.

### `groups`

Configures automatic group creation and membership assignment.

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

### `reaction_config`

When set, receiving a message triggers an automatic reply after a configurable delay. Replies are
real `MessageSend` events that appear in delivery metrics and can themselves trigger further
replies, producing cascading conversation threads.

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

### `friend_request_config`

Controls dynamic friend-graph construction. When absent, the full friend graph is active from
simulation start. When present, only `initial_friend_fraction` of the planned friendships are
active at `t=0`; the rest are established via `FriendRequest` / `FriendAccept` meta messages.

```toml
[simulation.friend_request_config]
initial_friend_fraction = 0.5   # 50% of friendships start active
request_rate_per_hour = 2.0     # each pending pair gets ~one request per 30 min

[simulation.friend_request_config.acceptance_delay]
type = "uniform"
min = 300.0    # 5 minutes
max = 1800.0   # 30 minutes
```

`acceptance_delay` uses the same distribution variants as `reply_delay_distribution`.

## `[network_conditions]` Fields

Controls network impairment applied to every envelope the harness posts to the relay. When absent
all messages are delivered instantly with no drops.

```toml
[network_conditions]
drop_probability = 0.1          # 10% of all envelopes silently dropped

[network_conditions.direct_latency]
type = "log_normal"
mean = 0.5
std_dev = 0.4

[network_conditions.relay_post_latency]
type = "uniform"
min = 0.05
max = 0.5

[network_conditions.relay_fetch_latency]
type = "uniform"
min = 0.05
max = 0.3
```

- `drop_probability` applies to **all** message kinds including meta (Online/Ack).
- `direct_latency`, `relay_post_latency`, `relay_fetch_latency` use the same distribution
  variants as `reply_delay_distribution`. All default to `fixed { seconds: 0.0 }`.

## `[relay]` Fields

```toml
[relay]
ttl_seconds = 3600
max_messages = 1000
max_bytes = 5242880
retry_backoff_seconds = [1, 2, 4]
peer_log_window_seconds = 60    # optional
peer_log_interval_seconds = 30  # optional
```

Relay TTLs must stay within protocol bounds (1s minimum, 7 days maximum).

## Complete Example: Store-and-Forward

This scenario exercises peer-assisted store-and-forward with three nodes. When node B is offline,
node A wraps the message for storage at node C; C forwards it once B comes online.

```toml
[simulation]
node_ids = ["node-a", "node-b", "node-c"]
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
weights = [0.2, 0.15, 0.1, 0.1, 0.15, 0.3, 0.5, 0.7, 0.9, 1.0, 1.0, 0.95,
           0.9, 0.8, 0.75, 0.8, 0.9, 1.0, 0.95, 0.8, 0.6, 0.45, 0.3, 0.25]
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
hourly_weights = [0.2, 0.15, 0.1, 0.1, 0.15, 0.3, 0.5, 0.7, 0.9, 1.0, 1.0, 0.95,
                  0.9, 0.8, 0.75, 0.8, 0.9, 1.0, 0.95, 0.8, 0.6, 0.45, 0.3, 0.25]

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
