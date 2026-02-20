# Flow: Add Friend (Key Exchange Handshake)

Adding a friend establishes a mutual cryptographic relationship. Both peers
exchange their long-term signing and encryption public keys so that future
messages can be authenticated and end-to-end encrypted.

The initial request is sent **in plaintext** because the initiator does not yet
have the recipient's encryption key. After the handshake both peers have each
other's X25519 encryption keys and can use HPKE for all subsequent messages.

See [friend_requests.md](../friend_requests.md) for the full security analysis.

## Happy Path

```mermaid
sequenceDiagram
    participant I as Initiator
    participant R as Relay
    participant Rc as Recipient

    Note over I: Knows only Recipient's Peer ID

    rect rgb(235, 245, 255)
        Note over I,R: Step 1 – Send friend request
        I->>I: Build FriendRequest MetaMessage<br/>(signing_key, encryption_key, message?)
        I->>I: Sign header with Ed25519
        I->>R: POST /envelopes<br/>{kind: Meta, recipient: Recipient}
        R->>R: Store in Recipient's inbox
        R-->>I: 200 OK
    end

    rect rgb(240, 255, 240)
        Note over R,Rc: Step 2 – Recipient reviews
        Rc->>R: GET /inbox/{recipient_peer_id}
        R-->>Rc: [FriendRequest envelope]
        Rc->>Rc: Verify Ed25519 signature<br/>using signing_key in request
        Rc->>Rc: Store pending FriendRequestRow
        Note over Rc: UI shows Accept / Ignore / Block
    end

    rect rgb(255, 245, 230)
        Note over Rc,R: Step 3 – Accept and respond
        Rc->>Rc: Store Initiator's keys in peers table
        Rc->>Rc: Build FriendAccept MetaMessage<br/>(own signing_key, encryption_key)
        Rc->>Rc: Sign header with Ed25519
        Rc->>R: POST /envelopes<br/>{kind: Meta, recipient: Initiator}
        R->>R: Store in Initiator's inbox
        R-->>Rc: 200 OK
    end

    rect rgb(255, 240, 255)
        Note over R,I: Step 4 – Initiator completes connection
        I->>R: GET /inbox/{initiator_peer_id}
        R-->>I: [FriendAccept envelope]
        I->>I: Verify Ed25519 signature<br/>using signing_key in acceptance
        I->>I: Store Recipient's keys in peers table
        I->>I: Mark friend request as accepted
    end

    Note over I,Rc: Both peers now hold each other's<br/>X25519 encryption keys.<br/>All future messages use HPKE + ChaCha20Poly1305.
```

## Block / Ignore Variants

```mermaid
sequenceDiagram
    participant I as Initiator
    participant R as Relay
    participant Rc as Recipient

    I->>R: POST /envelopes (FriendRequest)
    R-->>Rc: delivered to inbox

    alt Recipient ignores
        Note over Rc: Status set to "ignored"<br/>No response sent<br/>Request revisitable later
    else Recipient blocks
        Note over Rc: Status set to "blocked"<br/>Future requests auto-rejected<br/>Sender gets no notification
    end

    Note over I: Request stays "pending" from Initiator's view<br/>No indication of ignore or block
```

## Security Notes

| Property | Detail |
|---|---|
| Friend request content | **Plaintext** — relay can see who is friending whom |
| Public key trust | TOFU (Trust on First Use) — recipient trusts keys in the request |
| Peer ID binding | `SHA-256(signing_public_key)` — cannot forge a matching key |
| Post-handshake messages | HPKE-encrypted — relay cannot read content |
