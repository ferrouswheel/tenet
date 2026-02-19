# Relay Quality-of-Service Design

## Problem Statement

The relay is a store-and-forward server that routes envelopes between peers.
Because it is intentionally ignorant of block lists (see `docs/peerlist_plan.md
§3.3`), a malicious or misconfigured peer can:

1. **Flood a recipient's inbox** — send thousands of small messages, making it
   expensive to drain the queue and crowding out legitimate traffic.
2. **Saturate storage with large payloads** — send a handful of oversized
   envelopes (e.g. images) that consume the per-peer storage budget, preventing
   smaller protocol and text messages from arriving.
3. **Starve other senders** — because the relay uses global FIFO eviction, one
   bad actor can push out messages from all other senders to a given recipient.

The relay must defend against these without:
- Learning the recipient's block list (privacy leak).
- Learning the content of messages (all payloads are encrypted).
- Requiring sender registration or any out-of-band trust anchor.
- Breaking the best-effort delivery contract for legitimate traffic.

---

## What the Relay Already Knows

Every envelope header is visible in plaintext:

| Field | Used for |
|---|---|
| `header.sender_id` | Rate limiting per sender |
| `header.recipient_id` | Routing; per-inbox limits |
| `header.ttl_seconds` | Expiry enforcement (already done) |
| `header.timestamp` | Expiry enforcement (already done) |
| `payload.body.len()` | Already tracked as `size_bytes` in `StoredEnvelope` |

The relay already maintains `peer_sends: HashMap<String, VecDeque<Instant>>`
(rolling 24 h timestamps per sender) and tracks `size_bytes` per stored
envelope.  QoS builds on top of these.

---

## Design

### 1. Bidirectionality Tier

The core insight is that two peers that are actively replying to each other are
almost certainly having a legitimate conversation.  A peer that only sends (and
never receives a reply) looks more like spam.

The relay observes all envelopes and can therefore determine whether a
(sender, recipient) pair has ever been reversed — i.e. whether the recipient
has also sent a message *to* the sender.  This is the **bidirectionality
signal**.

Define three tiers:

| Tier | Condition | Intent |
|---|---|---|
| **1 — Unknown** | Sender has never received a message from this recipient (no reply ever observed) | Treat conservatively |
| **2 — Acknowledged** | Recipient has sent at least one envelope to sender (at any point in relay history) | Active conversation; relax limits |
| **3 — Active** | Bidirectional traffic observed within the last 7 days | Ongoing relationship; most permissive |

**Privacy note:** The relay already sees `(sender_id, recipient_id)` for every
envelope — the communication graph is an unavoidable consequence of the
store-and-forward model.  Tracking *bidirectionality* adds only the weaker
observation "these two peers have both sent to each other", which is already
implied by two one-way edges in the graph.  This is substantially less
revealing than content or metadata correlation.  Users should only use relay
operators they trust; this is documented in the threat model.

#### Implementation

Add to `RelayStateInner`:

```rust
// Set of (sender_id, recipient_id) pairs where both directions have been
// observed: entry (A, B) means "A has sent to B AND B has sent to A".
// Only the canonical ordering (lexicographic min first) is stored.
bidirectional_pairs: HashSet<(String, String)>,

// Per-direction last-seen timestamps for bidirectionality expiry.
// Key: (sender_id, recipient_id), Value: most recent send Instant.
directed_last_seen: HashMap<(String, String), Instant>,
```

When storing an envelope from sender S to recipient R:
1. Record `directed_last_seen[(S, R)] = now`.
2. Check whether `directed_last_seen[(R, S)]` exists and is within 7 days.
   - If yes: insert `(min(S,R), max(S,R))` into `bidirectional_pairs`.

`get_tier(sender, recipient)` returns:
- **Tier 3** if bidirectional pair exists and both directed timestamps < 7 days.
- **Tier 2** if `directed_last_seen[(recipient, sender)]` exists (at any age).
- **Tier 1** otherwise.

