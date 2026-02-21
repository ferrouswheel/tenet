# CLAUDE.md

This file provides guidance for AI assistants working with the Tenet codebase.

## Project Overview

Tenet is a Rust reference implementation of a peer-to-peer social network protocol designed for mobile-first, decentralized communication. Key design principles:

- Best-effort delivery with intermittent connectivity
- End-to-end encryption using standard cryptographic primitives (HPKE, ChaCha20Poly1305)
- Store-and-forward relays for NAT traversal
- Minimal trust assumptions with no bespoke cryptography
- Privacy by default

**Note:** This is a prototype/reference implementation, not production-ready.

## Build & Test Commands

```bash
# Build
cargo build                    # Debug build
cargo build --release          # Release build

# Test
cargo test                     # Run all tests

# Lint & Format (run before committing)
cargo fmt                      # Format code
cargo clippy                   # Run linter
```

## Web UI Build Process

The web application UI is automatically built during `cargo build` via the `build.rs` script:

1. Source files are in `web/src/`:
   - `index.html` - HTML template with `{{STYLES}}` and `{{SCRIPTS}}` placeholders
   - `styles.css` - CSS styles
   - `app.js` - JavaScript application code
   - `relay_dashboard.html` - Standalone relay status dashboard (not inlined; served separately)

2. The build script inlines CSS and JS into the HTML template and outputs `web/dist/index.html`

3. The built file (`web/dist/index.html`) is excluded from git (see `.gitignore`)

**Note:** You should NEVER edit `web/dist/index.html` directly. Edit the source files in `web/src/` instead, and the build script will regenerate the dist file automatically on the next build.

## Binary Targets

```bash
# HTTP relay server (defaults to 0.0.0.0:8080)
cargo run --bin tenet-relay

# CLI key management and messaging tool
cargo run --bin tenet -- init
cargo run --bin tenet -- add-peer <name> <public_key_hex>
cargo run --bin tenet -- send <peer> <message> --relay http://...

# Interactive REPL debugger (self-contained; starts its own in-process relay)
cargo run --bin tenet-debugger -- --peers 4
# Or with an external relay:
cargo run --bin tenet-debugger -- --peers 4 --relay http://127.0.0.1:8080

# Simulation scenario runner
cargo run --bin tenet-sim -- scenarios/small_dense_6.toml
cargo run --bin tenet-sim -- --tui scenarios/small_dense_6.toml

# Web client server (REST API + WebSocket + embedded SPA)
cargo run --bin tenet-web
```

## Architecture

See `docs/architecture.md` for detailed design and threat model documentation.

### Core Modules

| Module | Purpose |
|--------|---------|
| `src/protocol.rs` | Message types (`Envelope`, `Header`, `ContentId`), protocol version, TTL validation |
| `src/crypto.rs` | HPKE key encapsulation, ChaCha20Poly1305 encryption, keypair management, `validate_hex_key()` |
| `src/relay_transport.rs` | Client-side relay HTTP helpers: `post_envelope()`, `fetch_inbox()` |
| `src/client.rs` | `RelayClient` (HTTP), `SimulationClient` (in-process), `Client` trait |
| `src/relay.rs` | Axum-based HTTP relay server with TTL enforcement and deduplication |
| `src/identity.rs` | Multi-identity management, config, legacy migration |
| `src/storage.rs` | SQLite persistence layer (messages, peers, groups, profiles, etc.) |
| `src/groups.rs` | Group messaging: `GroupInfo`, `GroupManager`, membership |
| `src/logging.rs` | Structured logging: `tlog!` macro, colour-coded peer/message IDs, configurable writer |
| `src/message_handler.rs` | `StorageMessageHandler` implementing `MessageHandler` trait for protocol-level persistence |
| `src/web_client/` | Web server module (REST API + WebSocket + embedded SPA) |
| `src/simulation/` | Simulation harness, scenario configuration, metrics tracking |

### Web Client Submodules (`src/web_client/`)

The `tenet-web` binary delegates to `tenet::web_client::run()`. The module is organized as:

| File | Purpose |
|------|---------|
| `mod.rs` | Entry point `run()`: CLI parsing, identity resolution, server startup |
| `config.rs` | `Cli` (clap), `Config`, constants (`DEFAULT_TTL_SECONDS`, etc.) |
| `state.rs` | `AppState`, `SharedState` (`Arc<Mutex<AppState>>`), `WsEvent` enum |
| `router.rs` | `build_router()` — all Axum route definitions |
| `sync.rs` | Background relay sync loop, envelope processing, online announcements |
| `static_files.rs` | Embedded SPA serving via `rust-embed` |
| `utils.rs` | `api_error()`, `message_to_json()`, `link_attachments()`, `now_secs()`, `SendAttachmentRef` |
| `handlers/health.rs` | `GET /api/health`, `POST /api/sync` (manual sync trigger) |
| `handlers/messages.rs` | Message CRUD + send direct/public/group, mark read |
| `handlers/peers.rs` | Peer CRUD + activity tracking, blocking, muting |
| `handlers/friends.rs` | Friend request lifecycle (send, list, accept, ignore, block) |
| `handlers/groups.rs` | Group CRUD + symmetric key distribution |
| `handlers/attachments.rs` | Multipart upload + content-addressed download |
| `handlers/reactions.rs` | Upvote/downvote reactions |
| `handlers/replies.rs` | Threaded replies to public/group messages |
| `handlers/profiles.rs` | Profile management + broadcasting to friends |
| `handlers/conversations.rs` | Direct message conversation listing |
| `handlers/group_invites.rs` | Group invite lifecycle (list, send, accept, reject) |
| `handlers/notifications.rs` | Notification listing and mark-read |
| `handlers/websocket.rs` | WebSocket upgrade + broadcast connection |

