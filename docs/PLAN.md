# Tenet Project Assessment & Implementation Plan

## Current State Assessment

### ✅ What's Working Well

**Core Protocol** (src/protocol.rs):
- Message types defined: `Public`, `Meta`, `Direct`, `FriendGroup`, `StoreForPeer`
- Headers with signatures, TTL, message IDs
- Validation rules for message kinds
- Content-addressed IDs using SHA256
- Envelope serialization/deserialization

**Cryptography** (src/crypto.rs):
- HPKE key encapsulation for per-recipient encryption
- ChaCha20Poly1305 authenticated encryption
- X25519 keypairs and key derivation
- Content key generation

**Relay Server** (src/relay.rs):
- HTTP relay with Axum
- TTL enforcement and expiration
- Deduplication by message ID
- Storage quotas (max messages, max bytes per recipient)
- Batch operations

**Client Implementations** (src/client.rs):
- `RelayClient` for HTTP communication
- `SimulationClient` for testing
- Store-and-forward for offline peers (via `StoreForPeer`)
- Meta messages for presence (Online/Ack/MessageRequest)

**Simulation Framework**:
- Comprehensive simulation harness
- Scenario-based testing
- Metrics tracking (latency, delivery rates)

### ❌ Critical Gaps

#### 1. **Public Messages Not Implemented**

**Current State**: `MessageKind::Public` exists in the enum but is completely unimplemented.

**What's Missing**:
- No client code sends or receives public messages
- No relay logic for public message distribution
- No mechanism to prevent message amplification
- No peer-level tracking to avoid re-sending messages
- No time-based cutoff beyond TTL

**Required Behavior**:
- Any peer can forward a public message to other peers
- Messages remain signed by original author
- Deduplication at multiple levels:
  - Relay level: Already exists (message ID deduplication)
  - Peer level: Need to track which peers have received which messages
- Don't re-send to peers who already have the message
- Stop propagating messages after TTL expires
- Gossip-style distribution across peer networks

#### 2. **Group Messages Not Implemented**

**Current State**: `MessageKind::FriendGroup` exists, header validation requires `group_id`, but no actual group functionality.

**What's Missing**:
- No group key management (creation, distribution, rotation)
- No group membership tracking
- No group message encryption/decryption
- No client code to send/receive group messages
- No group discovery or invitation mechanism

**Required Behavior**:
- Groups have unique IDs
- Group members share a symmetric key for message encryption
- Messages encrypted with group key can be decrypted by all members
- Group metadata (member list, key version) needs management
- Messages delivered to all online group members via relay

#### 3. **Signature Verification Not Cryptographically Secure**

**Current Issue**: protocol.rs:176-177 shows that `expected_signature()` is just a SHA256 hash of the header, not an actual Ed25519 signature!

```rust
let digest = Sha256::digest(&bytes);
Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest))
```

This means:
- Headers can't be verified as coming from the claimed sender
- Anyone can forge messages
- Critical for public messages where authenticity is essential

#### 4. **Other Notable Gaps**:
- No persistent local storage (only in-memory feeds)
- Attachment storage/retrieval infrastructure missing
- No peer list management in clients
- No rate limiting or abuse prevention beyond relay quotas

---

## Implementation Plan

### Phase 1: Foundation Improvements (Prerequisites)

**1.1 Add True Cryptographic Signatures**
- Add Ed25519 signing to crypto.rs
- Update `StoredKeypair` to include Ed25519 keys alongside X25519
- Modify `Header::expected_signature()` to actually sign with sender's private key
- Update `Header::verify_signature()` to verify against sender's public key
- Requires: Sender public key lookup in clients
- Location: `src/crypto.rs`, `src/protocol.rs`

**1.2 Peer Directory for Public Key Lookup**
- Add peer registry to clients
- Store: peer_id → public_key mapping
- Needed for signature verification
- Location: `src/client.rs`, new `PeerRegistry` struct

### Phase 2: Public Messages Implementation

**2.1 Core Public Message Types** (src/protocol.rs)
```rust
pub struct PublicMessageMetadata {
    pub seen_by: HashSet<String>,  // Track which peers have seen this
    pub first_seen_at: u64,        // When we first saw this message
    pub propagation_count: usize,  // How many times we've forwarded
}
```