---

### 2. Per-Sender-Per-Inbox Limits

Each (sender, recipient) pair gets a quota applied *at store time*.  When a new
envelope from S to R would exceed the quota, it is rejected with `429 Too Many
Requests` before being enqueued.

#### Suggested default limits

| Tier | Max envelopes in inbox | Max bytes in inbox |
|---|---|---|
| 1 — Unknown | 20 | 256 KB |
| 2 — Acknowledged | 100 | 2 MB |
| 3 — Active | 500 | 10 MB |

"In inbox" means currently queued, not lifetime.  Limits are re-checked as
messages are delivered: once the recipient drains some messages, the sender
can queue more.

These values are tunable via `RelayConfig` so operators can adjust for their
use case.

#### Implementation

Add to `RelayStateInner`:

```rust
// Tracks, per (sender_id, recipient_id), how many envelopes and how many
// bytes from that sender currently sit in the recipient's queue.
// Updated on enqueue and on drain.
sender_inbox_usage: HashMap<(String, String), SenderUsage>,
```

```rust
struct SenderUsage {
    count: usize,
    bytes: usize,
}
```

On enqueue from S to R:
- Look up `sender_inbox_usage[(S, R)]` and compute tier.
- If `count >= tier_limit.max_count || bytes + new_size > tier_limit.max_bytes`:
  reject with 429.
- Otherwise increment count and bytes.

On drain (messages popped from queue):
- Decrement the `sender_inbox_usage` entry for the original sender.
- The sender ID is already available in `StoredEnvelope` (add a `sender_id:
  String` field alongside existing fields).

On TTL expiry:
- Same: decrement usage when an expired envelope is pruned.

#### Global inbox byte limit (existing behaviour, improved eviction)

The relay already has a global `max_bytes` per inbox.  When it must evict to
make room, the current code pops from the front (oldest first, regardless of
sender).  Change eviction to **evict from the sender currently using the most
bytes in this inbox**.  This protects small, timely messages from other senders
when one sender has accumulated a large backlog.

---

### 3. Attachment / Large-Payload Prioritisation

Rather than maintaining separate physical queues (complex, stateful), expose
a **size filter on the inbox fetch endpoint**:

```
GET /inbox/:recipient_id?limit=50&max_size_bytes=8192
```

The relay returns only envelopes whose `size_bytes ≤ max_size_bytes`.  Larger
envelopes remain in the queue until the client fetches without the filter (or
with a higher threshold).

#### Client sync strategy (two-pass)

1. **Pass 1 — Protocol + text**: fetch with `max_size_bytes=8192` (or similar
   configurable threshold).  Gets Online/Meta/friend-request/text messages
   immediately.  Fast; low bandwidth; unblocks the UI.
2. **Pass 2 — Bulk**: fetch without `max_size_bytes` (or with a large value) to
   drain remaining large-payload envelopes in the background.

This achieves progressive loading without requiring the relay to maintain
separate queues or inspect payload content beyond the already-tracked
`size_bytes`.

#### Implementation

In `fetch_inbox` and `fetch_inbox_batch`, add an optional `max_size_bytes`
query param.  In `pop_envelopes`, skip (leave in queue) envelopes whose
`size_bytes` exceeds the threshold.

```rust
#[derive(Deserialize)]
struct InboxQuery {
    limit: Option<usize>,
    max_size_bytes: Option<usize>,   // new
}
```

**Important:** envelopes skipped by the size filter are not popped and not
counted as delivered.  They stay in the queue until a fetch without the filter
collects them.  TTL expiry still applies.

#### Why not separate queues?

A separate "bulk queue" per sender-recipient would require the relay to decide
at store time which queue an envelope belongs to — either by size threshold or
by a `priority` header field.  If by size, senders could work around it by
splitting payloads.  If by a header field, senders would mark everything as
priority.  Client-side filtering at fetch time avoids both problems and is
simpler to implement and reason about.

