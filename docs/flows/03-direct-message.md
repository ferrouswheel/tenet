# Flow: Direct Message

Direct messages are end-to-end encrypted using HPKE (Hybrid Public Key
Encryption). Each message gets a fresh random content key so that compromising
one message does not expose others. The relay handles opaque encrypted blobs and
never has access to plaintext.

Both peers must have completed the [friend request handshake](01-add-friend.md)
before sending encrypted direct messages, because the sender needs the
recipient's X25519 encryption public key.

## Sending

```mermaid
sequenceDiagram
    participant B as Browser
    participant S as Sender (web server)
    participant R as Relay

    B->>S: POST /api/messages/direct<br/>{recipient_id, body, attachments?}

    rect rgb(235, 245, 255)
        Note over S: Encryption (HPKE)
        S->>S: Generate random 32-byte content_key
        S->>S: Generate random 12-byte nonce
        S->>S: Encrypt body with<br/>ChaCha20Poly1305(content_key, nonce, body)
        S->>S: Wrap content_key with HPKE:<br/>X25519 DH with recipient's public key<br/>→ derive shared secret via HKDF-SHA256<br/>→ encrypt content_key with ChaCha20Poly1305
    end

    rect rgb(240, 255, 240)
        Note over S: Signing and packaging
        S->>S: Build Header:<br/>{sender, recipient, timestamp,<br/> message_id (SHA-256 of payload),<br/> kind: Direct, ttl_seconds}
        S->>S: Sign header with Ed25519 private key
        S->>S: Package Envelope:<br/>{header, wrapped_key, encrypted_payload}
    end

    S->>R: POST /envelopes
    R->>R: Validate TTL, check capacity
    R->>R: Store in recipient's inbox<br/>(dedup by message_id)
    R-->>S: 200 OK
    S->>S: Store in local SQLite (outbox)
    S-->>B: 201 Created {message_id}
```

## Receiving

```mermaid
sequenceDiagram
    participant R as Relay
    participant Rc as Recipient (sync loop)
    participant DB as SQLite
    participant UI as Browser UI

    alt WebSocket push (real-time)
        R->>Rc: Push envelope over WebSocket
        Note over Rc: Wakes sync loop immediately
    else Periodic poll
        Rc->>R: GET /inbox/{peer_id}
        R-->>Rc: [pending envelopes]
    end

    loop For each envelope
        Rc->>Rc: Check dedup index<br/>(skip if already stored)
        Rc->>Rc: Verify Ed25519 signature<br/>using sender's stored signing key
        Rc->>Rc: Check TTL (discard if expired)

        rect rgb(235, 245, 255)
            Note over Rc: Decryption (HPKE)
            Rc->>Rc: Unwrap content_key with HPKE:<br/>X25519 DH with own private key<br/>→ recover shared secret<br/>→ decrypt content_key
            Rc->>Rc: Decrypt body with<br/>ChaCha20Poly1305(content_key, nonce, ciphertext)
        end

        Rc->>DB: INSERT message (body, sender, timestamp, …)
        Rc->>UI: WsEvent::NewMessage<br/>{message_id, sender_id, body, timestamp}
        UI->>UI: Display message
    end
```

## Cryptographic Properties

| Property | Mechanism |
|---|---|
| Confidentiality | HPKE (X25519-HKDF-SHA256 + ChaCha20Poly1305) |
| Integrity | Ed25519 signature on header |
| Per-message forward secrecy | Fresh random `content_key` per message |
| Content addressing | `message_id = SHA-256(payload_bytes)` |
| Replay prevention | Dedup index + TTL enforcement |
| Relay sees | sender ID, recipient ID, timestamp, message size |
| Relay cannot see | message body, content_key, or any plaintext |
