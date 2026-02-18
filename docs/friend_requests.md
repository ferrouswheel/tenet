# Friend Requests

This document describes the friend request protocol, how key exchange works during the process,
and the security properties of the handshake.

## Overview

Adding a friend in Tenet is a multi-step process that exchanges cryptographic keys between two
peers. The initiator only needs the recipient's **Peer ID** to start the process. All signing and
encryption keys are exchanged automatically as part of the friend request handshake.

## Cryptographic Primitives

| Primitive | Algorithm | Purpose |
|-----------|-----------|---------|
| Asymmetric encryption | HPKE (X25519-HKDF-SHA256 + ChaCha20Poly1305) | Wrapping per-message content keys |
| Symmetric encryption | ChaCha20Poly1305 | Encrypting message payloads |
| Signing | Ed25519 | Message authentication and integrity |
| Hashing | SHA-256 | Content addressing (ContentId) |

### How HPKE Encryption Works

HPKE (Hybrid Public Key Encryption) combines asymmetric and symmetric cryptography for efficient
message encryption:

1. **Key generation**: Each peer generates an X25519 key pair (for encryption) and an Ed25519 key
   pair (for signing). The Peer ID is derived from `SHA-256(public_encryption_key)`, encoded as
   base64url.

2. **Sending an encrypted message**:
   - Generate a random 32-byte **content key**
   - Encrypt the message body with **ChaCha20Poly1305** using the content key (with a random 12-byte
     nonce and authenticated additional data)
   - **Wrap** the content key using HPKE: this performs an X25519 Diffie-Hellman exchange with the
     recipient's public key, derives a shared secret via HKDF-SHA256, and encrypts the content key
     with ChaCha20Poly1305
   - Sign the message header with the sender's **Ed25519 private key**
   - Package everything into an Envelope: header + wrapped key + encrypted payload

3. **Receiving an encrypted message**:
   - Verify the Ed25519 signature on the header using the sender's known signing public key
   - **Unwrap** the content key using the recipient's X25519 private key (reverse of the HPKE wrapping)
   - Decrypt the message body with ChaCha20Poly1305 using the recovered content key

This approach means each message has a unique content key, so compromising one message does not
compromise others.

## Friend Request Protocol

### Step 1: Send Friend Request (Initiator)

The initiator knows only the recipient's Peer ID. They send a **Meta message** of type
`friend_request` to the recipient's relay inbox containing:

```json
{
  "type": "friend_request",
  "peer_id": "<initiator's peer id>",
  "signing_public_key": "<initiator's Ed25519 public key, hex>",
  "encryption_public_key": "<initiator's X25519 public key, hex>",
  "message": "Hi, I'd like to connect!"
}
```

This message is delivered as a plaintext Meta envelope (not HPKE-encrypted) because the initiator
does not yet have the recipient's encryption public key. However:

- The message is delivered through the relay's store-and-forward mechanism to the recipient's
  inbox only.
- The envelope header is signed with the initiator's Ed25519 key, which the recipient can verify
  using the signing key included in the request.

### Step 2: Recipient Reviews Request

The recipient sees the incoming friend request in their Friend Requests UI. They can:

- **Accept** — proceed to key exchange
- **Ignore** — dismiss without responding (can be revisited later)
- **Block** — reject and prevent future requests from this peer

### Step 3: Accept and Respond (Recipient)

If the recipient accepts, they:

1. Store the initiator's signing and encryption public keys locally
2. Add the initiator as a friend
3. Send back a **Meta message** of type `friend_accept`:

```json
{
  "type": "friend_accept",
  "peer_id": "<recipient's peer id>",
  "signing_public_key": "<recipient's Ed25519 public key, hex>",
  "encryption_public_key": "<recipient's X25519 public key, hex>"
}
```

This response is also a plaintext Meta message, since the recipient now has the initiator's
encryption key but the initiator does not yet have the recipient's key. The message is signed by
the recipient's Ed25519 key.

