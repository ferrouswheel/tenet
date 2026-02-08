# Web UI Architecture Summary

## Build Process (build.rs)

The web UI follows a template-based build approach:

1. Reads three source files from `web/src/`:
   - `index.html` - HTML skeleton with `{{STYLES}}` and `{{SCRIPTS}}` placeholders
   - `styles.css` - All styling
   - `app.js` - All JavaScript logic
2. Inlines CSS and JS directly into HTML template
3. Outputs a single self-contained HTML file to `web/dist/index.html`
4. This happens automatically during `cargo build`

**Key**: The built file is generated and excluded from git. Edits should ALWAYS go to `web/src/` files, not the dist file.

## Web UI Architecture (index.html + styles.css)

The UI is a **dark-themed, single-page application** with the following structure:

### Main Sections

1. **Header** (sticky, always visible)
   - "Tenet" title with status dot (green=OK, red=error)
   - Truncated peer ID display (user's identity)
   - Relay status banner (warns if relay unavailable)

2. **Sidebar** (280px, left side)
   - **My Profile Card** - clickable to edit profile
   - **Peer ID Display** - full ID with copy button
   - **Friends List** - shows friends with online status and last seen
   - **Friend Requests Panel** - collapsible, with pending/history tabs
   - **Add Friend Form** - hidden by default, toggles to accept peer ID and optional message

3. **Main Content** (flex: 1)
   - **Filter Tabs** - Public, Groups
   - **Compose Box** - textarea for message body + attachments + kind selector
   - **Timeline** - scrollable feed of public/group posts with load more button
   - **Conversation Detail** - DM messages with a specific peer (accessed from sidebar)
   - **Post Detail** - single post with comments section (accessed by clicking a post or comment count)
   - **Profile View/Edit** - view friend profiles or edit own profile

## Message Types & Structure

### MessageKind Enum (src/protocol.rs)

- `Public` - Broadcast to all peers
- `Meta` - Protocol metadata (online/ack/friend requests)
- `Direct` - Private 1-to-1 peer messages
- `FriendGroup` - Group messages with group_id
- `StoreForPeer` - Store-and-forward for offline delivery

### Message Object (from API)

```javascript
{
  message_id: "sha256hash...",
  sender_id: "peer-id...",
  recipient_id: "relay-or-peer-id...",
  message_kind: "public|direct|friend_group",
  body: "message text",
  timestamp: 1234567890,
  is_read: boolean,
  upvotes: 0,
  downvotes: 0,
  my_reaction: null|"upvote"|"downvote",
  reply_count: 0,
  reply_to: null|"parent-message-id",
  attachments: [
    {
      content_hash: "base64...",
      filename: "image.png",
      content_type: "image/png",
      size: 1024
    }
  ]
}
```

## Relay Server & Web API (src/relay.rs)

### Protocol Endpoints

| Endpoint | Method | Purpose |
|----------|--------|---------|
| `/health` | GET | Health check |
| `/envelopes` | POST | Store single message envelope |
| `/envelopes/batch` | POST | Store multiple envelopes (batch) |
| `/inbox/{recipient_id}` | GET | Fetch stored messages for a peer |
| `/inbox/batch` | POST | Fetch for multiple peers (batch) |
| `/ws/{recipient_id}` | WS | WebSocket for real-time delivery |

### Web App API Endpoints

| Endpoint | Method | Purpose |
|----------|--------|---------|
| `/api/health` | GET | Get peer ID and relay status |
| `/api/messages` | GET | Fetch paginated messages (with ?kind= and ?before= filters) |
| `/api/messages/public` | POST | Send a public message |
| `/api/messages/direct` | POST | Send a direct message |
| `/api/messages/{id}` | GET | Get single message by ID |
| `/api/messages/{id}/read` | POST | Mark message as read |
| `/api/messages/{id}/reactions` | GET | Get reactions for a message |
| `/api/messages/{id}/react` | POST/DELETE | Add/remove reaction |
| `/api/messages/{id}/reply` | POST | Reply to a message |
| `/api/messages/{id}/replies` | GET | Get replies for a message |
| `/api/conversations` | GET | List all DM conversations |
| `/api/conversations/{peerId}` | GET | Get messages with a peer |
| `/api/peers` | GET | List all friends/peers |
| `/api/peers/{peerId}/profile` | GET | Get peer's profile |
| `/api/profile` | GET/PUT | Get/update own profile |
| `/api/friend-requests` | GET/POST | List/send friend requests |
| `/api/friend-requests/{id}/accept` | POST | Accept friend request |
| `/api/friend-requests/{id}/ignore` | POST | Ignore friend request |
| `/api/friend-requests/{id}/block` | POST | Block peer |
| `/api/attachments` | POST | Upload attachment (multipart) |
| `/api/attachments/{hash}` | GET | Download attachment |
| `/api/ws` | WS | WebSocket for real-time events |

## Views & URL Routing

The app uses hash-based URL routing:

| Hash | View | Description |
|------|------|-------------|
| `#/` or `#/timeline` | Timeline | Public and group posts feed |
| `#/post/{messageId}` | Post Detail | Single post with comments |
| `#/peer/{peerId}` | DM Conversation | Friend info + direct messages |
| (no hash for) | Profile View/Edit | Peer profile or edit own profile |

## WebSocket Events

| Event Type | Payload | Action |
|------------|---------|--------|
| `new_message` | message data | Prepend to timeline if public/group; update conversations |
| `message_read` | message_id | Remove unread styling |
| `peer_online` | peer_id | Update friend online status |
| `peer_offline` | peer_id | Update friend offline status |
| `friend_request_received` | from_peer_id | Refresh requests; show toast |
| `friend_request_accepted` | from_peer_id | Refresh requests and peers; show toast |
| `relay_status` | connected, relay_url | Update relay banner |

## Key Data Structures

### Conversation Object
```javascript
{
  peer_id: "friend-peer-id...",
  display_name: "Alice",
  last_message: "preview text...",
  last_timestamp: 1234567890,
  unread_count: 2
}
```

### Peer Object
```javascript
{
  peer_id: "...",
  display_name: "Alice",
  online: boolean,
  last_seen_online: 1234567890,
  added_at: 1234567890
}
```

### Profile Object
```javascript
{
  peer_id: "...",
  display_name: "Alice",
  bio: "Bio text",
  avatar_hash: "content-hash...",
  public_fields: { key: value },
  friends_fields: { key: value }
}
```

## Feature Status

| Feature | Status |
|---------|--------|
| Core messaging + encryption | Implemented |
| Attachments (files/images) | Implemented |
| Reactions (upvote/downvote) | Implemented |
| Comments/Replies | Implemented |
| Profiles | Implemented |
| Public message timeline | Implemented |
| Direct messages (per-friend) | Implemented |
| Friend management | Implemented |
| Real-time updates (WebSocket) | Implemented |
| Hash-based URL routing | Implemented |
| Friend Groups UI | Partial (protocol exists, basic filtering) |
