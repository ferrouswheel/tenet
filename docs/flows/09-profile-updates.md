# Flow: Profile Updates

Each peer maintains a profile (display name, bio, avatar hash). Profiles are
distributed to friends either **proactively** when a user updates their own
profile, or **reactively** in response to a `ProfileRequest` from another peer.

Profile data is sent as an encrypted Direct message so only the intended
recipient can read it.

## Push: Updating Your Own Profile

```mermaid
sequenceDiagram
    participant B as Browser
    participant S as Server
    participant DB as SQLite
    participant R as Relay

    B->>S: PUT /api/profile<br/>{display_name, bio, avatar_hash?}
    S->>DB: UPSERT profiles (user_id = own_peer_id, …)
    S-->>B: 200 OK {profile}

    rect rgb(235, 245, 255)
        Note over S: Broadcast updated profile to all friends
        S->>DB: SELECT peer_id FROM peers WHERE is_friend = true
        loop For each friend
            S->>S: Build profile payload:<br/>{"type":"tenet.profile",<br/> "user_id": own_id,<br/> "display_name": "…",<br/> "bio": "…",<br/> "avatar_hash": "…"}
            S->>S: Encrypt for friend (HPKE Direct message)
            S->>S: Sign header with Ed25519
            S->>R: POST /envelopes → Friend
            R->>R: Store in friend's inbox
        end
    end
```

## Receive: Incoming Profile Update

```mermaid
sequenceDiagram
    participant R as Relay
    participant P as Peer (sync loop)
    participant DB as SQLite
    participant UI as Browser UI

    P->>R: GET /inbox/{peer_id}
    R-->>P: [Direct envelope containing profile]

    P->>P: Verify Ed25519 signature
    P->>P: Decrypt HPKE payload
    P->>P: Parse JSON body<br/>{type: "tenet.profile", display_name, bio, …}
    P->>DB: UPSERT profiles (user_id = sender_id, …)
    P->>DB: UPDATE peers SET last_profile_responded_at = now()
    P->>UI: WsEvent::ProfileUpdated<br/>{peer_id, display_name, bio, avatar_hash}
    UI->>UI: Refresh peer display name / avatar
```

## Pull: Requesting a Peer's Profile

Profiles are also requested automatically by the background sync loop for peers
whose profile is missing or stale (hourly for missing, daily for stale).

```mermaid
sequenceDiagram
    participant A as Alice (sync loop)
    participant R as Relay
    participant B as Bob

    Note over A: Bob has no profile data yet<br/>OR last_profile_responded_at > 1 hour ago

    rect rgb(235, 245, 255)
        Note over A,R: Alice sends ProfileRequest
        A->>A: Build MetaMessage::ProfileRequest<br/>{peer_id: Alice, for_peer_id: Bob}
        A->>A: Sign envelope header
        A->>R: POST /envelopes → Bob
        R->>R: Store in Bob's inbox
        A->>A: Record last_profile_requested_at = now()
    end

    rect rgb(240, 255, 240)
        Note over B,R: Bob responds with his profile
        B->>R: GET /inbox/{bob_peer_id}
        R-->>B: [ProfileRequest from Alice]
        B->>B: Build own profile payload
        B->>B: Encrypt for Alice (HPKE Direct message)
        B->>R: POST /envelopes → Alice
    end

    rect rgb(255, 245, 230)
        Note over A: Alice receives and stores Bob's profile
        A->>R: GET /inbox/{alice_peer_id}
        R-->>A: [profile Direct message from Bob]
        A->>A: Decrypt and parse profile
        A->>A: UPSERT profiles table
        A->>A: UPDATE peers.last_profile_responded_at
        A->>A: WsEvent::ProfileUpdated → browser
    end
```

## Manual Profile Request

A user can also manually request a peer's profile from the web UI:

```
POST /api/peers/{peer_id}/request-profile
```

This sends an immediate `ProfileRequest` MetaMessage regardless of the
rate-limit timers.

## Profile Data Fields

| Field | Description |
|---|---|
| `display_name` | Human-readable name shown in the UI |
| `bio` | Short description / status text |
| `avatar_hash` | SHA-256 content hash of an avatar image attachment |
| `user_id` | Peer ID of the profile owner (for attribution) |
