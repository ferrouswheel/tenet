# Flow: Peer Reconnect

When a peer comes back online (after a restart or a period of being offline) it
performs several actions to re-join the network: it announces its presence to
known peers, fetches any queued messages from the relay, and issues mesh queries
to catch up on public content it may have missed.

## Startup Sequence

```mermaid
sequenceDiagram
    participant P as Peer (coming online)
    participant R as Relay
    participant F1 as Friend A
    participant F2 as Friend B

    Note over P: Peer starts up / reconnects

    rect rgb(235, 245, 255)
        Note over P,R: 1 – Online announcement
        loop For each known peer
            P->>P: Build MetaMessage::Online<br/>{peer_id, timestamp}
            P->>P: Sign envelope header
            P->>R: POST /envelopes<br/>{kind: Meta, recipient: Friend}
            R->>R: Store in friend's inbox
        end
    end

    rect rgb(240, 255, 240)
        Note over P,R: 2 – Open WebSocket for real-time push
        P->>R: GET /ws/{peer_id}?token=<auth>
        R-->>P: 101 Switching Protocols
        P->>R: {"type":"hello", "version":"..."}
        Note over P,R: Relay pushes new envelopes immediately<br/>instead of waiting for next poll
    end

    rect rgb(255, 245, 230)
        Note over P,R: 3 – Poll inbox for queued messages
        P->>R: GET /inbox/{peer_id}
        R-->>P: [all queued envelopes]
        loop For each envelope
            P->>P: Check dedup index
            P->>P: Verify Ed25519 signature
            P->>P: Decrypt payload (HPKE + ChaCha20)
            P->>P: Store in SQLite
            P->>P: Broadcast WsEvent to browser
        end
    end

    rect rgb(255, 240, 255)
        Note over P,R: 4 – Mesh queries for missed public content
        loop For each known peer (rate-limited)
            P->>P: Build MetaMessage::MessageRequest<br/>{peer_id, since_timestamp}
            P->>R: POST /envelopes → Friend A
        end
    end

    rect rgb(245, 235, 255)
        Note over P,R: 5 – Profile refresh requests
        loop For peers with stale/missing profiles
            P->>P: Build MetaMessage::ProfileRequest
            P->>R: POST /envelopes → Friend
        end
    end

    Note over F1: Receives Online MetaMessage
    F1->>F1: Update peers.online = true<br/>Record last_seen_online timestamp
    F1->>F1: Broadcast WsEvent::PeerOnline to browser

    Note over F2: Also receives Online MetaMessage
    F2->>F2: Update peer status
```

## WebSocket-Accelerated Delivery

The relay WebSocket connection (`/ws/{peer_id}`) allows near-real-time message
delivery without waiting for the next polling interval.

```mermaid
sequenceDiagram
    participant S as Sender
    participant R as Relay
    participant P as Peer (online)
    participant UI as Browser UI

    S->>R: POST /envelopes (new message)
    R->>R: Store in Peer's inbox
    R->>P: Push notification over WebSocket
    Note over P: Wakes sync loop immediately
    P->>R: GET /inbox/{peer_id}
    R-->>P: [new envelope]
    P->>P: Decrypt + store
    P->>UI: WsEvent::NewMessage
    UI->>UI: Display message
```

## Sync Loop Backoff

If the relay is unreachable the sync loop backs off exponentially (30 s, 60 s,
120 s … up to 5 minutes). Once reconnected it broadcasts
`WsEvent::RelayStatus { connected: true }` to update the browser UI.
