tenet
=====

> **NOTICE** - Since I wrote this, I became aware of [Secure Scuttlebutt](https://github.com/ssbc/secure-scuttlebutt)
> I totally recommend checking that out, as if I have the time to contribute to a p2p social fabric, then
> I'll probably work with something people are already using instead of taking Tenet any further.

A protocol concept for a peer-to-peer social network and a Rust reference implementation.

## Documentation

* Architecture and design notes live in [docs/architecture.md](docs/architecture.md).
* MVP scope lives in [docs/mvp.md](docs/mvp.md).

## Build

```bash
cargo build
```

## Run

Run the CLI:

```bash
cargo run --bin tenet-crypto -- init
cargo run --bin tenet-crypto -- add-peer <name> <public_key_hex>
```

Run the relay service (defaults to `0.0.0.0:8080`):

```bash
cargo run --bin relay
```

You can also configure the relay via environment variables:

```bash
TENET_RELAY_BIND=127.0.0.1:8080 \
TENET_RELAY_TTL_SECS=3600 \
TENET_RELAY_MAX_MESSAGES=1000 \
TENET_RELAY_MAX_BYTES=5242880 \
  cargo run --bin relay
```

Run the toy UI (spawns N peers, defaults to 3):

```bash
cargo run --bin toy_ui -- --peers 4 --relay http://127.0.0.1:8080
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

The simulation harness is implemented as an integration test. Run it directly with:

```bash
cargo test --test simulation_harness_tests
```
