# Flow: Reactions and Replies

Reactions (upvote / downvote) and threaded replies extend public and group
messages. Both are delivered via the relay like any other envelope and reference
the original message by its `message_id`.

## Reactions

```mermaid
sequenceDiagram
    participant B as Browser
    participant S as Server
    participant DB as SQLite
    participant R as Relay
    participant Au as Author (original poster)

    B->>S: POST /api/messages/{id}/react<br/>{reaction: "upvote"}

    S->>DB: SELECT message FROM messages WHERE id = ?
    DB-->>S: original message (sender_id, kind, …)

    rect rgb(235, 245, 255)
        Note over S: Build reaction payload (Meta message)
        S->>S: Build reaction JSON:<br/>{"type":"reaction",<br/> "message_id": "{id}",<br/> "reaction": "upvote",<br/> "peer_id": own_id}
        S->>S: Sign envelope header
    end

    S->>R: POST /envelopes → Author (Meta kind)
    R->>R: Store in Author's inbox
    S->>DB: UPSERT reactions (message_id, peer_id, reaction)
    S-->>B: 200 OK

    rect rgb(240, 255, 240)
        Note over Au: Author receives reaction
        Au->>R: GET /inbox/{author_peer_id}
        R-->>Au: [reaction Meta envelope]
        Au->>Au: Parse reaction payload
        Au->>DB: UPSERT reactions table
        Au->>Au: Create notification
        Au->>Au: WsEvent::Notification → browser
    end
```

## Threaded Replies

Replies reference the parent message via the `reply_to` field in the envelope
header. They are otherwise sent and received like regular public or group
messages.

```mermaid
sequenceDiagram
    participant B as Browser
    participant S as Server
    participant R as Relay
    participant P as Peers

    B->>S: POST /api/messages/{parent_id}/reply<br/>{body}

    S->>S: Build Public (or FriendGroup) envelope
    Note over S: header.reply_to = parent_id

    rect rgb(235, 245, 255)
        Note over S: Sign and deliver
        S->>S: Sign header with Ed25519
        loop For each known friend (or group member)
            S->>R: POST /envelopes<br/>{kind: Public, reply_to: parent_id}
        end
    end

    S->>S: Store locally (reply_to column set)
    S-->>B: 201 Created {message_id}

    rect rgb(240, 255, 240)
        Note over P: Recipients receive reply
        P->>R: GET /inbox/{peer_id}
        R-->>P: [reply envelope with reply_to]
        P->>P: Verify signature
        P->>P: Store message with reply_to set
        P->>P: WsEvent::NewMessage → browser
        Note over P: UI threads reply under parent message
    end
```

## Removing a Reaction

```mermaid
sequenceDiagram
    participant B as Browser
    participant S as Server
    participant DB as SQLite
    participant R as Relay

    B->>S: DELETE /api/messages/{id}/react

    S->>DB: DELETE FROM reactions WHERE message_id=? AND peer_id=own_id
    S->>S: Build reaction removal payload:<br/>{"type":"reaction_remove", "message_id": "…"}
    S->>R: POST /envelopes → Author (Meta kind)
    S-->>B: 200 OK
```
