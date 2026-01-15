# Tenet MVP

This document captures the minimal viable product (MVP) scope for Tenet: the smallest set of features
and protocol flows required to validate the concept in a real-world, mobile-first environment.

## MVP Feature List

* **Identity**
  * Long-term public/private keypair per user.
  * Stable user ID derived from the public key.
  * Friend/peer list stored locally (manual exchange or QR/URL bootstrap).
* **Envelope Encryption**
  * Per-recipient encryption of payloads using authenticated encryption.
  * Metadata header includes sender ID, recipient ID, timestamp, message ID, and payload size.
  * Signature over the header to bind sender identity and payload integrity.
* **Relay Transport**
  * Store-and-forward relay client for peers behind NAT or offline.
  * Relay stores opaque encrypted blobs with minimal metadata (sender, recipient, message ID, TTL).
  * Best-effort delivery; no guarantee of relay availability or storage duration.
* **Local Store**
  * Append-only feed per peer with rolling retention.
  * Indexed by message ID for fast deduplication.
  * Optional attachment cache keyed by content hash.
* **TTL (Time-to-Live)**
  * Each message carries a TTL that limits relay storage and local retention.
  * Clients enforce TTL expiry on fetch and during local compaction.

## Non-Goals and Constraints

* **No global availability**: delivery is best-effort; missing updates are acceptable.
* **No bespoke cryptography**: only standard, audited primitives.
* **No public indexing**: no global search or discovery beyond known peers/relays.
* **No long-term archival**: history is short and capped by TTL/size.
* **No consensus or blockchain**: identity and ordering are local and per-peer.
* **Mobile-first constraints**: intermittent connectivity, NAT traversal via relays only.

## Minimal Protocol Flows

### 1) Send

1. Sender composes payload and selects recipients.
2. For each recipient:
   * Encrypt payload with recipient key (or group key if supported).
   * Construct header: sender ID, recipient ID, timestamp, message ID, TTL, payload size.
   * Sign header.
3. Write encrypted message to local outbox and feed.
4. Submit envelope to relay (or direct peer if available).

### 2) Relay

1. Relay accepts envelope and stores it with minimal metadata.
2. Relay enforces TTL and size caps; expired envelopes are dropped.
3. Recipient polls relay (or relay pushes if supported) to fetch envelopes.

### 3) Receive

1. Recipient fetches envelopes from relay.
2. Validate header signature and sender ID.
3. Check TTL; discard if expired.
4. Decrypt payload and append to local feed.
5. Update dedup index with message ID.

### 4) Dedup

1. Before storing, check message ID against local index.
2. If already present, discard duplicate envelope.
3. Keep latest-seen metadata (e.g., most recent relay source) for diagnostics.

## Storage Constraints and Quotas

* **Local feed retention**: rolling window by size and time (e.g., last N MB or N days).
* **Attachment cache**: fixed-size LRU, keyed by content hash.
* **Relay quotas**:
  * Per-recipient storage cap (e.g., max total bytes per user).
  * Per-sender rate limits to mitigate spam.
  * TTL enforced; expired data is removed without notice.
* **Index bounds**: dedup index pruned alongside feed compaction.

## References

* README overview and design context: [../README.md](../README.md)
