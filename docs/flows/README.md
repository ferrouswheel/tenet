# Protocol Flow Diagrams

Mermaid sequence diagrams for the major Tenet protocol flows. Each diagram
shows the messages exchanged between peers, the relay, and client components.

## Index

| # | Flow | Description |
|---|---|---|
| 01 | [Add Friend](01-add-friend.md) | Mutual key exchange handshake; establishes encrypted channel |
| 02 | [Peer Reconnect](02-peer-reconnect.md) | Online announcement, inbox polling, mesh queries on startup |
| 03 | [Direct Message](03-direct-message.md) | HPKE encryption, signing, relay delivery, and decryption |
| 04 | [Store-and-Forward](04-store-and-forward.md) | Routing a message through a mutual friend for an offline recipient |
| 05 | [Public Message](05-public-message.md) | Timeline post delivery to known friends |
| 06 | [Attachment](06-attachment.md) | Content-addressed file upload, embedding in encrypted payloads, and download |
| 07 | [Mesh Distribution](07-mesh-distribution.md) | Four-way gossip protocol for sharing past public messages |
| 08 | [Peer Discovery](08-peer-discovery.md) | How signing and encryption keys are obtained and verified |
| 09 | [Profile Updates](09-profile-updates.md) | Push profile to friends; pull profile on request |
| 10 | [Group Create & Invite](10-group-create-invite.md) | Group creation, consent-based invite, and key distribution |
| 11 | [Group Message](11-group-message.md) | Sending and receiving messages with the shared group key |
| 12 | [Reactions & Replies](12-reactions-replies.md) | Upvote/downvote reactions and threaded replies |
| 13 | [Relay WebSocket Push](13-relay-websocket.md) | Real-time envelope delivery and local UI event broadcasting |

## Reading the Diagrams

- **Participants** represent distinct roles: `Peer`, `Relay`, `Browser UI`,
  `SQLite`, etc.
- **Solid arrows** (`->>`) are messages sent; **dashed arrows** (`-->>`) are
  responses.
- **Coloured `rect` blocks** group related steps into phases.
- **Notes** explain preconditions, decisions, or side effects.

## Core Concepts

### Message Kinds

| Kind | Purpose | Encrypted? |
|---|---|---|
| `Public` | Timeline posts | No (signed only) |
| `Direct` | 1-to-1 messages | HPKE per-recipient |
| `FriendGroup` | Group messages | Symmetric group key |
| `Meta` | Protocol messages (friend requests, online, profiles, mesh) | Plaintext or per-recipient |
| `StoreForPeer` | Outer wrapper for store-and-forward | HPKE for storage peer |

### Relay Role

The relay is **untrusted by design**. It stores opaque encrypted blobs and
routes them by `recipient_id`. It can observe sender ID, recipient ID,
timestamps, and message sizes — but never plaintext content.

### Key Types

| Key | Algorithm | Derived ID |
|---|---|---|
| Signing key | Ed25519 | `SHA-256(signing_pub_key)` = Peer ID |
| Encryption key | X25519 (for HPKE) | — |
| Group key | 32-byte random symmetric | — |