---

### 4. Configuration

Extend `RelayConfig` with:

```rust
pub struct RelayQosConfig {
    /// Per-sender-per-inbox limits by tier.
    pub tier1: TierLimits,
    pub tier2: TierLimits,
    pub tier3: TierLimits,
    /// How long bidirectional activity must be absent before downgrading to Tier 2.
    pub bidirectional_window: Duration,  // default: 7 days
}

pub struct TierLimits {
    pub max_envelopes: usize,
    pub max_bytes: usize,
}
```

Sensible defaults:

```rust
tier1: TierLimits { max_envelopes: 20,  max_bytes: 256 * 1024 },
tier2: TierLimits { max_envelopes: 100, max_bytes: 2 * 1024 * 1024 },
tier3: TierLimits { max_envelopes: 500, max_bytes: 10 * 1024 * 1024 },
bidirectional_window: Duration::from_secs(7 * 24 * 3600),
```

The relay operator may set tier limits to `usize::MAX` to disable enforcement.

---

### 5. API Response Changes

When an envelope is rejected by QoS, return:

```
HTTP 429 Too Many Requests
{"error": "sender quota exceeded for this inbox"}
```

The sender's client should back off and retry after TTL expiry has freed space.
No information about the recipient's tier assignment or quota usage is exposed
to the sender (to avoid probing attacks: a sender should not be able to infer
whether the recipient has also been communicating with them).

---

### 6. Privacy Audit

| Data tracked | Already tracked? | New risk |
|---|---|---|
| `sender_id` per envelope | Yes | None |
| `recipient_id` per envelope | Yes | None |
| Envelope `size_bytes` | Yes | None |
| `directed_last_seen[(S,R)]` | Partially (`peer_sends` per sender, not per pair) | Weak: reveals "S has sent to R" — already implied by routing |
| Bidirectionality `(A,B)` pair | No | Weak: reveals "A and B have exchanged messages" — already deducible from two directed edges |
| Content, message kind, encryption keys | No | Not accessed |

Conclusion: the QoS data structures add no meaningful new information beyond
what a relay operator could already deduce by inspecting routing headers.  The
same caveat applies as for all relay deployments: users should use relay
operators they trust.  The threat model assumes a curious but not actively
malicious relay operator.

---

### 7. Interaction with Client-Side Blocking

Client-side blocking (`peerlist_plan.md §3.1`) and relay QoS are complementary:

- **Relay QoS** limits how much a bad actor can *store* in the relay on behalf
  of a victim.  It reduces bandwidth and storage cost before the client ever
  fetches.
- **Client-side blocking** silently discards matching messages *after* fetching.
  The client still pays the fetch cost, but Tier 1 limits keep that cost small.

Together they bound the attack surface: an unknown sender can queue at most
20 messages / 256 KB before being rate-limited, and even those are discarded
on receipt if the client has blocked that peer.

---

## Implementation Order

1. Add `sender_id` to `StoredEnvelope` (trivial; already available at store time).
2. Add `sender_inbox_usage` tracking with per-enqueue / per-drain updates.
3. Add `directed_last_seen` and `bidirectional_pairs` with tier computation.
4. Wire tier limits into `store_envelope_locked()`.
5. Add `max_size_bytes` query param to `fetch_inbox` / `fetch_inbox_batch`.
6. Update `RelayConfig` with `RelayQosConfig` and expose in the debug stats endpoint.
7. Update client sync loop to do two-pass fetch (text-first, then bulk).
8. Update relay dashboard to show per-inbox sender usage breakdown.

---

## Out of Scope

- Sender authentication / allowlists (would require relay-side registration).
- Per-recipient configurable limits sent by the recipient to the relay (leaks
  social graph; not aligned with privacy model).
- Content-based filtering at the relay (payloads are encrypted).
- Punishing senders globally across all recipients (would allow one recipient
  to retaliate against a sender that is legitimate elsewhere).
