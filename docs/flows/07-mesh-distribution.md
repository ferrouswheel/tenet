# Flow: Mesh Distribution of Past Public Messages

Peers share public messages they have seen with each other using a
four-message gossip protocol. This allows a peer to catch up on content it
missed while offline and to receive posts from peers it is not directly
connected to.

The protocol is rate-limited per peer: queries are only sent at most once per
`MESH_QUERY_INTERVAL_SECS` interval.

## Four-Way Gossip Handshake

```mermaid
sequenceDiagram
    participant A as Peer A
    participant R as Relay
    participant B as Peer B

    Note over A: A reconnects / sync loop fires<br/>Wants public messages since timestamp T

    rect rgb(235, 245, 255)
        Note over A,R: Step 1 – A asks B what it has
        A->>A: Build MetaMessage::MessageRequest<br/>{peer_id: A, since_timestamp: T}
        A->>A: Sign envelope header
        A->>R: POST /envelopes → B
        R->>R: Store in B's inbox
    end

    rect rgb(240, 255, 240)
        Note over B,R: Step 2 – B advertises available messages
        B->>R: GET /inbox/{B}
        R-->>B: [MessageRequest from A]
        B->>B: Query local DB for public messages since T
        B->>B: Build MetaMessage::MeshAvailable<br/>{peer_id: B,<br/> message_ids: [id1, id2, …],<br/> since_timestamp: T}
        B->>R: POST /envelopes → A
    end

    rect rgb(255, 245, 230)
        Note over A,R: Step 3 – A requests messages it is missing
        A->>R: GET /inbox/{A}
        R-->>A: [MeshAvailable from B]
        A->>A: Check each message_id against local dedup index
        A->>A: Build MetaMessage::MeshRequest<br/>{peer_id: A,<br/> message_ids: [id1, id3, …]}  ← only missing ones
        A->>R: POST /envelopes → B
    end

    rect rgb(255, 240, 255)
        Note over B,R: Step 4 – B delivers the requested envelopes
        B->>R: GET /inbox/{B}
        R-->>B: [MeshRequest from A]
        B->>B: Look up requested envelopes in local DB
        B->>B: Build MetaMessage::MeshDelivery<br/>{peer_id: B,<br/> envelopes: [env1, env3, …],<br/> sender_keys: {sender_id: signing_key_hex, …}}
        Note over B: sender_keys lets A verify signatures<br/>from peers A has never met
        B->>R: POST /envelopes → A
    end

    rect rgb(235, 255, 245)
        Note over A: Step 5 – A processes new messages
        A->>R: GET /inbox/{A}
        R-->>A: [MeshDelivery from B]
        loop For each delivered envelope
            A->>A: Check dedup index (skip duplicates)
            A->>A: Verify sender's Ed25519 signature<br/>using sender_keys from delivery
            A->>A: Store signing key for future verification
            A->>A: Store message in SQLite
            A->>A: Broadcast WsEvent::NewMessage to browser
        end
    end
```

## Backfilling Signatures

When a message arrives via mesh from a peer A has never added as a friend,
A has no stored signing key for that sender. The `sender_keys` map in
`MeshDelivery` solves this:

```mermaid
sequenceDiagram
    participant A as Peer A
    participant B as Peer B (delivery peer)

    Note over A: Receives MeshDelivery containing<br/>message from unknown Peer C

    A->>A: Check: SHA-256(key_bytes) == sender_id?
    Note over A: Peer ID is derived from public key,<br/>so the key is self-authenticating
    A->>A: Store C's signing key locally
    A->>A: Verify C's Ed25519 signature
    A->>A: Backfill signatures for any<br/>previously unverified messages from C
```

## Scope and Rate Limiting

- Mesh queries use `DEFAULT_MESH_WINDOW_SECS` (e.g. 24 h) as the lookback window.
- Each peer is queried at most once per `MESH_QUERY_INTERVAL_SECS`.
- Only **public** messages are shared via mesh; direct and group messages are
  not included in `MeshDelivery`.
- A peer that has no new messages simply sends `MeshAvailable` with an empty
  list, and A sends no `MeshRequest`.
