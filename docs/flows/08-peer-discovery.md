# Flow: Peer Discovery and Public Key Fetching

Tenet uses a **Trust on First Use (TOFU)** model. Peers learn each other's
public keys through explicit handshakes or via the mesh delivery protocol.
There is no global key directory — key distribution is decentralised.

## How Peers Learn Each Other's Keys

There are three paths to obtaining a peer's public keys:

| Path | When used |
|---|---|
| Friend request handshake | Adding a new friend directly |
| Mesh delivery (`sender_keys`) | Receiving posts from peers of peers |
| Profile request / response | Refreshing or fetching profile data |

## Path 1: Friend Request (Direct Key Exchange)

The [friend request flow](01-add-friend.md) is the primary key exchange
mechanism. Keys are embedded in the plaintext `FriendRequest` and `FriendAccept`
meta messages.

```mermaid
sequenceDiagram
    participant A as Alice
    participant R as Relay
    participant B as Bob

    Note over A: Knows only Bob's Peer ID<br/>(shared out-of-band, e.g. QR code)

    A->>R: POST /envelopes → Bob<br/>FriendRequest {signing_key, encryption_key}
    R-->>B: delivered

    B->>R: POST /envelopes → Alice<br/>FriendAccept {signing_key, encryption_key}
    R-->>A: delivered

    Note over A,B: Both peers now have each other's<br/>Ed25519 signing key and X25519 encryption key
    Note over A,B: Peer ID binding:<br/>SHA-256(signing_public_key) == peer_id<br/>→ key cannot be forged for an existing ID
```

## Path 2: Mesh Delivery (Keys from Unknown Peers)

When a peer receives messages via [mesh distribution](07-mesh-distribution.md)
from someone it has never met, the delivering peer includes a `sender_keys` map
so the receiver can verify signatures.

```mermaid
sequenceDiagram
    participant A as Alice
    participant B as Bob (mutual friend)
    participant C as Carol (unknown to Alice)

    Note over C,B: Carol posts a public message<br/>Bob has seen it

    B->>A: MeshDelivery<br/>{envelopes: [Carol's post],<br/> sender_keys: {"carol_id": carol_signing_key_hex}}

    rect rgb(235, 245, 255)
        Note over A: Verify key is self-authenticating
        A->>A: Compute SHA-256(carol_signing_key_bytes)
        A->>A: Compare with carol_id in envelope header
        Note over A: Match → key cannot be forged
    end

    A->>A: Verify Carol's Ed25519 signature
    A->>A: Store Carol's signing key locally
    A->>A: Backfill: verify any earlier unverified<br/>messages from Carol
    Note over A: Alice can now verify Carol's future messages<br/>even without being friends
```

## Path 3: Profile Request

```mermaid
sequenceDiagram
    participant A as Alice
    participant R as Relay
    participant B as Bob

    Note over A: Bob's profile is missing or stale<br/>(checked hourly for missing, daily for stale)

    A->>A: Build MetaMessage::ProfileRequest<br/>{peer_id: Alice, for_peer_id: Bob}
    A->>R: POST /envelopes → Bob
    R-->>B: delivered

    B->>B: Receive ProfileRequest
    B->>B: Build profile payload:<br/>{"type":"tenet.profile", "display_name":…, "bio":…}
    B->>B: Encrypt for Alice (HPKE Direct message)
    B->>R: POST /envelopes → Alice
    R-->>A: delivered

    A->>A: Decrypt profile message
    A->>A: Upsert profiles table
    A->>A: Broadcast WsEvent::ProfileUpdated to browser
```

## Signature Verification on Receive

Every received envelope is checked against the sender's known signing key.

```mermaid
sequenceDiagram
    participant P as Peer (sync loop)
    participant DB as SQLite

    P->>P: Receive envelope from sender X

    alt Sender X is a known peer
        P->>DB: SELECT signing_public_key FROM peers WHERE peer_id = X
        DB-->>P: key_hex
        P->>P: Verify Ed25519 signature ✓
    else Sender X is unknown (e.g. public message via mesh)
        P->>P: Check sender_keys from MeshDelivery
        P->>P: Verify SHA-256(key) == sender_id
        P->>P: Verify Ed25519 signature ✓
        P->>DB: Cache signing key for future verification
    else Key not available
        P->>P: Store message with signature_verified = false
        Note over P: Key may arrive later via mesh or profile request
    end
```

## Key Binding Guarantee

The Peer ID is `SHA-256(signing_public_key)`. Because SHA-256 is
preimage-resistant, an attacker cannot manufacture a signing key that hashes to
an existing Peer ID. Possessing a valid signature proves ownership of the
corresponding private key.
