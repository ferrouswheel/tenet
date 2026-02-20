# Flow: Group Creation and Invite

Groups share a 32-byte symmetric key. The creator generates the key and
distributes it to each member **after** they explicitly accept an invitation.
This consent model ensures no peer receives a group key without agreeing to join.

See [groups.md](../groups.md) for REST API reference and schema details.

## Creating a Group and Inviting Members

```mermaid
sequenceDiagram
    participant B as Browser
    participant Cr as Creator (server)
    participant R as Relay
    participant M1 as Member 1
    participant M2 as Member 2

    B->>Cr: POST /api/groups<br/>{group_id, member_ids: [M1, M2], message?}

    rect rgb(235, 245, 255)
        Note over Cr: Setup
        Cr->>Cr: Generate random 32-byte group_key
        Cr->>Cr: INSERT INTO groups (group_id, group_key, creator_id)
        Cr->>Cr: INSERT INTO group_members (group_id, creator_id) ← creator joins immediately
    end

    rect rgb(240, 255, 240)
        Note over Cr,R: Send invitations (no key yet)
        loop For each invited member
            Cr->>Cr: Build MetaMessage::GroupInvite<br/>{peer_id: creator, group_id, message?}
            Cr->>Cr: Sign envelope header
            Cr->>R: POST /envelopes → Member
            R->>R: Store in member's inbox
            Cr->>Cr: INSERT INTO group_invites<br/>(status=pending, direction=outgoing)
        end
    end

    Cr-->>B: 201 Created {group_id}

    rect rgb(255, 245, 230)
        Note over M1,R: Member 1 receives invite
        M1->>R: GET /inbox/{M1_peer_id}
        R-->>M1: [GroupInvite envelope]
        M1->>M1: Verify creator's Ed25519 signature
        M1->>M1: INSERT INTO group_invites<br/>(status=pending, direction=incoming)
        M1->>M1: WsEvent::GroupInviteReceived → browser
        Note over M1: UI shows Accept / Ignore
    end

    rect rgb(255, 240, 255)
        Note over M1,R: Member 1 accepts
        M1->>Cr: POST /api/group-invites/{id}/accept
        M1->>M1: UPDATE group_invites SET status=accepted
        M1->>M1: Build MetaMessage::GroupInviteAccept<br/>{peer_id: M1, group_id}
        M1->>R: POST /envelopes → Creator
        R->>R: Store in creator's inbox
    end

    rect rgb(245, 235, 255)
        Note over Cr,R: Creator distributes group key to M1
        Cr->>R: GET /inbox/{creator_peer_id}
        R-->>Cr: [GroupInviteAccept from M1]
        Cr->>Cr: UPDATE group_invites SET status=accepted (outgoing)
        Cr->>Cr: INSERT INTO group_members (group_id, M1)
        Cr->>Cr: WsEvent::GroupMemberJoined → browser
        Cr->>Cr: Build group_key_distribution payload:<br/>{"type":"group_key_distribution",<br/> "group_id": "…",<br/> "group_key": "<hex>",<br/> "key_version": 1,<br/> "creator_id": "…"}
        Cr->>Cr: Encrypt for M1 (HPKE Direct message)
        Cr->>R: POST /envelopes → M1
    end

    rect rgb(235, 255, 245)
        Note over M1: M1 receives group key
        M1->>R: GET /inbox/{M1_peer_id}
        R-->>M1: [group_key_distribution from Creator]
        M1->>M1: Decrypt HPKE payload
        M1->>M1: Consent check:<br/>verify accepted invite exists from this creator
        M1->>M1: INSERT INTO groups (group_id, group_key, key_version)
        M1->>M1: INSERT INTO group_members (group_id, M1)
        Note over M1: Can now decrypt FriendGroup messages
    end

    Note over M2: Same flow runs independently for M2
```

## Adding a Member Later

```mermaid
sequenceDiagram
    participant Cr as Creator
    participant R as Relay
    participant New as New Member

    Cr->>R: POST /api/groups/{id}/members<br/>{peer_id: New}
    Note over Cr,New: Same GroupInvite → GroupInviteAccept<br/>→ group_key_distribution flow as above
```

## Consent Model

```mermaid
sequenceDiagram
    participant Cr as Creator
    participant R as Relay
    participant M as Invitee

    Cr->>R: GroupInvite → M
    R-->>M: delivered
    Note over M: Does NOT have the group key yet
    Note over M: Cannot decrypt any FriendGroup messages yet

    alt M ignores
        Note over M: status = "ignored"<br/>No response sent<br/>Creator's invite stays "pending"
    else M accepts
        M->>R: GroupInviteAccept → Creator
        R-->>Cr: delivered
        Cr->>R: group_key_distribution → M
        R-->>M: delivered
        Note over M: NOW has the group key<br/>Can decrypt FriendGroup messages
    end
```
