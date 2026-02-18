# Group Messaging

This document describes how groups work in Tenet: how they are created, how members are invited
and join, and how the group key is distributed.

## Overview

Groups in Tenet are named sets of peers that share a symmetric encryption key. All group messages
(`MessageKind::FriendGroup`) are encrypted with this shared key. The group creator manages
membership; new members must explicitly accept an invitation before they receive the key.

Key properties:
- Groups are identified by a `group_id` string (currently also used as the display name).
- A 32-byte random symmetric key is generated per group and distributed to members.
- Membership is consent-based: invitees receive a `GroupInvite` meta message and must accept
  before the key is sent.
- The creator is always an active member immediately.

## Group Creation

When a group is created (`POST /api/groups`):

1. A random 32-byte symmetric key is generated via `GroupManager::create_group`.
2. The creator is added as an active member in `group_members`.
3. For each invited peer, a `MetaMessage::GroupInvite` is sent via relay:

```json
{
  "type": "group_invite",
  "peer_id": "<creator's peer id>",
  "group_id": "my-group",
  "group_name": "my-group",
  "message": "Optional invite note"
}
```

Invited peers are **not** added to `group_members` until they accept. The invite is tracked in
the `group_invites` table with `status = "pending"` and `direction = "outgoing"`.

## Invite Flow

```
Creator                        Relay                       Recipient
  |                              |                              |
  | createGroup(id, [bob, ...])  |                              |
  |----------------------------->|                              |
  | -- GroupInvite meta msg --> bob                             |
  |                              |--- deliver to bob ---------->|
  |                              |                              | store GroupInviteRow
  |                              |                              | WsEvent::GroupInviteReceived
  |                              |                              | UI: notification + Accept/Ignore
  |                              |                              |
  |                              |   (bob clicks Accept)        |
  |                              |                              | POST /api/group-invites/:id/accept
  |                              |<-- GroupInviteAccept meta ---|
  |<-- deliver to creator -------|                              |
  | store acceptance                                            |
  | send group_key_distribution to bob                          |
  |----------------------------->|--- deliver to bob ---------->|
  |                              |                              | store GroupRow + GroupMemberRow
  |                              |                              | can now decrypt FriendGroup msgs
```

### Receiving an Invite

Behavior depends on the client:

**Web client** (`sync.rs`):
1. The invite is stored in `group_invites` with `status = "pending"` and `direction = "incoming"`.
2. A `WsEvent::GroupInviteReceived` is broadcast to connected browser clients.
3. A notification is created in the `notifications` table.
4. The user sees an Accept / Ignore prompt in the UI and must explicitly act.

**Library / debugger** (`StorageMessageHandler`):
- Invites are **auto-accepted** immediately. A `GroupInviteAccept` is sent back without user
  confirmation. This is appropriate for automated clients but not for interactive ones.

Both paths deduplicate: if an invite for the same `(group_id, from_peer_id, to_peer_id)` already
exists, the duplicate is ignored.

### Accepting an Invite

`POST /api/group-invites/:id/accept`:

1. The invite status is updated to `"accepted"`.
2. A `MetaMessage::GroupInviteAccept` is sent to the inviter:
   ```json
   { "type": "group_invite_accept", "peer_id": "<accepter's id>", "group_id": "my-group" }
   ```
3. The group key is **not** stored yet — the creator must send it first.

### Ignoring an Invite

`POST /api/group-invites/:id/ignore` sets `status = "ignored"`. No message is sent.

## Group Key Distribution

When the creator receives a `GroupInviteAccept`:

1. The outgoing invite status is updated to `"accepted"`.
2. The new member is added to `group_members`.
3. A `WsEvent::GroupMemberJoined` is broadcast.
4. The creator sends the group key to the new member as an encrypted Direct message:

```json
{
  "type": "group_key_distribution",
  "group_id": "my-group",
  "group_key": "<32-byte key, hex>",
  "key_version": 1,
  "creator_id": "<creator's peer id>"
}
```

### Receiving the Group Key

When a peer receives a `group_key_distribution` Direct message:

1. The consent check verifies that the peer has an accepted invite for this group from this sender.
   If not, the key is discarded with a log warning.
2. If consent is confirmed, the `groups` row is inserted (group key + creator + `key_version`).
3. The recipient is added to `group_members`.
4. The peer can now decrypt any `FriendGroup` messages for this group.

## Sending Group Messages

Once a peer has the group key, they can send group messages:

```
POST /api/messages/group   { "group_id": "my-group", "body": "hello everyone" }
```

The message is encrypted with the group's symmetric key (ChaCha20Poly1305) and broadcast to the
relay as a `MessageKind::FriendGroup` envelope. Recipients who have the key can decrypt it;
those who have not yet accepted their invite will not be able to.

## Adding Members Later

`POST /api/groups/:group_id/members` with `{ "peer_id": "..." }` sends a new `GroupInvite` to
the specified peer. The same invite/accept/key-distribution flow applies.

## REST API Summary

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/api/groups` | GET | List all groups the local peer belongs to |
| `/api/groups` | POST | Create a group `{ group_id, member_ids, message? }` |
| `/api/groups/:id` | GET | Group detail: members + pending invites |
| `/api/groups/:id/members` | POST | Invite a new member `{ peer_id }` |
| `/api/group-invites` | GET | List all invites (filter with `?status=pending`) |
| `/api/group-invites/:id/accept` | POST | Accept an incoming invite |
| `/api/group-invites/:id/ignore` | POST | Ignore an incoming invite |
| `/api/messages/group` | POST | Send a group message `{ group_id, body }` |

## Storage Schema

```sql
CREATE TABLE groups (
    group_id    TEXT PRIMARY KEY,
    group_key   BLOB NOT NULL,      -- 32-byte symmetric key
    creator_id  TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    key_version INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE group_members (
    group_id    TEXT NOT NULL REFERENCES groups(group_id),
    peer_id     TEXT NOT NULL,
    joined_at   INTEGER NOT NULL,
    PRIMARY KEY (group_id, peer_id)
);

CREATE TABLE group_invites (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    group_id        TEXT NOT NULL,
    from_peer_id    TEXT NOT NULL,
    to_peer_id      TEXT NOT NULL,
    status          TEXT NOT NULL DEFAULT 'pending',  -- pending | accepted | ignored
    message         TEXT,
    direction       TEXT NOT NULL,                    -- incoming | outgoing
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL
);
```

## Debugger Usage

The `tenet-debugger` creates and distributes groups entirely in-process (no relay round-trip):

```
tenet-debugger> create-group peer-1 mygroup peer-2 peer-3
created group 'mygroup' with 3 members

tenet-debugger> send-group peer-1 mygroup good morning
sent group message peer-1 → 'mygroup'

tenet-debugger> groups peer-2
  mygroup (3 members, key v1)
```

See [clients/debugger.md](clients/debugger.md) for the full debugger command reference.

## Known Limitations and Deferred Work

- **Key rotation on member removal**: not yet implemented. Removing a member does not re-key the
  group, so removed members can still decrypt future messages if they retained the key.
- **Group name vs group ID**: `group_id` currently doubles as the display name.
- **Inviting non-friends**: the invitee must already be in the local peer registry.
- **Multi-device**: if a user accepts on one device, other devices will not automatically receive
  the group key.
