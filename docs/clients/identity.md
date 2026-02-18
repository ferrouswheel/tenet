# Identity Management

Tenet supports multiple identities within a single data directory. Each identity is an independent
keypair with its own SQLite database, peer registry, and relay configuration. This enables running
separate personas from one machine without intermingling their state.

## Directory Layout

```
{TENET_HOME}/
  config.toml               ← default_identity = "<short_id>"
  identities/
    {short_id}/
      tenet.db              ← keypair, peers, messages, relays
    {short_id}/
      tenet.db
      peers.json            ← CLI peer registry (tenet binary only)
      inbox.jsonl           ← CLI received messages
      outbox.jsonl          ← CLI sent envelopes
```

The short ID is the first 12 characters of the base64-encoded peer ID (derived from the X25519
public key). It is used as the directory name and is unique enough in practice.

## Identity Resolution

When any binary starts, `resolve_identity()` selects which identity to use by following this
priority order:

1. **Explicit selection** — `--identity <prefix>` flag or `TENET_IDENTITY=<prefix>` environment
   variable. A prefix match is used, so `--identity abc` matches the directory `abc123def456`.
2. **Config default** — `default_identity` field in `{TENET_HOME}/config.toml`.
3. **Only one exists** — if a single identity directory is present, it is used automatically.
4. **Create new** — if no identities exist, a fresh keypair is generated, stored, and set as the
   default in `config.toml`.

If multiple identities exist and none of the above criteria match, the binary exits with an
"ambiguous" error listing available short IDs.

## Managing Identities

### Viewing the Current Identity

```bash
# Web client: shown in /api/health
curl http://localhost:3000/api/health

# CLI
cargo run --bin tenet -- export-key --public
```

### Creating Additional Identities

```bash
# Each `init` without --identity creates a new one
cargo run --bin tenet -- init           # identity 1 (auto-selected if only one)
cargo run --bin tenet -- init           # identity 2 (now ambiguous — must select)

# Select by prefix
cargo run --bin tenet -- --identity abc12 export-key --public
```

The `tenet-web` binary also creates a new identity on first run if none exist.

### Setting a Default

Edit `{TENET_HOME}/config.toml`:

```toml
default_identity = "abc123def456"
```

Or use `TENET_IDENTITY=abc123def456` as an environment variable to override the config default
for a single invocation.

### Rotating a Keypair

The CLI supports `rotate-key`, which generates a new keypair, creates a new identity directory,
copies the peer list, and sets the new identity as the default:

```bash
cargo run --bin tenet -- rotate-key
# identity rotated from <old_id> to <new_id>
# (share the new public key with peers)
```

The old identity directory is preserved. The old keypair can still be selected with
`--identity <old_prefix>`.

## Legacy Migration

Older builds stored the database directly in `{TENET_HOME}/tenet.db` (no `identities/`
subdirectory). On first run with a new build, `migrate_legacy_layout()` automatically moves
`{TENET_HOME}/tenet.db` into `identities/{short_id}/tenet.db` and records the new default in
`config.toml`. No manual intervention is required.

The CLI also migrates old `identity.json` + `keypair.json` JSON files from earlier prototypes via
`migrate_legacy_json_to_identities()`.

## Multi-Device Usage

There is currently no built-in synchronization mechanism for operating a single Tenet identity
across multiple devices. Each device that runs `tenet-web` or the CLI will have its own keypair.

To share an identity across devices, you can manually copy the keypair material:

1. On the source device, export the keypair:
   ```bash
   cargo run --bin tenet -- export-key --public   # copy this
   cargo run --bin tenet -- export-key --private  # copy this (keep it secret)
   ```
2. On the destination device, import it:
   ```bash
   cargo run --bin tenet -- import-key <pub_hex> <priv_hex>
   ```
   Note: new Ed25519 signing keys are generated automatically on import. The signing keys are not
   portable; any signatures from the original device will not be verifiable on the new device.

Because signing keys differ, the two devices are not fully equivalent participants in the network.
This is a known limitation. See [`docs/to_check_later.md`](../to_check_later.md) for status.
