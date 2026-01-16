# Tenet Architecture & Design Notes

Tenet is designed around modern mobile reality: devices are frequently on the move, often behind NATs,
connections are intermittent, and delivery is best-effort rather than guaranteed. The goal is to share
social updates through a mesh of trusted peers without bespoke cryptography or centralized control.

## Core Ideas

* **Peer distribution**: friends and friends-of-friends relay updates, including content they cannot read.
* **Best-effort delivery**: nodes keep a rolling window of recent updates; missing data is acceptable.
* **Privacy by default**: payloads are encrypted end-to-end using established primitives.
* **Replaceable transports**: the protocol tolerates different network paths, including relays.
* **Mobile-first reality**: peers may not be directly reachable (NAT, carrier networks, sleep modes).

## Cryptographic Model

* **No bespoke crypto**: use well-vetted libraries and standard constructions.
* **Per-recipient encryption**: messages are encrypted separately for each recipient or via standard group
  keying if the library supports it.
* **Authenticated encryption**: every payload is integrity protected and bound to sender identity.
* **Key exchange**: peers exchange long-term public keys during a friendship/peering handshake.

## Threat Model

Tenet aims to reduce centralized metadata collection, but it does not prevent powerful global adversaries.
It assumes:

* Local adversaries may observe some traffic but not compromise all peers.
* Malicious peers can spam or attempt to infer social graphs.
* Clients must tolerate compromised or offline peers.

## Abuse, Spam, and Limits

* Rate-limit inbound updates and friend requests per peer.
* Require proof-of-work or proof-of-relationship for unsolicited content.
* Enforce size caps and rolling retention to prevent storage exhaustion.
* Allow users to block, mute, or unfriend peers with immediate effect.

## Transport Layer

Tenet relies on a transport abstraction that supports:

* **Store-and-forward relays** for peers behind NAT or on mobile networks.
* **Direct connections** when reachable (LAN, IPv6, or public addresses).
* **Opportunistic discovery** via known peers or DNS hints.

Transports are interchangeable as long as they deliver opaque encrypted blobs and metadata (sender id,
recipient id, timestamp, and message id).

## Data Model

* **User identity**: public key + stable user id.
* **Message**: encrypted payload + metadata header.
* **Feed**: an ordered log of updates per user, truncated by local retention policy.
* **Attachments**: optional blobs referenced by content hash.

### Message Header

Headers include:

* `sender_id`, `recipient_id`, `timestamp`, `message_id`, `ttl_seconds`, `payload_size`
* `message_kind`: one of `public`, `meta`, `direct`, or `friend_group`
* `group_id`: required only when `message_kind` is `friend_group`

Friend-group messages are addressed to a logical group identifier (`group_id`). The payload is
encrypted with a group-scoped key, and recipients treat the message as part of the named group
conversation or feed.

## MVP Feature Scope

* **Identity**: long-term keypairs per user, stable IDs derived from public keys, and a locally stored
  friend/peer list (manual exchange or QR/URL bootstrap).
* **Envelope encryption**: per-recipient authenticated encryption with a signed metadata header
  (sender ID, recipient ID, timestamp, message ID, message kind, group ID (if any), TTL, payload size).
* **Relay transport**: store-and-forward relays for NATed/offline peers; relays retain opaque blobs
  with minimal metadata and provide best-effort delivery.
* **Local store**: append-only per-peer feeds with rolling retention and message ID indexing for
  deduplication.
* **TTL enforcement**: messages carry a TTL that bounds relay storage and local retention windows.

## Minimal Protocol Flows

### 1) Send

1. Sender composes payload and selects recipients.
2. For each recipient:
   * Encrypt payload with recipient key (or group key if supported).
   * Construct header: sender ID, recipient ID, timestamp, message ID, message kind, group ID (if
     friend-group), TTL, payload size.
   * Sign header.
3. Write encrypted message to local outbox and feed.
4. Submit envelope to relay (or direct peer if available).

### 2) Relay

1. Relay accepts envelope and stores it with minimal metadata.
2. Relay enforces TTL and size caps; expired envelopes are dropped.
3. Recipient polls relay (or relay pushes if supported) to fetch envelopes.

### 3) Receive

1. Recipient fetches envelopes from relay.
2. Validate header signature, message kind, and sender ID.
3. Check TTL; discard if expired.
4. Decrypt payload and append to local feed.
5. Update dedup index with message ID.

### 4) Dedup

1. Before storing, check message ID against local index.
2. If already present, discard duplicate envelope.
3. Keep latest-seen metadata (e.g., most recent relay source) for diagnostics.

## Storage and TTL Constraints

* **Local feed retention**: rolling window by size and time (e.g., last N MB or N days).
* **Attachment cache**: fixed-size LRU, keyed by content hash.
* **Relay quotas**:
  * Per-recipient storage cap (e.g., max total bytes per user).
  * Per-sender rate limits to mitigate spam.
  * TTL enforced; expired data is removed without notice.
  * Relay TTLs are intentionally short (default 3600s in the simulation scenarios) and should
    remain within protocol bounds (1s minimum, 7 days maximum).
* **Index bounds**: dedup index pruned alongside feed compaction.

## Getting Started (Rust)

Minimal crates for a Rust MVP:

* `tokio` for async I/O and task scheduling.
* `serde` + `serde_json` for message framing prior to encryption.
* `ed25519-dalek` or `ring` for signatures and key handling.
* `chacha20poly1305` or `aes-gcm` for authenticated encryption.
* `libp2p` (optional) or a simple relay client for initial transport.

Suggested modules:

* `crypto/`: key management, encryption/decryption, signatures.
* `transport/`: relay client, direct sockets, retry/backoff.
* `store/`: local feed storage, retention policy, attachment cache.
* `protocol/`: message types, serialization, validation.

First steps:

1. Define message types and metadata headers.
2. Implement key generation and a simple handshake.
3. Build an encrypted payload format with authenticated encryption.
4. Add a relay transport with best-effort send/receive.
5. Persist a rolling feed and enforce size/time retention.

## Non-Goals

* Global availability or guaranteed delivery.
* A novel cryptographic scheme.
* Public blockchain or consensus-driven identity.
* Long-term archival of all content.

## Status

This repository is a design sketch and prototype; it is not production-ready.

## Summary

Tenet is a peer-distributed, mobile-aware social protocol that favors simplicity,
best-effort delivery, and standard cryptography. The system assumes intermittent
connectivity, favors replaceable transport layers, and keeps local history short
to reduce risk and storage costs.
