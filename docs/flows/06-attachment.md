# Flow: Attachments

Attachments are stored using **content addressing**: the storage key is the
SHA-256 hash of the file bytes, so identical files are stored only once and
hashes double as integrity proofs.

For **direct and group messages**, attachment bytes are base64-encoded and
embedded directly in the encrypted payload — the relay never sees the file
content. For **public messages**, attachments travel as plaintext references
that any recipient can fetch by hash.

## Upload and Send (Direct Message with Attachment)

```mermaid
sequenceDiagram
    participant B as Browser
    participant S as Server
    participant DB as SQLite
    participant R as Relay
    participant Rc as Recipient

    rect rgb(235, 245, 255)
        Note over B,S: Step 1 – Upload attachment
        B->>S: POST /api/attachments<br/>multipart/form-data (file bytes)
        S->>S: Compute content_hash = SHA-256(file_bytes)
        S->>DB: INSERT INTO attachments<br/>(content_hash, content_type, size, data)
        S-->>B: 201 Created<br/>{content_hash, content_type, size_bytes}
    end

    rect rgb(240, 255, 240)
        Note over B,S: Step 2 – Send message referencing attachment
        B->>S: POST /api/messages/direct<br/>{recipient_id, body,<br/> attachments: [{hash, content_type, size}]}
        S->>DB: SELECT data FROM attachments WHERE content_hash = ?
        S->>S: Build payload:<br/>{body, attachments: [{hash, data_base64, …}]}
        S->>S: Encrypt entire payload with HPKE<br/>(attachment bytes included in ciphertext)
        S->>S: Sign header with Ed25519
        S->>R: POST /envelopes
        R->>R: Store encrypted envelope<br/>(relay cannot see file content)
        R-->>S: 200 OK
        S-->>B: 201 Created
    end

    rect rgb(255, 245, 230)
        Note over R,Rc: Step 3 – Recipient receives and stores
        Rc->>R: GET /inbox/{peer_id}
        R-->>Rc: [encrypted Direct envelope]
        Rc->>Rc: Verify signature
        Rc->>Rc: Decrypt payload (HPKE + ChaCha20Poly1305)
        Rc->>Rc: Extract attachment bytes from decrypted payload
        Rc->>Rc: Verify: SHA-256(bytes) == content_hash
        Rc->>DB: INSERT INTO attachments (content_hash, data, …)
        Rc->>DB: INSERT INTO message_attachments (message_id, content_hash)
        Rc->>DB: INSERT INTO messages
    end

    rect rgb(255, 240, 255)
        Note over B,Rc: Step 4 – Retrieve attachment later
        B->>S: GET /api/attachments/{content_hash}
        S->>DB: SELECT data FROM attachments WHERE content_hash = ?
        S-->>B: file bytes (with correct Content-Type)
    end
```

## Public Message with Attachment

For public messages, attachments are referenced by hash but **not embedded** in
the envelope. Recipients fetch the attachment separately from the original
sender or a peer who has it.

```mermaid
sequenceDiagram
    participant B as Browser
    participant S as Author's Server
    participant R as Relay
    participant P as Peer

    B->>S: POST /api/attachments (upload)
    S-->>B: {content_hash}

    B->>S: POST /api/messages/public<br/>{body, attachments: [{hash, …}]}
    S->>S: Build Public envelope:<br/>payload references hash only (no embedded bytes)
    S->>R: POST /envelopes
    R-->>P: envelope delivered

    P->>P: Receive public message<br/>with attachment reference {hash}
    Note over P: Attachment bytes not in envelope
    P->>S: GET /api/attachments/{content_hash}
    S-->>P: file bytes
    P->>P: Verify SHA-256(bytes) == hash
    P->>P: Store attachment locally
```

## Content Addressing Properties

| Property | Detail |
|---|---|
| Deduplication | Same file uploaded multiple times stores only once |
| Integrity | `SHA-256(bytes) == content_hash` verified on receive |
| Privacy (Direct) | Attachment bytes are inside the HPKE ciphertext |
| Privacy (Public) | Attachment bytes are fetched in plaintext from sender |
| Relay visibility | For Direct messages: none (hash is in encrypted header) |
