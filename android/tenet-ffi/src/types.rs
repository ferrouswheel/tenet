// FFI mirror types for the Tenet–Android boundary.
//
// These are plain structs with only types that UniFFI can handle (String, i64,
// u32, bool, Option<T>, Vec<T>).  They mirror the internal storage row types
// but with no generics, lifetimes, or Rust-specific idioms.

/// A single message as exposed across the FFI boundary.
pub struct FfiMessage {
    pub message_id: String,
    pub sender_id: String,
    pub recipient_id: Option<String>,
    /// "public" | "direct" | "friend_group"
    pub kind: String,
    pub group_id: Option<String>,
    pub body: String,
    pub timestamp: i64,
    pub is_read: bool,
    pub reply_to: Option<String>,
}

/// A known peer as exposed across the FFI boundary.
pub struct FfiPeer {
    pub peer_id: String,
    pub display_name: Option<String>,
    pub is_online: bool,
    pub is_blocked: bool,
    pub is_muted: bool,
    pub is_friend: bool,
    pub last_seen: Option<i64>,
}

/// Summary of a relay sync operation.
pub struct FfiSyncResult {
    pub fetched: u32,
    pub new_messages: u32,
    pub errors: Vec<String>,
}

/// Summary of a single DM conversation.
pub struct FfiConversation {
    pub peer_id: String,
    pub display_name: Option<String>,
    pub last_message: Option<String>,
    pub last_timestamp: i64,
    pub unread_count: u32,
}

/// Aggregated reaction counts plus the local user's own reaction.
pub struct FfiReactionSummary {
    pub upvotes: u32,
    pub downvotes: u32,
    /// "upvote" | "downvote" | None
    pub my_reaction: Option<String>,
}

// ---------------------------------------------------------------------------
// Phase 3 — Social features
// ---------------------------------------------------------------------------

/// A friend request (incoming or outgoing).
pub struct FfiFriendRequest {
    pub id: i64,
    pub from_peer_id: String,
    pub to_peer_id: String,
    /// "pending" | "accepted" | "ignored" | "blocked"
    pub status: String,
    pub message: Option<String>,
    /// "incoming" | "outgoing"
    pub direction: String,
    pub created_at: i64,
}

/// A group the local user belongs to.
pub struct FfiGroup {
    pub group_id: String,
    pub creator_id: String,
    pub member_count: u32,
    pub created_at: i64,
}

/// A single group member.
pub struct FfiGroupMember {
    pub group_id: String,
    pub peer_id: String,
    pub display_name: Option<String>,
    pub joined_at: i64,
}

/// A group invite (incoming or outgoing).
pub struct FfiGroupInvite {
    pub id: i64,
    pub group_id: String,
    pub from_peer_id: String,
    pub to_peer_id: String,
    /// "pending" | "accepted" | "ignored"
    pub status: String,
    pub message: Option<String>,
    /// "incoming" | "outgoing"
    pub direction: String,
    pub created_at: i64,
}

/// A user profile as exposed across the FFI boundary.
pub struct FfiProfile {
    pub user_id: String,
    pub display_name: Option<String>,
    pub bio: Option<String>,
    /// Content hash for avatar image (fetch via download_attachment).
    pub avatar_hash: Option<String>,
    /// JSON string of public profile fields.
    pub public_fields: String,
    /// JSON string of friends-only profile fields.
    pub friends_fields: String,
    pub updated_at: i64,
}

/// A single notification.
pub struct FfiNotification {
    pub id: i64,
    /// "direct_message" | "reply" | "reaction" | "friend_request" | "group_invite"
    pub notification_type: String,
    pub message_id: String,
    pub sender_id: String,
    pub created_at: i64,
    pub is_read: bool,
}

// ---------------------------------------------------------------------------
// Multiple identities
// ---------------------------------------------------------------------------

/// A local identity entry as exposed across the FFI boundary.
pub struct FfiIdentity {
    /// Short ID used as the identity's directory name (first 12 chars of peer ID).
    pub short_id: String,
    /// Full peer ID (hex-encoded public key).
    pub peer_id: String,
    /// The relay URL stored for this identity, if any.
    pub relay_url: Option<String>,
    /// Whether this identity is the current default.
    pub is_default: bool,
}
