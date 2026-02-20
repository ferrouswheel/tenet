# Flow: Public Message

Public messages are timeline posts visible to peers who follow the author.
Unlike direct messages they are **not HPKE-encrypted** (they are public by
intent), but the header is still Ed25519-signed so recipients can verify
authenticity.

Public messages are distributed in two ways:
1. **Direct delivery** — posted to each known friend's inbox at send time.
2. **Mesh backfill** — peers share messages with each other on reconnect; see
   [mesh-distribution.md](07-mesh-distribution.md).

## Sending a Public Post

```mermaid
sequenceDiagram
    participant B as Browser
    participant S as Sender (web server)
    participant R as Relay

    B->>S: POST /api/messages/public<br/>{body}

    rect rgb(235, 245, 255)
        Note over S: Build envelope
        S->>S: Build Payload:<br/>{content_type: "text/plain", body}
        S->>S: Compute message_id = SHA-256(payload)
        S->>S: Build Header:<br/>{sender, recipient: friend,<br/> kind: Public, timestamp, ttl}
        S->>S: Sign header with Ed25519
    end

    loop For each known friend
        S->>R: POST /envelopes<br/>{kind: Public, recipient: Friend_N}
        R->>R: Store in friend's inbox
        R-->>S: 200 OK
    end

    S->>S: Store in local SQLite (own feed)
    S-->>B: 201 Created {message_id}
```

## Receiving a Public Post

```mermaid
sequenceDiagram
    participant R as Relay
    participant P as Peer (sync loop)
    participant DB as SQLite
    participant UI as Browser UI

    alt WebSocket push
        R->>P: Push Public envelope
    else Periodic poll
        P->>R: GET /inbox/{peer_id}
        R-->>P: [envelopes including Public ones]
    end

    loop For each Public envelope
        P->>P: Check dedup index
        P->>P: Verify Ed25519 signature<br/>using sender's stored signing key
        P->>P: Check sender not muted/blocked
        P->>P: Check TTL
        P->>DB: INSERT message (kind=public, body, sender, …)
        P->>UI: WsEvent::NewMessage
        UI->>UI: Show in timeline feed
    end
```

## Notes

- Public message bodies are stored as plaintext in SQLite on the recipient's
  device.
- If the sender's signing key is not yet known (e.g. message arrived via mesh
  from a non-friend), signature verification is deferred until the key is
  obtained (see [peer-discovery.md](08-peer-discovery.md)).
- Peers that the author does not know about will only receive public messages
  via the [mesh distribution](07-mesh-distribution.md) protocol.
