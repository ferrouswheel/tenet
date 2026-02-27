# Attachment v2: Resilient and Scalable Blob Distribution

## Current System and Its Limitations

The v1 attachment design works for small files but breaks down at scale:

| Symptom | Root cause |
|---|---|
| 10 MB per-attachment cap | Entire blob must fit in a single HPKE-encrypted envelope |
| Group of N copies the same blob N times | Each recipient gets a separate encrypted copy in their inbox |
| Public attachment unavailable when sender is offline | Recipient fetches directly from sender's web server |
| No resumable transfer | Dropped upload/download starts from scratch |
| Relay memory spikes on media | Relay holds the full blob in memory while routing |

For short videos (50–300 MB), multi-page PDFs, or voice notes, these limits make the protocol unusable.

---

## Design Constraints

Any replacement must respect Tenet's core properties:

- **E2E confidentiality** — the relay (and any caching peers) must not learn plaintext content.
- **Integrity** — recipients must verify they got exactly what the sender intended.
- **Best-effort / intermittent connectivity** — peers are often offline; the system must tolerate that.
- **Minimal trust** — no central authority; no bespoke cryptography.
- **Mobile-first** — solutions requiring always-on servers or large DHT participation are impractical.

---

## Proposals

### Option A — Chunked Blob Relay (simplest extension)

Extend the existing relay with a dedicated blob endpoint. Blobs are split into fixed-size chunks on the client before upload; the relay stores and serves individual chunks by content hash.

#### Protocol sketch

1. **Sender** splits the file into 256 KB chunks, computes `SHA-256(chunk)` for each, then encrypts each chunk independently with ChaCha20Poly1305 using a single random blob key.
2. Sender uploads each encrypted chunk: `POST /blobs/<sha256hex>` → relay stores it.
3. Sender builds a **blob manifest** — a small JSON document listing the blob key (HPKE-wrapped for the recipient), the ordered list of chunk hashes, total size, and content type.
4. The manifest is placed in the existing `Envelope.payload.attachments` field instead of inline data. Manifests are tiny (< 1 KB even for 1 GB files) and continue to travel through the normal relay inbox.
5. **Recipient** receives the envelope, decrypts the manifest, then fetches each chunk: `GET /blobs/<sha256hex>`.
6. Chunks are reassembled, decrypted, and integrity-verified.

#### Relay endpoint additions

```
POST /blobs/:sha256hex          Upload an encrypted chunk
GET  /blobs/:sha256hex          Download a chunk (unauthenticated; content is opaque)
DELETE /blobs/:sha256hex        Sender-authenticated deletion (optional)
```

Chunks are content-addressed, so the relay can deduplicate across all senders. The relay learns only the hash of each encrypted chunk — it cannot correlate chunks with their plaintext content.

#### Properties

| Property | Assessment |
|---|---|
| Relay privacy | Relay sees encrypted chunks only; no plaintext |
| Redundancy | Poor — relay is still single point of failure |
| Group efficiency | Good — sender uploads one encrypted blob, all recipients fetch same chunks |
| Implementation complexity | Low — small protocol addition, no new dependencies |
| Mobile friendliness | Good — small chunks allow resumable transfers |
| File size limit | Configurable; 1 GB is practical |
| Offline sender | Poor — chunks disappear when relay TTLs expire |

#### Trade-offs

**Pro:** Fastest to implement; works with the existing relay infrastructure; does not require peers to run any additional software.

**Con:** Relay is still a single point of failure for blob availability. If chunks expire before a recipient comes online, they are lost. Requires relay operators to provision blob storage separately (or S3/object-store backend).

---

### Option B — Peer-Hosted Content-Addressed Blobs (BitTorrent-inspired)

Each peer who has downloaded a blob can serve it to others. The protocol is inspired by BitTorrent's piece exchange but uses the existing relay as a signalling channel rather than a DHT tracker.

#### Protocol sketch