**2.2 Client-Side Public Message Handling** (src/client.rs)
- Add public message tracking:
  - `seen_public_messages: HashMap<ContentId, PublicMessageMetadata>`
  - `public_message_cache: Vec<Envelope>` (recent public messages)
- Implement `send_public_message()`:
  - Create envelope with `MessageKind::Public`
  - Broadcast to all known peers
  - Track initial distribution
- Implement `receive_public_message()`:
  - Verify signature against sender's public key
  - Check deduplication
  - Add to local cache
  - Determine if should forward
- Implement `forward_public_message()`:
  - Check TTL hasn't expired
  - Don't send to peers in `seen_by` set
  - Update `seen_by` when forwarding
  - Respect propagation limits

**2.3 Relay Public Message Support** (src/relay.rs)
- Relay can treat public messages like any other for storage
- Add optional gossip endpoint for public messages
- Consider: separate queue for public vs direct messages

**2.4 Propagation Algorithm**
Strategy: Controlled flooding with tracking
```
On receiving public message:
1. Verify signature
2. Check if seen (message_id in cache) → skip if yes
3. Check TTL not expired → skip if expired
4. Add to local cache with metadata
5. Deliver to local user
6. For each peer in peer list:
   - If peer not in seen_by set:
     - Forward message to peer
     - Add peer to seen_by set
   - Track propagation count
   - Stop if propagation_count > MAX_HOPS
```

**2.5 Tests for Public Messages**
- `tests/public_message_tests.rs`: Unit tests
- Simulation scenarios with public message propagation
- Test deduplication across multiple hops
- Test TTL expiration stops propagation
- Test seen_by tracking prevents loops

### Phase 3: Group Messages Implementation

**3.1 Group Key Management** (new src/groups.rs)
```rust
pub struct GroupInfo {
    pub group_id: String,
    pub group_key: [u8; 32],      // Symmetric key for group
    pub members: HashSet<String>,  // Member peer IDs
    pub created_at: u64,
    pub key_version: u32,
}

pub struct GroupManager {
    groups: HashMap<String, GroupInfo>,
}

impl GroupManager {
    pub fn create_group(&mut self, members: Vec<String>) -> GroupInfo;
    pub fn add_member(&mut self, group_id: &str, peer_id: &str) -> Result<(), GroupError>;
    pub fn remove_member(&mut self, group_id: &str, peer_id: &str) -> Result<(), GroupError>;
    pub fn rotate_key(&mut self, group_id: &str) -> Result<(), GroupError>;
    pub fn get_group_key(&self, group_id: &str) -> Option<&[u8; 32]>;
}
```

**3.2 Group Message Encryption** (src/crypto.rs)
```rust
pub fn encrypt_group_payload(
    plaintext: &[u8],
    group_key: &[u8; 32],
    aad: &[u8],
) -> Result<(Vec<u8>, Vec<u8>), CryptoError>;

pub fn decrypt_group_payload(
    ciphertext: &[u8],
    nonce: &[u8],
    group_key: &[u8; 32],
    aad: &[u8],
) -> Result<Vec<u8>, CryptoError>;
```

**3.3 Group Message Protocol** (src/protocol.rs)
```rust
pub fn build_group_message_payload(
    plaintext: impl AsRef<[u8]>,
    group_key: &[u8; 32],
    aad: &[u8],
) -> Result<Payload, PayloadCryptoError>;

pub fn decrypt_group_message_payload(
    payload: &Payload,
    group_key: &[u8; 32],
    aad: &[u8],
) -> Result<Vec<u8>, PayloadCryptoError>;
```

