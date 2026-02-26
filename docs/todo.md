# Tenet — Todo / Open Work

This file tracks genuine implementation gaps, deferred features, and ideas for future development.
Items are organised by area. Completed items are noted inline.

Sources: code `// TODO` comments, `docs/relay_qos.md`, `docs/public_payload_fix.md`,
`docs/public_mesh_plan.md`, `docs/groups.md`, `docs/clients/android.md`, and codebase review.

---

## Relay

### Relay QoS (designed, not implemented)

`docs/relay_qos.md` contains a complete design. None of it is in the codebase yet.

- [ ] **Bidirectionality tier system** — classify `(sender, recipient)` pairs as
  Unknown / Acknowledged / Active based on whether both directions have been observed; add
  `directed_last_seen` and `bidirectional_pairs` to `RelayStateInner`.
- [ ] **Per-sender-per-inbox quotas** — reject envelopes with HTTP 429 when a sender exceeds
  their tier's count or byte limit; add `sender_inbox_usage: HashMap<(String, String), SenderUsage>`
  and update it on enqueue, drain, and TTL expiry.
- [ ] **Size-filtered inbox fetch** — add optional `?max_size_bytes=N` query param to
  `GET /inbox/:recipient_id` and `POST /inbox/batch`; skipped envelopes remain queued.
- [ ] **Two-pass client sync** — web client sync loop should fetch small envelopes first (protocol
  / text), then fetch large ones (attachments) in a background pass.
- [ ] **Relay dashboard: per-sender usage** — extend the `/` dashboard to show per-inbox sender
  breakdown using the `sender_inbox_usage` map.
- [ ] **`RelayQosConfig` in `RelayConfig`** — expose tier limits via environment variables so
  operators can tune without recompiling.

### Relay persistence

- [ ] **Optional persistent storage** — the relay currently holds all envelopes in memory; a
  restart drops everything. An optional SQLite (or flat-file) backend would improve reliability
  for small hosted deployments where peers connect infrequently. This is a significant design
  decision (adds a disk dependency) and should be opt-in via config.

---

## Protocol / Security

### Public message payload authentication (Protocol V2)

`docs/public_payload_fix.md` describes the attack and proposed fix. Not yet implemented.

- [ ] **Add `payload_hash` to `CanonicalHeader`** — replace `payload_size` in the signed bytes
  with `SHA256(payload.body)` for V2 messages; keeps `payload_size` in the header for stream
  sizing but removes it from the signature input.
- [ ] **Protocol V2 version enum** — add `ProtocolVersion::V2` and dispatch
  `canonical_signing_bytes` on version.
- [ ] **New `HeaderError` variants** — `MissingPayloadHash` and `PayloadHashMismatch` for V2
  validation errors.
- [ ] **Backward compatibility** — V1 messages (no `payload_hash`) must continue to verify; V2
  senders include the hash; receivers accept both.

### Delivery acknowledgments

- [ ] **Optional read-receipt Meta message** — currently a sender has no protocol-level way to
  know whether their message was received or read. An opt-in `MetaMessage::ReadReceipt
  { message_id, reader_id }` (mirroring the existing `reaction` pattern) would enable delivery
  indicators in the UI. Should be opt-in to preserve privacy for users who prefer not to send
  receipts.

---

## Groups

### Key rotation on member removal

- [ ] **Re-key group after member removal** — `leave_group_handler` and
  `remove_group_member_handler` in `src/web_client/handlers/groups.rs` (lines 387 and 408) both
  have `// TODO: Implement key rotation for remaining members`. Until this is done, a removed
  member who retained the old group key can decrypt all future messages. Fix requires:
  generating a new group key, incrementing `key_version`, and distributing the new key to every
  remaining member via an encrypted Direct message.

### Group name vs group ID

- [ ] **Separate display name from `group_id`** — `group_id` is currently used as both the
  unique identifier and the human-readable display name throughout the codebase, the storage
  schema, and the protocol wire format. Splitting them requires a schema migration (new
  `display_name` column in `groups`), a `group_name` field in `GroupInvite` / `GroupKeyDistribution`
  meta messages, and a protocol version bump. The `group_name` field already exists in the
  `GroupInvite` JSON (it mirrors `group_id`) but is not independently mutable.

### Inviting non-friends

