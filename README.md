tenet
=====

> **NOTICE** - Since I wrote this, I became aware of [Secure Scuttlebutt](https://github.com/ssbc/secure-scuttlebutt)
> I totally recommend checking that out, as if I have the time to contribute to a p2p social fabric, then
> I'll probably work with something people are already using instead of taking Tenet any further.

A protocol concept for a peer-to-peer social network and a Rust reference implementation.

## Documentation

* Architecture and design notes live in [docs/architecture.md](docs/architecture.md).
* MVP scope lives in [docs/architecture.md](docs/architecture.md).

## Build

```bash
cargo build
```

## Run

Run the CLI:

```bash
cargo run --bin tenet -- init
cargo run --bin tenet -- add-peer <name> <public_key_hex>
```

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
TENET_RELAY_PEER_LOG_WINDOW_SECS=60 \
TENET_RELAY_PEER_LOG_INTERVAL_SECS=30 \
  cargo run --bin tenet-relay
```

Run the toy UI (spawns N peers, defaults to 3):

```bash
cargo run --bin tenet-debugger -- --peers 4 --relay http://127.0.0.1:8080
```

Once running, try:

```bash
peers
send peer-1 peer-2 hello there
sync peer-2
feed peer-2
offline peer-3
online peer-3
```

## Tests

```bash
cargo test
```

## Simulation

Scenario-driven simulation runs are documented in [docs/simulation.md](docs/simulation.md) and
sample scenarios live in `scenarios/`. Run one with:

```bash
cargo run --bin tenet-sim -- scenarios/small_dense_6.toml
```

The simulation harness is also implemented as an integration test. Run it directly with:

```bash
cargo test --test simulation_harness_tests
```