### Step 4: Initiator Completes Connection

When the initiator receives the `friend_accept` response:

1. Store the recipient's signing and encryption public keys locally
2. Mark the friend request as accepted
3. The initiator now has the recipient's encryption key, so all subsequent messages can be
   HPKE-encrypted

At this point, both peers have each other's keys and can exchange fully encrypted Direct messages.

## Security Properties

### What is protected

- **Message confidentiality**: After the handshake, all Direct messages use HPKE encryption. Only
  the intended recipient can decrypt them.
- **Message integrity**: Every envelope header is Ed25519-signed. Tampering is detectable.
- **Forward secrecy (per-message)**: Each message uses a fresh random content key. Compromising
  one content key does not reveal other messages.
- **Content addressing**: Message IDs are SHA-256 hashes of the payload, enabling deduplication
  without reading content.

### Trust model

- **Trust on first use (TOFU)**: The friend request includes the sender's public keys. The
  recipient trusts that these keys belong to the claimed Peer ID. This is similar to how SSH or
  Signal handle initial key exchange.
- **Peer ID binding**: The Peer ID is derived from `SHA-256(public_encryption_key)`, so the
  binding between an identity and its keypair is cryptographically fixed. An attacker cannot
  impersonate a peer because SHA-256's preimage resistance prevents manufacturing a public key
  that maps to an existing Peer ID, and even with the correct public key, valid signatures require
  the corresponding Ed25519 private key.
- **Relay is untrusted**: The relay stores and forwards envelopes but cannot decrypt
  HPKE-encrypted content. It can observe metadata (sender ID, recipient ID, timestamps, message
  sizes).

### What is NOT protected during the handshake

- **Friend request content**: The initial friend request and acceptance are plaintext Meta
  messages. A relay operator (or network observer) can see who is requesting to be friends with
  whom, and the optional request message.
- **Metadata**: The relay always sees sender/recipient IDs and timing. This is inherent to the
  store-and-forward design.

### Threat mitigations

| Threat | Mitigation |
|--------|------------|
| Relay reads messages | HPKE encryption (post-handshake) |
| Message tampering | Ed25519 signatures on headers |
| Replay attacks | Content-addressed deduplication + TTL enforcement |
| Spam friend requests | Block functionality prevents repeat requests |
| Key impersonation | Peer ID derived from public key (TOFU model) |

## Sequence Diagram

```
Initiator                     Relay                      Recipient
    |                           |                            |
    |-- FriendRequest --------->|                            |
    |   (signing_key,           |-- store in inbox --------->|
    |    encryption_key,        |                            |
    |    message)               |                     [Review request]
    |                           |                            |
    |                           |<--- FriendAccept ---------|
    |<-- deliver from inbox ----|    (signing_key,           |
    |                           |     encryption_key)        |
    |                           |                            |
    [Store keys, mark accepted] |              [Store keys, add friend]
    |                           |                            |
    |== Encrypted Direct msgs ==|== Encrypted Direct msgs ==|
    |   (HPKE + ChaCha20)       |   (HPKE + ChaCha20)       |
```

## Block and Ignore Behavior

- **Block**: Stored locally on the recipient's client. Future friend requests from the blocked
  peer are automatically ignored. The blocked peer receives no indication they have been blocked.
- **Ignore**: The request remains in the recipient's request list but is hidden by default. It can
  be accepted or blocked later.
- Both block and ignore are client-side only — no message is sent back to the initiator. From the
  initiator's perspective, the request remains "pending."

## Web Client API

The web client exposes these REST endpoints for managing friend requests:

```
GET    /api/friend-requests                  -- list all requests (incoming + outgoing)
POST   /api/friend-requests                  -- send a request { peer_id, message? }
POST   /api/friend-requests/:id/accept       -- accept an incoming request
POST   /api/friend-requests/:id/ignore       -- ignore an incoming request
POST   /api/friend-requests/:id/block        -- block the sender
```