- [ ] **Allow inviting peers not yet in the local registry** — the current group invite flow
  requires the invitee to already be in the `peers` table. Sending an invite to someone you only
  know by Peer ID (before they've accepted a friend request) would require looking up their
  encryption key via a new discovery mechanism or storing an "unresolved peer" state.

### Multi-device group key

- [ ] **Propagate group key to additional devices** — if a user accepts a group invite on device
  A, device B does not automatically receive the group key. There is no protocol message for
  device-to-device key sync; this is blocked by the same multi-device / signing-key portability
  gap described below.

---

## Web Client

### Friend Groups UI

- [ ] **Add-member UI** — REST endpoint `POST /api/groups/:id/members` exists but there is no
  button or form in the SPA. (`docs/clients/web.md` Feature Status: "Partial").
- [ ] **Remove-member UI** — REST endpoint `DELETE /api/groups/:id/members/:peer_id` exists but
  is not exposed in the SPA.
- [ ] **Leave-group UI** — REST endpoint `POST /api/groups/:id/leave` exists but is not exposed
  in the SPA.

### Mesh distribution enhancements

- [ ] **Push-based `MeshAvailable`** — currently peers only respond to `MessageRequest` queries
  sent by others. Peers could proactively broadcast `MeshAvailable` to friends when they come
  online, triggering catch-up without waiting for the periodic `MESH_QUERY_INTERVAL_SECS`. Noted
  as out-of-scope in `docs/public_mesh_plan.md`; revisit when the pull-based flow is stable.

---

## CLI (`tenet` binary)

- [x] **Peer registry not shared with web client** — CLI now uses the same `resolve_identity()`
  system as the web client; `add-peer` writes to the identity's SQLite `peers` table and all
  commands read peers from SQLite. Both clients share the same database.
- [x] **Limited to Direct messages** — added `post <message> [--relay <url>]` for public posts
  and `receive-all [--relay <url>]` to sync all message kinds (direct, public, group).
- [x] **JSONL message storage** — received messages are stored via `store_received_envelope` into
  the SQLite `messages` table; sent envelopes go into the SQLite `outbox` table. The CLI and web
  client now share the same message history.

---

## Simulation and Debugger

### Mesh protocol in simulation

- [x] **Simulation client mesh catch-up** — `SimulationClient.handle_inbox` now handles
  `MessageRequest` / `MeshAvailable` / `MeshRequest` / `MeshDelivery` directly via the new
  `handle_mesh_meta()` method when no external `MessageHandler` is registered. The simulation
  harness already routes outgoing envelopes through the relay, so all four phases are exercised
  end-to-end in simulation scenarios.

### Debugger REPL mesh queries

- [x] **Wire mesh queries into `tenet-debugger`** — added `mesh-query <peer-a> <peer-b>` REPL
  command. The command posts a `MessageRequest` from peer-a to peer-b then drives five sync
  rounds to complete the full four-phase exchange, printing each step's outcome.

### Doc comment updates

- [x] **Update `CLAUDE.md` and `Client` trait doc comments** — `CLAUDE.md` now contains a
  "Public-Message Mesh Catch-up Protocol" section with a phase table, implementation notes, and
  pointers to each implementation. `MessageHandler.on_meta` doc comment lists all four mesh
  variants and their expected response envelopes.

---

## Android Client

- [ ] **Verify `cargo-ndk` build for all three ABIs** — `docs/clients/android.md` Phase 1
  checklist: building `arm64-v8a`, `armeabi-v7a`, and `x86_64` targets requires the Android NDK
  and has not been verified in CI.
- [ ] **Generate and verify Kotlin bindings** — `uniffi-bindgen generate` and JNI linkage in a
  stub Android project has not been confirmed to work end-to-end. (`docs/clients/android.md`
  Phase 1, unchecked items)
- [ ] **Android Phase 2 and beyond** — the roadmap in `docs/clients/android.md` lists group
  messaging UI, QR code peer discovery, profile screens, and Keystore-backed key protection as
  later phases. These depend on Phase 1 NDK verification being complete.

---

## Identity / Multi-device

- [ ] **Signing key portability** — `import-key` in `src/bin/tenet-crypto.rs` generates fresh
  Ed25519 signing keys instead of importing them. A peer using the same X25519 identity on two
  devices therefore has different signing keys per device; signatures from device A cannot be
  verified on device B. There is no documented or implemented path to carry signing keys across
  devices. Resolution options: (a) include Ed25519 keys in the exportable key format, (b) derive
  the Ed25519 key deterministically from the X25519 private key, or (c) introduce a device
  sub-key attestation scheme. Option (b) is the simplest but changes the key derivation model.

---

## New Ideas (from codebase review)

These are not tracked elsewhere and emerged from examining the current design:

- [ ] **Content-addressed relay attachment cache** — attachments are embedded inline in every
  message copy (base64 in the HPKE payload). For a group of N members, the same image is sent N
  times through the relay. A separate relay endpoint (`POST /blobs`, `GET /blobs/:hash`) with
  content-addressed storage would let clients upload once and reference by hash; recipients fetch
  the blob on demand. This is a significant protocol change but would substantially reduce relay
  bandwidth for media-heavy groups. Privacy implication: the relay would know the content hash of
  each blob (not the content itself, which would remain encrypted), and could correlate blob
  retrieval with recipient identities.

- [ ] **Relay-side envelope deletion API** — after a client successfully fetches its inbox,
  envelopes sit in the relay until TTL expiry (default 3600 s). Adding `DELETE
  /inbox/:recipient_id/messages/:message_id` (authenticated by a signed token) would let clients
  free relay quota immediately after confirmed receipt, rather than waiting for TTL. This helps
  on constrained relays with small `max_messages` limits and would complement the QoS tier
  system.

- [x] **QR-code / short-code peer discovery for web client** — the Android client doc mentions
  QR code peer discovery as a planned feature. The web client currently requires manually copying
  and pasting a 64-character hex public key and a peer ID. A web-client-native QR code display
  (for `#/profile`) and a QR scanner (via `getUserMedia`) would make key exchange practical for
  co-located users without the Android app. The QR payload would encode the peer ID and
  encryption public key in a URI scheme (e.g. `tenet://peer/<peer_id>?key=<pubkey_hex>`).
  Implemented: `GET /api/qr` returns an SVG QR code (via the `qrcode` crate); the profile
  page (`#/profile`) displays it inline; the Friends view has a "Scan QR" button that opens a
  camera modal using `getUserMedia` + `BarcodeDetector`; the scanner auto-fills the friend
  request form on success; a manual URI paste fallback handles unsupported browsers.