1. **Sender** produces the same chunk-and-manifest structure as Option A. The manifest additionally includes a list of `seeder_id`s (initially just the sender).
2. The manifest travels via the normal envelope path.
3. **Recipient** fetches chunks directly from any listed seeder via HTTP (if online) or via the relay blob endpoint (as a fallback).
4. A new `MetaMessage` variant — `BlobAnnounce { blob_id, chunk_hashes, seeder_id }` — lets any peer who has finished downloading announce itself as a seeder to the message's group/recipients. Senders update the manifest in subsequent messages or on request.
5. A `MetaMessage::BlobRequest { blob_id, chunk_index, requester_id }` lets a peer ask specific seeders for individual chunks.

#### Seeder selection

Recipients pick seeders using a simple prioritisation:
1. Peers currently online (detected via `MetaMessage::Online`)
2. Peers with the most chunks (announced via `BlobAnnounce`)
3. Relay blob fallback

#### Properties

| Property | Assessment |
|---|---|
| Relay privacy | Same as Option A — relay sees only encrypted chunks |
| Redundancy | Good — multiple seeders, degrades gracefully |
| Group efficiency | Excellent — after one member downloads, they seed to others |
| Implementation complexity | Medium — new meta messages, seeder tracking, piece selection |
| Mobile friendliness | Moderate — seeders must be online; mobile devices sleep |
| File size limit | Unlimited in principle |
| Offline sender | Good — once any peer has the blob, the sender need not be online |

#### Trade-offs

**Pro:** Availability improves as group size grows; reduces relay bandwidth for popular content; no central blob server needed.

**Con:** Seeder availability is not guaranteed on mobile; requires peers to expose an HTTP endpoint (or relay-forwarded fetch). Adds protocol complexity (new meta messages, seeder state). Mobile peers behind NAT need the relay as intermediary, reducing the P2P benefit unless hole-punching or relay-forwarded byte-range fetches are implemented.

---

### Option C — IPFS / libp2p Content-Addressed Storage

Use IPFS (or a subset of libp2p) as the blob transport layer. Files are chunked into IPFS blocks (typically 256 KB), assembled into a Merkle DAG, and addressed by their root CID (Content Identifier). Tenet envelopes carry only the CID plus an HPKE-wrapped decryption key.

#### Protocol sketch

1. **Sender** adds the file to a local IPFS node (or a pinning service): `ipfs add <file>` → returns a CID.
2. Sender wraps the symmetric encryption key and CID in the envelope attachment ref (replacing the raw hash).
3. **Recipient** resolves the CID via any IPFS gateway or local node: `ipfs get <CID>`.
4. Decryption key is unwrapped from the HPKE-wrapped field; file is decrypted.

#### Encryption model

IPFS blocks are public by default. Two approaches:

- **Symmetric pre-encryption:** Encrypt the file before adding to IPFS. The CID addresses the ciphertext; only the key matters for confidentiality. Simple but prevents IPFS-level deduplication across users (different keys → different CIDs even for identical plaintexts).
- **IPFS + private networks:** Use an IPFS private swarm restricted to Tenet peers. More complex setup; adds a network layer.

#### Properties

| Property | Assessment |
|---|---|
| Relay privacy | Relay does not participate; IPFS nodes see only encrypted blocks |
| Redundancy | Excellent — IPFS is designed for redundancy |
| Group efficiency | Excellent — CID is universal; any IPFS node caches it |
| Implementation complexity | High — IPFS daemon/library dependency; cross-platform support on mobile is immature |
| Mobile friendliness | Poor — running an IPFS node on iOS/Android is impractical today |
| File size limit | Unlimited |
| Offline sender | Good — as long as any pinning node holds the blocks |

#### Trade-offs

**Pro:** Battle-tested; global CDN-like properties; free pinning services (web3.storage, Pinata, etc.) provide persistence.

**Con:** Adds a heavyweight external dependency. Mobile IPFS support is poor — `go-ipfs` does not run on iOS; `rust-libp2p` is promising but immature for full IPFS compatibility. Users would need a pinning service account for resilience, reintroducing a trusted third party. The encryption-before-add approach forfeits IPFS deduplication.

