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
