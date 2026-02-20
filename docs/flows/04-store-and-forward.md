# Flow: Store-and-Forward Direct Message

When the intended recipient is offline and no relay is available to hold
messages long enough, a sender can route a message through a **mutual friend**
who stores it until the recipient reconnects.

The outer envelope is encrypted for the storage peer (who unwraps and holds it),
while the inner envelope is encrypted for the final recipient (who decrypts and
reads it). The storage peer never sees the message content.

See [architecture.md § Peer Store-and-Forward](../architecture.md#peer-store-and-forward)
for design context.

## Message Routing via Storage Peer

```mermaid
sequenceDiagram
    participant A as Alice (sender)
    participant R as Relay
    participant B as Bob (storage peer, mutual friend)
    participant C as Carol (offline recipient)

    Note over C: Carol is currently offline

    rect rgb(235, 245, 255)
        Note over A: Step 1 – Build inner envelope (for Carol)
        A->>A: Generate content_key
        A->>A: Encrypt message with ChaCha20Poly1305(content_key)
        A->>A: Wrap content_key with HPKE(Carol's X25519 public key)
        A->>A: Sign header with Ed25519
        A->>A: Inner Envelope:<br/>{recipient: Carol, kind: Direct, encrypted_payload}
    end

    rect rgb(240, 255, 240)
        Note over A: Step 2 – Wrap inner envelope for Bob
        A->>A: Serialize inner envelope as bytes
        A->>A: Encrypt serialized inner envelope<br/>with HPKE(Bob's X25519 public key)
        A->>A: Build outer Envelope:<br/>{kind: StoreForPeer,<br/> recipient_id: Bob,<br/> storage_peer_id: Bob,<br/> store_for: Carol,<br/> payload: encrypted(inner envelope)}
        A->>A: Sign outer header with Ed25519
    end

    A->>R: POST /envelopes (outer StoreForPeer)
    R->>R: Store in Bob's inbox
    R-->>A: 200 OK

    rect rgb(255, 245, 230)
        Note over B: Step 3 – Bob receives and stores
        B->>R: GET /inbox/{bob_peer_id}
        R-->>B: [StoreForPeer envelope]
        B->>B: Verify outer Ed25519 signature
        B->>B: Decrypt outer payload with own X25519 key<br/>→ recover inner envelope bytes
        B->>B: Store inner envelope in local DB<br/>(tagged store_for = Carol)
    end

    Note over C: Carol comes back online

    rect rgb(255, 240, 255)
        Note over C,B: Step 4 – Carol announces online
        C->>R: POST /envelopes (MetaMessage::Online → Bob)
        R->>R: Store in Bob's inbox
        B->>R: GET /inbox/{bob_peer_id}
        R-->>B: [Online announcement from Carol]
        B->>B: Mark Carol as online
    end

    rect rgb(245, 235, 255)
        Note over B,C: Step 5 – Bob forwards to Carol
        B->>B: Look up stored envelopes for Carol
        B->>R: POST /envelopes (inner envelope, recipient: Carol)
        R->>R: Store in Carol's inbox
        R-->>B: 200 OK
    end

    rect rgb(235, 255, 245)
        Note over C: Step 6 – Carol receives and decrypts
        C->>R: GET /inbox/{carol_peer_id}
        R-->>C: [inner Direct envelope from Alice]
        C->>C: Verify Alice's Ed25519 signature
        C->>C: Unwrap content_key with HPKE(Carol's X25519 private key)
        C->>C: Decrypt body with ChaCha20Poly1305
        C->>C: Store message in SQLite
    end

    Note over A,C: Alice's message delivered to Carol<br/>Bob held an encrypted blob he could not read
```

## Privacy Properties

| Question | Answer |
|---|---|
| Can Bob read Alice's message to Carol? | No — inner envelope is encrypted for Carol only |
| Can the relay read any content? | No — both envelopes are HPKE-encrypted |
| Does Carol learn that Bob was the storage peer? | Yes, via the `storage_peer_id` field in the outer header |
| Does the relay know Carol is the final recipient? | Yes — `store_for` field is visible in the outer header |