**3.4 Client Group Support** (src/client.rs)
- Add `GroupManager` to client implementations
- Implement `create_group()`:
  - Generate group key
  - Share key with initial members (encrypted with each member's public key)
  - Store group locally
- Implement `send_group_message()`:
  - Encrypt with group key
  - Create envelope with `MessageKind::FriendGroup` and `group_id`
  - Send to all group members (or relay)
- Implement `receive_group_message()`:
  - Verify sender is group member
  - Look up group key
  - Decrypt payload
  - Deliver to user

**3.5 Group Key Distribution Protocol**
Initial approach (simple):
- Group creator generates symmetric key
- For each member: wrap key with HPKE (like direct messages)
- Send `MessageKind::Meta` with group invitation + wrapped key
- Members store group key locally

Advanced (future):
- Use a group key agreement protocol (e.g., TreeKEM)
- Support member addition/removal with forward secrecy

**3.6 Tests for Group Messages**
- `tests/group_message_tests.rs`: Unit tests
- Test group creation and key distribution
- Test message encryption/decryption
- Test multi-member delivery
- Simulation scenarios with groups

### Phase 4: Integration & Polish

**4.1 Update Binary Targets**
- `bin/tenet.rs` (CLI):
  - Add `send-public <message>` command
  - Add `create-group <peer1> <peer2> ...` command
  - Add `send-group <group_id> <message>` command
  - Add `list-groups` command
- `bin/tenet-debugger.rs` (TUI):
  - Add public message sending
  - Add group creation/management UI
  - Show public message propagation
- `bin/tenet-sim.rs`:
  - Add public message scenarios
  - Add group message scenarios

**4.2 Enhanced Scenarios**
Create new scenario files:
- `scenarios/public_gossip.toml`: Public message propagation
- `scenarios/group_chat.toml`: Group messaging with multiple groups
- `scenarios/mixed_traffic.toml`: Direct, public, and group messages

**4.3 Documentation**
- Update `docs/architecture.md` with public/group message details
- Add `docs/public_messages.md` - Public message propagation algorithm
- Add `docs/group_messages.md` - Group management and key distribution
- Update `CLAUDE.md` with new types and patterns

**4.4 Additional Tests**
- Integration tests for public + group messages
- Test message kind transitions
- Test security boundaries (can't decrypt without keys)
- Test abuse scenarios (spam, amplification)

---

## Implementation Order & Priority

### High Priority (Core Functionality)
1. ✅ **Phase 1.1**: True cryptographic signatures (CRITICAL for security) - **COMPLETE**
2. ✅ **Phase 1.2**: Peer directory for public key lookup - **COMPLETE**
3. **Phase 2**: Public messages (requested by user)
4. **Phase 3**: Group messages (requested by user)

### Medium Priority (Completeness)
5. **Phase 4.1**: Update CLI/TUI binaries
6. **Phase 4.2**: Enhanced scenarios
7. **Phase 4.4**: Additional tests

### Lower Priority (Nice to Have)
8. **Phase 4.3**: Documentation updates
9. Advanced group key management (TreeKEM)
10. Persistent storage
11. Attachment handling
12. Rate limiting & abuse prevention

---

## Key Design Decisions

### Public Messages: Propagation Strategy
**Decision**: Controlled flooding with per-message seen_by tracking
- **Pros**: Simple, resilient, natural gossip-style distribution
- **Cons**: Bandwidth overhead, requires peer-level state
- **Alternatives considered**: DHT-based routing (too complex), publish-subscribe (requires infrastructure)

### Group Messages: Key Distribution
**Decision**: Simple symmetric key + HPKE wrapping for initial version
- **Pros**: Leverages existing crypto, straightforward
- **Cons**: No forward secrecy on member removal, key rotation is manual
- **Future**: Upgrade to TreeKEM or similar group key agreement

### Signature Scheme
**Decision**: Ed25519 for message signatures, separate from X25519 for encryption
- **Pros**: Industry standard, fast verification, small signatures
- **Cons**: Need to manage two keypairs per identity (can derive one from the other in future)

### Deduplication Strategy
**Decision**: Multi-level deduplication
1. Relay: Message ID dedup with TTL-based expiry
2. Client: Per-client seen set for all message types
3. Public messages: Per-message seen_by set for propagation control

---

## Estimated Complexity

| Component | LOC Estimate | Risk | Dependencies |
|-----------|--------------|------|--------------|
| Crypto signatures (Phase 1.1) | ~200 | Medium | Ed25519 crate |
| Peer registry (Phase 1.2) | ~100 | Low | None |
| Public messages (Phase 2) | ~500 | Medium | Phase 1 |
| Group messages (Phase 3) | ~800 | High | Phase 1 |
| Binary updates (Phase 4.1) | ~300 | Low | Phase 2, 3 |
| Tests | ~600 | Low | All phases |
| **Total** | **~2,500** | | |

---

## Status Tracking

- [x] Phase 1.1: Cryptographic Signatures
- [x] Phase 1.2: Peer Directory
- [ ] Phase 2: Public Messages
- [ ] Phase 3: Group Messages
- [ ] Phase 4: Integration & Polish