### Simulation Submodules

| File | Purpose |
|------|---------|
| `simulation/mod.rs` | `SimulationHarness` orchestration and message routing |
| `simulation/config.rs` | TOML-serializable scenario configuration types |
| `simulation/scenario.rs` | Graph building, schedule generation, scenario execution |
| `simulation/event.rs` | `Event` types, `EventQueue`, `EventLog`, `SimulationClock`, `TimeControlMode` |
| `simulation/metrics.rs` | Latency tracking, delivery statistics, aggregation |
| `simulation/random.rs` | Distribution sampling (Poisson, Zipf, etc.) |

## Key Types

- `ContentId` - SHA256-based content addressing with base64 URL-safe encoding
- `Envelope` - Complete message: header + encrypted payload + attachments
- `Header` - Metadata: sender, recipient, timestamp, TTL, signature, message kind
- `MessageKind` - `Public`, `Meta`, `Direct`, `FriendGroup`, `StoreForPeer`
- `RelayClient` / `SimulationClient` - Client implementations (both implement `Client` trait)

## Coding Conventions

### Error Handling

Use custom error enums implementing `std::error::Error`:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MyError {
    InvalidInput(String),
    CryptoFailure,
}

impl std::fmt::Display for MyError { ... }
impl std::error::Error for MyError { ... }
```

Existing error types: `CryptoError`, `HeaderError`, `EnvelopeBuildError`, `ClientError`, `PayloadCryptoError`, `KeyStoreError`

### Derive Macros

- Data types: `#[derive(Debug, Clone, Serialize, Deserialize)]`
- Internal state: `#[derive(Debug, Clone)]`
- Collection keys: add `PartialEq, Eq, Hash`

### Naming

- Types: `CamelCase`
- Functions/variables: `snake_case`
- Constants: `UPPER_SNAKE_CASE`
- Function prefixes: `build_*`, `sample_*`, `record_*`, `is_*`

### Module Visibility

- Public API: `pub` exports in `lib.rs`
- Internal sharing: `pub(crate)` for cross-module access

## Testing

- **Unit tests**: Embedded in source files with `#[test]`
- **Integration tests**: `tests/` directory
- **Scenario fixtures**: `scenarios/*.toml`

Test files:
- `tests/protocol_tests.rs` - Message serialization, signatures
- `tests/crypto_tests.rs` - Key generation, encryption/decryption
- `tests/relay_tests.rs` - HTTP relay endpoints
- `tests/simulation_client_tests.rs` - Client behavior
- `tests/simulation_harness_tests.rs` - Multi-peer scenarios
- `tests/client_sync_tests.rs` - `SyncEvent`/`MessageHandler` trait, `sync_inbox()` integration

## Async/Threading Model

- **Relay server**: Tokio async runtime with `Arc<Mutex<T>>` for shared state
- **Clients**: Synchronous (no async in client implementations)
- **Simulation**: Single-threaded, step-based execution

## Key Dependencies

- **Crypto**: `hpke`, `chacha20poly1305`, `sha2`, `rand`
- **Networking**: `axum`, `tokio`, `ureq`
- **Serialization**: `serde`, `serde_json`, `toml`
- **TUI**: `ratatui`, `crossterm`, `rustyline`

## Android Client

The `android/` directory contains a separate Android client. It is **not** part of the main Cargo workspace — it has its own build system.

### Structure

```
android/
├── build.sh                    # Build script (compiles FFI + assembles APK)
├── tenet-ffi/                  # Rust→Kotlin bridge (UniFFI)
│   ├── Cargo.toml              # Separate Rust crate
│   ├── build.rs                # UniFFI bindgen step
│   ├── src/lib.rs              # TenetClient: synchronous Rust API, Mutex-protected
│   ├── src/types.rs            # FFI-safe type definitions (FfiMessage, FfiPeer, etc.)
│   └── src/tenet_ffi.udl       # UniFFI interface definition
└── app/                        # Android Jetpack Compose application
    └── app/src/main/java/com/example/tenet/
        ├── data/
        │   ├── TenetRepository.kt      # Singleton wrapping TenetClient FFI
        │   ├── KeystoreManager.kt      # Android Keystore integration
        │   ├── SyncWorker.kt           # Background WorkManager sync
        │   └── TenetPreferences.kt     # DataStore preferences
        └── ui/                         # Compose screens + ViewModels
            ├── conversations/          # Direct message conversations
            ├── compose/                # Message composition
            ├── friends/                # Friend request management
            ├── groups/                 # Group messaging
            ├── peers/                  # Peer management
            ├── timeline/               # Public message feed
            ├── profile/                # User profile
            ├── qr/                     # QR code peer discovery
            └── setup/                  # First-run setup
```

### Key Design Points

- The Android app uses the **UniFFI FFI bridge** (not the web REST API) for all protocol operations.
- `TenetClient` (Rust) wraps all mutable state behind a `Mutex`; Kotlin dispatches calls to `Dispatchers.IO`.
- Sync opens a second SQLite connection (WAL mode) to avoid holding the main lock during network I/O.
- The FFI crate depends on the main `tenet` library crate.

## Pre-Commit Checklist

Before committing changes:

1. `cargo fmt` - Format code
2. `cargo build` - Ensure compilation
3. `cargo test` - Run all tests
4. `cargo clippy` - Check for lints