---

### Option D — Tiered Attachment Protocol (Recommended Hybrid)

Rather than replacing the current system entirely, introduce a **tiered strategy** where the attachment transport is selected based on file size and context. The `AttachmentRef` struct gains a `transport` field to signal which mechanism to use.

#### Tiers

| Tier | Size threshold | Transport | Privacy |
|---|---|---|---|
| `inline` | < 256 KB | Base64 inside HPKE payload (current v1) | Full E2E, relay-opaque |
| `relay_blob` | 256 KB – 50 MB | Chunked upload to relay blob endpoint (Option A) | Relay sees encrypted chunks |
| `peer_seeded` | > 50 MB | Peer-seeded chunk exchange (Option B) | Relay sees encrypted chunks; peers serve each other |

The thresholds are configurable by the sender and can be overridden by relay capability advertisements.

#### Wire format extension

```rust
// New field on AttachmentRef (backwards-compatible: defaults to Inline)
pub enum AttachmentTransport {
    Inline,                          // v1 behaviour
    RelayBlob {
        relay_url: String,           // base URL of the blob endpoint
        chunk_hashes: Vec<String>,   // ordered SHA-256 hashes of encrypted chunks
    },
    PeerSeeded {
        blob_id: String,             // root hash of the full blob
        chunk_hashes: Vec<String>,
        seeders: Vec<String>,        // peer_ids known to have the blob
        relay_fallback: String,      // relay blob URL as fallback
    },
}
```

Because `AttachmentRef` is serialized inside the HPKE-encrypted payload for direct and group messages, the relay learns nothing about which tier is in use or which chunks belong to which message.

#### Manifest security

The manifest (chunk list + blob key) travels inside the existing HPKE envelope — it is encrypted end-to-end and signed by the sender. The relay blob endpoint only serves opaque ciphertext chunks. Recipients verify `SHA-256(encrypted_chunk) == chunk_hash` before decryption.

#### Chunk encryption

Each chunk is encrypted with the same symmetric blob key using a per-chunk nonce derived as:

```
nonce_i = BLAKE3(blob_key || chunk_index_le64)[:12]
```

This avoids storing a nonce per chunk while preventing nonce reuse.

#### Relay blob endpoint design

```
POST /blobs
  Body: { chunk_hash: "<sha256hex>", data_b64: "<base64 encrypted chunk>" }
  Auth: Signed by sender (same Ed25519 key used for envelopes)
  Response: 201 Created | 409 Conflict (already exists)

GET /blobs/:sha256hex
  Auth: None required (content is opaque ciphertext; hash is self-authenticating)
  Response: 200 raw bytes | 404 Not Found

DELETE /blobs/:sha256hex
  Auth: Signed by original uploader
  Response: 204 | 403 Forbidden
```

Relay enforces a per-sender daily upload quota (aligning with the existing QoS tier system in `docs/relay_qos.md`).

#### Delivery flow for a large direct message attachment

```
Sender                     Relay                     Recipient
  |                          |                           |
  |-- split file into 256KB chunks                       |
  |-- encrypt chunks with blob_key                       |
  |-- POST /blobs/<hash_0> --|                           |
  |-- POST /blobs/<hash_1> --|                           |
  |        ...               |                           |
  |-- build manifest JSON    |                           |
  |-- HPKE encrypt manifest  |                           |
  |-- POST /envelopes -----> |                           |
  |                          |-- GET /inbox/:id -------> |
  |                          |<------------------------- |
  |                          | [encrypted envelope]      |
  |                          |                 decrypt manifest
  |                          |                 verify chunk_hashes
  |                          |-- GET /blobs/<hash_0> --> |
  |                          |-- GET /blobs/<hash_1> --> |
  |                          |        ...                |
  |                          |              reassemble & decrypt
  |                          |              verify SHA-256(plaintext)
```

