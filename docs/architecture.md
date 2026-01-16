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
