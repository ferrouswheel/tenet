# Flow: Relay WebSocket Push

The relay exposes a WebSocket endpoint (`/ws/{recipient_id}`) that pushes
incoming envelopes to connected clients in real time. This eliminates polling
latency when a peer is actively online.

The web client opens **two** WebSocket connections:
1. **Relay WS** — to the relay server (`/ws/{peer_id}`), for real-time envelope push.
2. **Local WS** — to the web server (`/api/ws`), for UI updates (new messages, peer status, etc.).

## Connection and Push Flow

```mermaid
sequenceDiagram
    participant UI as Browser UI
    participant WS as Web Server (local WS)
    participant Sync as Sync Loop (background task)
    participant Relay as Relay Server
    participant Sender as Sending Peer

    rect rgb(235, 245, 255)
        Note over UI,WS: Startup – open local WebSocket
        UI->>WS: GET /api/ws (upgrade)
        WS-->>UI: 101 Switching Protocols
    end

    rect rgb(240, 255, 240)
        Note over Sync,Relay: Startup – open relay WebSocket
        Sync->>Relay: GET /ws/{peer_id}?token=<auth>
        Relay-->>Sync: 101 Switching Protocols
        Sync->>Relay: {"type":"hello","version":"…"}
    end

    rect rgb(255, 245, 230)
        Note over Sender,Relay: Sender posts a message
        Sender->>Relay: POST /envelopes → this peer
        Relay->>Relay: Store envelope in inbox
        Relay->>Sync: Push envelope over WebSocket
    end

    rect rgb(255, 240, 255)
        Note over Sync,WS: Sync loop wakes and processes
        Sync->>Sync: Notify wakes relay_sync_loop immediately<br/>(no wait for polling interval)
        Sync->>Relay: GET /inbox/{peer_id}
        Relay-->>Sync: [new envelope(s)]
        Sync->>Sync: Verify signature
        Sync->>Sync: Decrypt payload
        Sync->>Sync: Store in SQLite
        Sync->>WS: WsEvent::NewMessage (broadcast channel)
        WS->>UI: {"type":"new_message", "message_id":"…", …}
        UI->>UI: Display message immediately
    end
```

## Auth Token for Relay WebSocket

The relay WebSocket endpoint requires a short-lived authentication token to
prevent unauthorized inbox access.

```mermaid
sequenceDiagram
    participant C as Client
    participant R as Relay

    Note over C: Generate auth token
    C->>C: token = peer_id + "." + unix_ts + "." + Ed25519_sig<br/>sig covers: "tenet-relay-auth\n{peer_id}\n{timestamp}"

    C->>R: GET /ws/{peer_id}?token={token}
    R->>R: Parse token: extract peer_id, timestamp, sig
    R->>R: Verify timestamp within ±60 seconds
    R->>R: Verify Ed25519 signature
    alt Valid
        R-->>C: 101 Switching Protocols
    else Invalid or expired
        R-->>C: 401 Unauthorized
    end
```

## Reconnect with Backoff

If the relay WebSocket disconnects, the client reconnects with exponential
backoff (2 s, 4 s, 8 s … up to 60 s). Polling continues as a fallback during
the disconnected interval.

```mermaid
sequenceDiagram
    participant C as Client
    participant R as Relay

    C->>R: connect
    R-->>C: disconnect (network error)

    loop Reconnect attempts
        Note over C: Wait 2s → 4s → 8s … (max 60s)
        C->>R: reconnect attempt
        alt Success
            R-->>C: 101 Switching Protocols
            Note over C: backoff resets to 2s
        else Failure
            Note over C: double backoff, retry
        end
    end

    Note over C: Periodic polling continues<br/>as fallback while WS is down
```

## WebSocket Events (Local UI Channel)

The web server broadcasts the following events to the browser over `/api/ws`:

| Event | Trigger |
|---|---|
| `new_message` | New message stored (any kind) |
| `message_read` | Message marked as read |
| `peer_online` | `MetaMessage::Online` received |
| `peer_offline` | Peer marked offline (timeout) |
| `friend_request_received` | Incoming friend request |
| `friend_request_accepted` | Outgoing request accepted |
| `group_invite_received` | Incoming group invite |
| `group_member_joined` | Member accepted group invite |
| `profile_updated` | Profile data received for a peer |
| `relay_status` | Relay connected / disconnected |
| `notification` | Any notifiable event |