#### Public message attachment

For public messages, the manifest is in the plaintext payload (like today's attachment ref). Recipients fetch chunks from the relay blob endpoint. This means the relay knows which public messages have attachments, but not their content — consistent with existing public message privacy properties.

#### Seeder gossip (optional future phase)

Once the core chunked relay blob path is solid, add `MetaMessage::BlobAnnounce` to enable peer seeding (Option B behaviour) as an overlay on top of Option D. Peers that have fetched a blob can announce themselves as seeders; future recipients prefer peer-to-peer chunk fetches over relay fetches. This is an additive, backwards-compatible change.

#### Properties

| Property | Assessment |
|---|---|
| Backwards compatibility | Full — `Inline` tier is v1 behaviour; clients negotiate tier transparently |
| Relay privacy | Same or better than v1 for Direct/Group; no worse for Public |
| Redundancy | Moderate (relay blob) → Good (peer seeded) |
| Group efficiency | Excellent — one upload, N downloads |
| Implementation complexity | Medium — new relay endpoints, chunk logic in client; no new dependencies |
| Mobile friendliness | Good — small chunks, resumable; no daemon required |
| File size limit | Configurable; practical at 1 GB with relay blob, unlimited with peer seeding |
| Offline sender | Moderate — relay blob persists until TTL |

---

## Comparison Summary

| | Option A | Option B | Option C | Option D (Recommended) |
|---|---|---|---|---|
| New dependencies | None | None | IPFS/libp2p | None |
| Mobile support | Good | Moderate | Poor | Good |
| Redundancy | Low | High | High | Medium → High |
| Group efficiency | Good | Excellent | Excellent | Excellent |
| Implementation effort | Low | Medium | High | Medium |
| Backwards compatible | Yes | Yes | No | Yes |
| File size ceiling | ~1 GB (relay) | Unlimited | Unlimited | ~1 GB relay, unlimited P2P |

---

## Recommendation and Phasing

**Implement Option D in two phases:**

### Phase 1 — Chunked Relay Blob (3–4 weeks)

1. Add `relay_blob` tier to `AttachmentTransport`; keep `inline` as default.
2. Implement relay `POST /blobs` and `GET /blobs/:hash` endpoints.
3. Add chunk splitting / reassembly and per-chunk encryption in the client.
4. Update `AttachmentRef` (wire format, storage schema).
5. Extend the QoS quota system to cover blob uploads.
6. Update the web client upload handler to use chunked blobs above the 256 KB threshold.
7. Add integration tests covering upload, download, integrity failure, and quota rejection.

### Phase 2 — Peer Seeding Overlay (future)

1. Add `MetaMessage::BlobAnnounce { blob_id, chunk_hashes, seeder_id }`.
2. Add `MetaMessage::BlobRequest { blob_id, chunk_index, requester_id }`.
3. Implement seeder selection logic in the client.
4. Add `peer_seeded` tier to `AttachmentTransport`.
5. Benchmark seeder availability under simulated mobile churn scenarios.

Phase 2 is entirely additive and can be deferred until Phase 1 is validated in production.

---

## Open Questions

- **Blob TTL** — should blob chunks share the TTL of the referencing envelope, or have an independent TTL? An independent TTL (e.g. 30 days) allows recipients who come online late to still fetch the blob after the envelope has expired from the relay.
- **Relay blob quota** — should blob storage be billed per-chunk or per-blob? Per-blob avoids penalising senders for deduplication hits.
- **Public blob encryption** — public message attachments could optionally encrypt blobs with a per-blob key advertised in the plaintext manifest. This preserves the relay's ability to serve blobs without authentication while hiding content from passive observers who only tap the blob endpoint.
- **Chunk size** — 256 KB is a good default (matches IPFS block size; small enough for mobile). Should be tunable per relay.
- **Relay storage backend** — in-memory blob storage is impractical for large files. Phase 1 should allow a configurable backend: in-memory (default, dev), filesystem, or S3-compatible object store.
