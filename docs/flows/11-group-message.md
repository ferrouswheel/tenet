# Flow: Group Message

Once a member holds the group's symmetric key they can send and receive group
messages. Unlike direct messages which use HPKE per-recipient wrapping, group
messages use **a single ChaCha20Poly1305 symmetric key** shared by all members.
Every member uses the same key to decrypt.

See [group-create-invite.md](10-group-create-invite.md) for how the key is
established, and [groups.md](../groups.md) for the full API reference.

## Sending a Group Message

```mermaid
sequenceDiagram
    participant B as Browser
    participant S as Sender (server)
    participant DB as SQLite
    participant R as Relay

    B->>S: POST /api/messages/group<br/>{group_id, body}

    rect rgb(235, 245, 255)
        Note over S: Encrypt with group key
        S->>DB: SELECT group_key FROM groups WHERE group_id = ?
        DB-->>S: group_key (32 bytes)
        S->>S: Generate random 12-byte nonce
        S->>S: Encrypt body with<br/>ChaCha20Poly1305(group_key, nonce, body)
        S->>S: Compute message_id = SHA-256(payload)
    end

    rect rgb(240, 255, 240)
        Note over S: Build and sign envelope
        S->>S: Build Header:<br/>{sender, recipient: member_N,<br/> kind: FriendGroup,<br/> group_id, timestamp, ttl}
        S->>S: Sign header with Ed25519
    end

    rect rgb(255, 245, 230)
        Note over S,R: Deliver to each group member individually
        S->>DB: SELECT peer_id FROM group_members WHERE group_id = ?<br/>AND peer_id != own_id
        loop For each member
            S->>R: POST /envelopes<br/>{kind: FriendGroup, recipient: member_N}
            R->>R: Store in member's inbox
        end
    end

    S->>DB: INSERT message (own copy, kind=friend_group)
    S-->>B: 201 Created {message_id}
```

## Receiving a Group Message

```mermaid
sequenceDiagram
    participant R as Relay
    participant M as Member (sync loop)
    participant DB as SQLite
    participant UI as Browser UI

    alt WebSocket push
        R->>M: Push FriendGroup envelope
    else Periodic poll
        M->>R: GET /inbox/{peer_id}
        R-->>M: [envelopes including FriendGroup ones]
    end

    loop For each FriendGroup envelope
        M->>M: Check dedup index
        M->>M: Verify Ed25519 signature<br/>using sender's stored signing key
        M->>M: Check TTL

        rect rgb(235, 245, 255)
            Note over M: Decrypt with group key
            M->>DB: SELECT group_key FROM groups WHERE group_id = ?
            DB-->>M: group_key
            M->>M: Decrypt with ChaCha20Poly1305(group_key, nonce, ciphertext)
        end

        M->>DB: INSERT message (body, sender, group_id, kind=friend_group)
        M->>UI: WsEvent::NewMessage<br/>{message_id, sender_id, kind: "friend_group", body}
        UI->>UI: Show in group conversation
    end
```

## Comparison: Group vs Direct Encryption

```mermaid
sequenceDiagram
    participant S as Sender
    participant R as Relay
    participant A as Member A
    participant B as Member B

    Note over S,B: Group message (one key, multiple recipients)
    S->>S: Encrypt with group_key (shared)
    S->>R: POST /envelopes → A (FriendGroup)
    S->>R: POST /envelopes → B (FriendGroup)
    A->>A: Decrypt with group_key
    B->>B: Decrypt with group_key

    Note over S,B: Direct message (different key wrap per recipient)
    S->>S: Generate content_key
    S->>S: HPKE-wrap for A using A's X25519 key
    S->>R: POST /envelopes → A (Direct)
    S->>S: HPKE-wrap for B using B's X25519 key
    S->>R: POST /envelopes → B (Direct)
    A->>A: HPKE-unwrap with A's private key
    B->>B: HPKE-unwrap with B's private key
```

## Known Limitations

- **No key rotation on member removal**: removing a member from the group does
  not generate a new group key. The removed member retains their copy of the
  old key and can decrypt past (and future) messages if they store them.
- **One group key per group**: all messages use the same key regardless of
  when a member joined. A member who joins later and receives the current key
  could theoretically decrypt past messages if they obtain old ciphertext.
