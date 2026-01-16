Simulation scenarios
====================

Simulation scenarios are TOML files consumed by `tenet-sim`. Each scenario has a `[simulation]`
section that describes the network behavior and a `[relay]` section that configures the in-process
relay used during the run. You can find ready-made scenarios in `scenarios/`.

## Top-level layout

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
When node B is offline, node A will wrap the message for storage at node C and C will forward it
once B comes online.

```toml
[simulation]
node_ids = ["node-a", "node-b", "node-c"]
steps = 20
seed = 7

[simulation.friends_per_node]
type = "uniform"
min = 2
max = 2

[simulation.post_frequency]
type = "weighted_schedule"
weights = [1, 1, 1, 2, 1]
total_posts = 6

[simulation.availability]
type = "markov"
p_online_given_online = 0.7
p_online_given_offline = 0.2
start_online_prob = 0.4

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
- `steps` (integer): number of simulation steps.
- `seed` (integer): RNG seed.
- `friends_per_node`: friend graph distribution (see below).
- `post_frequency`: how often each node posts (see below).
- `availability`: online/offline behavior (see below).
- `message_size_distribution`: message size generator (see below).
- `clustering` (optional): cluster layout for dense subgraphs with sparse cross-links.
- `encryption` (optional): message payload handling (`plaintext` or `encrypted`).

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
lambda_per_step = 0.4
```

```toml
[simulation.post_frequency]
type = "weighted_schedule"
weights = [1, 1, 2, 3, 5]
# total posts per node
total_posts = 20
```

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
