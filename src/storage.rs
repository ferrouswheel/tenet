//! SQLite storage layer for tenet.
//!
//! Provides a shared database that both the web server and CLI can use.
//! Handles schema creation, CRUD operations for all entity types, and
//! one-time migration from the legacy JSON/JSONL file format.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::crypto::StoredKeypair;
use crate::protocol::{Envelope, MessageKind};

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum StorageError {
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
    Serde(serde_json::Error),
    NotFound(String),
    AlreadyExists(String),
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StorageError::Sqlite(e) => write!(f, "sqlite error: {e}"),
            StorageError::Io(e) => write!(f, "io error: {e}"),
            StorageError::Serde(e) => write!(f, "serialization error: {e}"),
            StorageError::NotFound(msg) => write!(f, "not found: {msg}"),
            StorageError::AlreadyExists(msg) => write!(f, "already exists: {msg}"),
        }
    }
}

impl std::error::Error for StorageError {}

impl From<rusqlite::Error> for StorageError {
    fn from(e: rusqlite::Error) -> Self {
        StorageError::Sqlite(e)
    }
}

impl From<std::io::Error> for StorageError {
    fn from(e: std::io::Error) -> Self {
        StorageError::Io(e)
    }
}

impl From<serde_json::Error> for StorageError {
    fn from(e: serde_json::Error) -> Self {
        StorageError::Serde(e)
    }
}

// ---------------------------------------------------------------------------
// Row types
// ---------------------------------------------------------------------------

/// Identity row stored in the database.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdentityRow {
    pub id: String,
    pub public_key: String,
    pub private_key: String,
    pub signing_pub: String,
    pub signing_priv: String,
    pub created_at: u64,
}

impl From<&StoredKeypair> for IdentityRow {
    fn from(kp: &StoredKeypair) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Self {
            id: kp.id.clone(),
            public_key: kp.public_key_hex.clone(),
            private_key: kp.private_key_hex.clone(),
            signing_pub: kp.signing_public_key_hex.clone(),
            signing_priv: kp.signing_private_key_hex.clone(),
            created_at: now,
        }
    }
}

impl IdentityRow {
    pub fn to_stored_keypair(&self) -> StoredKeypair {
        StoredKeypair {
            id: self.id.clone(),
            public_key_hex: self.public_key.clone(),
            private_key_hex: self.private_key.clone(),
            signing_public_key_hex: self.signing_pub.clone(),
            signing_private_key_hex: self.signing_priv.clone(),
        }
    }
}

/// Peer row stored in the database.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerRow {
    pub peer_id: String,
    pub display_name: Option<String>,
    pub signing_public_key: String,
    pub encryption_public_key: Option<String>,
    pub added_at: u64,
    pub is_friend: bool,
    pub last_seen_online: Option<u64>,
    pub online: bool,
}

/// Message row stored in the database.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageRow {
    pub message_id: String,
    pub sender_id: String,
    pub recipient_id: String,
    pub message_kind: String,
    pub group_id: Option<String>,
    pub body: Option<String>,
    pub timestamp: u64,
    pub received_at: u64,
    pub ttl_seconds: u64,
    pub is_read: bool,
    pub raw_envelope: Option<String>,
    /// Phase 9: Optional reference to the parent message this is a reply to.
    #[serde(default)]
    pub reply_to: Option<String>,
}

/// Group row stored in the database.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupRow {
    pub group_id: String,
    pub group_key: Vec<u8>,
    pub creator_id: String,
    pub created_at: u64,
    pub key_version: u32,
}

/// Group member row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupMemberRow {
    pub group_id: String,
    pub peer_id: String,
    pub joined_at: u64,
}

/// Outbox row for sent messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboxRow {
    pub message_id: String,
    pub envelope: String,
    pub sent_at: u64,
    pub delivered: bool,
}

/// Relay configuration row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayRow {
    pub url: String,
    pub added_at: u64,
    pub last_sync: Option<u64>,
    pub enabled: bool,
}

/// Attachment row stored in the database (Phase 7).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentRow {
    pub content_hash: String,
    pub content_type: String,
    pub size_bytes: u64,
    #[serde(skip)]
    pub data: Vec<u8>,
    pub created_at: u64,
}

/// Link between a message and its attachments (Phase 7).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageAttachmentRow {
    pub message_id: String,
    pub content_hash: String,
    pub filename: Option<String>,
    pub position: u32,
}

/// Conversation summary for the conversations list view (Phase 5).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationSummary {
    pub peer_id: String,
    pub last_timestamp: u64,
    pub last_message: Option<String>,
    pub unread_count: u32,
}

/// Reaction row stored in the database (Phase 8).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReactionRow {
    pub message_id: String,
    pub target_id: String,
    pub sender_id: String,
    pub reaction: String,
    pub timestamp: u64,
}

/// Profile row stored in the database (Phase 10).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileRow {
    pub user_id: String,
    pub display_name: Option<String>,
    pub bio: Option<String>,
    pub avatar_hash: Option<String>,
    pub public_fields: String,
    pub friends_fields: String,
    pub updated_at: u64,
}

/// Friend request row stored in the database.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FriendRequestRow {
    pub id: i64,
    pub from_peer_id: String,
    pub to_peer_id: String,
    /// "pending", "accepted", "ignored", "blocked"
    pub status: String,
    pub message: Option<String>,
    pub from_signing_key: String,
    pub from_encryption_key: String,
    /// "incoming" or "outgoing" from the local peer's perspective
    pub direction: String,
    pub created_at: u64,
    pub updated_at: u64,
}

/// Group invite row stored in the database.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupInviteRow {
    pub id: i64,
    pub group_id: String,
    pub from_peer_id: String,
    pub to_peer_id: String,
    /// "pending", "accepted", "ignored"
    pub status: String,
    pub message: Option<String>,
    /// "incoming" or "outgoing" from the local peer's perspective
    pub direction: String,
    pub created_at: u64,
    pub updated_at: u64,
}

/// Notification row stored in the database (Phase 11).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationRow {
    pub id: i64,
    /// "direct_message", "reply", "reaction", "friend_request", "group_invite"
    pub notification_type: String,
    pub message_id: String,
    pub sender_id: String,
    pub created_at: u64,
    /// Whether the user has opened the notification panel and seen this item.
    pub seen: bool,
    /// Whether the user has clicked through and acted on this notification.
    pub read: bool,
}

// ---------------------------------------------------------------------------
// Storage handle
// ---------------------------------------------------------------------------

/// Main storage handle wrapping a SQLite connection.
pub struct Storage {
    conn: Connection,
    /// Directory on disk where attachment files are stored.
    pub attachment_dir: PathBuf,
}

impl Storage {
    /// Open or create a database at the given path. Creates schema if needed.
    /// Attachments are stored as files in an `attachments/` subdirectory
    /// alongside the database file.
    pub fn open(path: &Path) -> Result<Self, StorageError> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        let attachment_dir = path.parent().unwrap_or(Path::new(".")).join("attachments");
        std::fs::create_dir_all(&attachment_dir)?;
        let storage = Self {
            conn,
            attachment_dir,
        };
        storage.create_schema()?;
        Ok(storage)
    }

    /// Create an in-memory database with an explicit attachment directory.
    /// The caller is responsible for providing an appropriate path; this
    /// function never chooses a location autonomously.
    pub fn open_in_memory(attachment_dir: &Path) -> Result<Self, StorageError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        std::fs::create_dir_all(attachment_dir)?;
        let storage = Self {
            conn,
            attachment_dir: attachment_dir.to_path_buf(),
        };
        storage.create_schema()?;
        Ok(storage)
    }

    fn create_schema(&self) -> Result<(), StorageError> {
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS identity (
                id          TEXT PRIMARY KEY,
                public_key  TEXT NOT NULL,
                private_key TEXT NOT NULL,
                signing_pub TEXT NOT NULL,
                signing_priv TEXT NOT NULL,
                created_at  INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS peers (
                peer_id             TEXT PRIMARY KEY,
                display_name        TEXT,
                signing_public_key  TEXT NOT NULL,
                encryption_public_key TEXT,
                added_at            INTEGER NOT NULL,
                is_friend           INTEGER NOT NULL DEFAULT 1,
                last_seen_online    INTEGER,
                online              INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS messages (
                message_id      TEXT PRIMARY KEY,
                sender_id       TEXT NOT NULL,
                recipient_id    TEXT NOT NULL,
                message_kind    TEXT NOT NULL,
                group_id        TEXT,
                body            TEXT,
                timestamp       INTEGER NOT NULL,
                received_at     INTEGER NOT NULL,
                ttl_seconds     INTEGER NOT NULL,
                is_read         INTEGER NOT NULL DEFAULT 0,
                raw_envelope    TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_messages_recipient
                ON messages(recipient_id, timestamp);
            CREATE INDEX IF NOT EXISTS idx_messages_sender
                ON messages(sender_id, timestamp);
            CREATE INDEX IF NOT EXISTS idx_messages_kind
                ON messages(message_kind, timestamp);
            CREATE INDEX IF NOT EXISTS idx_messages_group
                ON messages(group_id, timestamp);

            CREATE TABLE IF NOT EXISTS groups (
                group_id    TEXT PRIMARY KEY,
                group_key   BLOB NOT NULL,
                creator_id  TEXT NOT NULL,
                created_at  INTEGER NOT NULL,
                key_version INTEGER NOT NULL DEFAULT 1
            );

            CREATE TABLE IF NOT EXISTS group_members (
                group_id    TEXT NOT NULL REFERENCES groups(group_id),
                peer_id     TEXT NOT NULL,
                joined_at   INTEGER NOT NULL,
                PRIMARY KEY (group_id, peer_id)
            );

            CREATE TABLE IF NOT EXISTS outbox (
                message_id  TEXT PRIMARY KEY,
                envelope    TEXT NOT NULL,
                sent_at     INTEGER NOT NULL,
                delivered   INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS relays (
                url         TEXT PRIMARY KEY,
                added_at    INTEGER NOT NULL,
                last_sync   INTEGER,
                enabled     INTEGER NOT NULL DEFAULT 1
            );

            CREATE TABLE IF NOT EXISTS attachments (
                content_hash    TEXT PRIMARY KEY,
                content_type    TEXT NOT NULL,
                size_bytes      INTEGER NOT NULL,
                created_at      INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS message_attachments (
                message_id      TEXT NOT NULL REFERENCES messages(message_id),
                content_hash    TEXT NOT NULL REFERENCES attachments(content_hash),
                filename        TEXT,
                position        INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (message_id, content_hash)
            );

            -- Reactions (Phase 8)
            CREATE TABLE IF NOT EXISTS reactions (
                message_id      TEXT NOT NULL,
                target_id       TEXT NOT NULL,
                sender_id       TEXT NOT NULL,
                reaction        TEXT NOT NULL,
                timestamp       INTEGER NOT NULL,
                PRIMARY KEY (target_id, sender_id)
            );

            -- Friend Requests
            CREATE TABLE IF NOT EXISTS friend_requests (
                id                  INTEGER PRIMARY KEY AUTOINCREMENT,
                from_peer_id        TEXT NOT NULL,
                to_peer_id          TEXT NOT NULL,
                status              TEXT NOT NULL DEFAULT 'pending',
                message             TEXT,
                from_signing_key    TEXT NOT NULL,
                from_encryption_key TEXT NOT NULL,
                direction           TEXT NOT NULL,
                created_at          INTEGER NOT NULL,
                updated_at          INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_friend_requests_to
                ON friend_requests(to_peer_id, status);
            CREATE INDEX IF NOT EXISTS idx_friend_requests_from
                ON friend_requests(from_peer_id, status);

            -- Group Invites
            CREATE TABLE IF NOT EXISTS group_invites (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                group_id        TEXT NOT NULL,
                from_peer_id    TEXT NOT NULL,
                to_peer_id      TEXT NOT NULL,
                status          TEXT NOT NULL DEFAULT 'pending',
                message         TEXT,
                direction       TEXT NOT NULL,
                created_at      INTEGER NOT NULL,
                updated_at      INTEGER NOT NULL
            );

            CREATE UNIQUE INDEX IF NOT EXISTS idx_group_invites_unique
                ON group_invites(group_id, from_peer_id, to_peer_id, direction);
            CREATE INDEX IF NOT EXISTS idx_group_invites_to
                ON group_invites(to_peer_id, status);
            CREATE INDEX IF NOT EXISTS idx_group_invites_from
                ON group_invites(from_peer_id, status);

            -- Profiles (Phase 10)
            CREATE TABLE IF NOT EXISTS profiles (
                user_id         TEXT PRIMARY KEY,
                display_name    TEXT,
                bio             TEXT,
                avatar_hash     TEXT,
                public_fields   TEXT NOT NULL DEFAULT '{}',
                friends_fields  TEXT NOT NULL DEFAULT '{}',
                updated_at      INTEGER NOT NULL
            );

            -- Notifications (Phase 11)
            CREATE TABLE IF NOT EXISTS notifications (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                type            TEXT NOT NULL,
                message_id      TEXT NOT NULL,
                sender_id       TEXT NOT NULL,
                created_at      INTEGER NOT NULL,
                seen            INTEGER NOT NULL DEFAULT 0,
                read            INTEGER NOT NULL DEFAULT 0
            );

            CREATE INDEX IF NOT EXISTS idx_notifications_unread
                ON notifications(read, created_at);
            ",
        )?;

        // Phase 9: Add reply_to column to messages if not present
        // We check if the column exists first to avoid errors on existing databases
        let has_reply_to: bool = self
            .conn
            .prepare("SELECT reply_to FROM messages LIMIT 0")
            .is_ok();
        if !has_reply_to {
            self.conn
                .execute_batch("ALTER TABLE messages ADD COLUMN reply_to TEXT;")?;
        }

        // Create reply_to index after the column exists
        self.conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_messages_reply_to ON messages(reply_to);",
        )?;

        // Migration: add seen column to notifications if not present (existing DBs won't have it)
        let has_seen_col: bool = self
            .conn
            .prepare("SELECT seen FROM notifications LIMIT 0")
            .is_ok();
        if !has_seen_col {
            self.conn.execute_batch(
                "ALTER TABLE notifications ADD COLUMN seen INTEGER NOT NULL DEFAULT 0;",
            )?;
        }
        self.conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_notifications_unseen ON notifications(seen, created_at);",
        )?;

        // Migration: move any existing attachment blobs from DB to filesystem,
        // then drop the legacy `data` column. Runs only if the column still exists.
        let has_data_col = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('attachments') WHERE name='data'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0;
        if has_data_col {
            let mut stmt = self.conn.prepare(
                "SELECT content_hash, content_type, data FROM attachments
                 WHERE data IS NOT NULL AND length(data) > 0",
            )?;
            let rows: Vec<(String, String, Vec<u8>)> = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                })?
                .filter_map(|r| r.ok())
                .collect();
            for (hash, ct, data) in rows {
                let path = self.attachment_path(&hash, &ct);
                if !path.exists() {
                    if let Some(parent) = path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    let _ = std::fs::write(&path, &data);
                }
            }
            let _ = self
                .conn
                .execute_batch("ALTER TABLE attachments DROP COLUMN data;");
        }

        Ok(())
    }

    /// Derive the filesystem path for an attachment file.
    ///
    /// Uses a two-level directory prefix (`hash[0..2] / hash[2..4]`) to avoid
    /// large flat directories, and appends an extension derived from the
    /// content type.
    fn attachment_path(&self, content_hash: &str, content_type: &str) -> PathBuf {
        let ext = content_type_to_ext(content_type);
        let (d1, d2) = if content_hash.len() >= 4 {
            (&content_hash[..2], &content_hash[2..4])
        } else {
            (&content_hash[..content_hash.len().min(2)], "xx")
        };
        self.attachment_dir
            .join(d1)
            .join(d2)
            .join(format!("{content_hash}.{ext}"))
    }

    // -----------------------------------------------------------------------
    // Identity CRUD
    // -----------------------------------------------------------------------

    /// Insert a new identity. Returns error if one already exists.
    pub fn insert_identity(&self, row: &IdentityRow) -> Result<(), StorageError> {
        self.conn.execute(
            "INSERT INTO identity (id, public_key, private_key, signing_pub, signing_priv, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                row.id,
                row.public_key,
                row.private_key,
                row.signing_pub,
                row.signing_priv,
                row.created_at as i64,
            ],
        )?;
        Ok(())
    }

    /// Load the first identity from the database (single-user model).
    pub fn get_identity(&self) -> Result<Option<IdentityRow>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, public_key, private_key, signing_pub, signing_priv, created_at
             FROM identity LIMIT 1",
        )?;
        let row = stmt
            .query_row([], |row| {
                Ok(IdentityRow {
                    id: row.get(0)?,
                    public_key: row.get(1)?,
                    private_key: row.get(2)?,
                    signing_pub: row.get(3)?,
                    signing_priv: row.get(4)?,
                    created_at: row.get::<_, i64>(5)? as u64,
                })
            })
            .optional()?;
        Ok(row)
    }

    // -----------------------------------------------------------------------
    // Peers CRUD
    // -----------------------------------------------------------------------

    pub fn insert_peer(&self, row: &PeerRow) -> Result<(), StorageError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO peers
             (peer_id, display_name, signing_public_key, encryption_public_key,
              added_at, is_friend, last_seen_online, online)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                row.peer_id,
                row.display_name,
                row.signing_public_key,
                row.encryption_public_key,
                row.added_at as i64,
                row.is_friend as i32,
                row.last_seen_online.map(|t| t as i64),
                row.online as i32,
            ],
        )?;
        Ok(())
    }

    pub fn get_peer(&self, peer_id: &str) -> Result<Option<PeerRow>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT peer_id, display_name, signing_public_key, encryption_public_key,
                    added_at, is_friend, last_seen_online, online
             FROM peers WHERE peer_id = ?1",
        )?;
        let row = stmt
            .query_row(params![peer_id], |row| {
                Ok(PeerRow {
                    peer_id: row.get(0)?,
                    display_name: row.get(1)?,
                    signing_public_key: row.get(2)?,
                    encryption_public_key: row.get(3)?,
                    added_at: row.get::<_, i64>(4)? as u64,
                    is_friend: row.get::<_, i32>(5)? != 0,
                    last_seen_online: row.get::<_, Option<i64>>(6)?.map(|t| t as u64),
                    online: row.get::<_, i32>(7)? != 0,
                })
            })
            .optional()?;
        Ok(row)
    }

    pub fn list_peers(&self) -> Result<Vec<PeerRow>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT peer_id, display_name, signing_public_key, encryption_public_key,
                    added_at, is_friend, last_seen_online, online
             FROM peers ORDER BY added_at",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(PeerRow {
                peer_id: row.get(0)?,
                display_name: row.get(1)?,
                signing_public_key: row.get(2)?,
                encryption_public_key: row.get(3)?,
                added_at: row.get::<_, i64>(4)? as u64,
                is_friend: row.get::<_, i32>(5)? != 0,
                last_seen_online: row.get::<_, Option<i64>>(6)?.map(|t| t as u64),
                online: row.get::<_, i32>(7)? != 0,
            })
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    pub fn delete_peer(&self, peer_id: &str) -> Result<bool, StorageError> {
        let affected = self
            .conn
            .execute("DELETE FROM peers WHERE peer_id = ?1", params![peer_id])?;
        Ok(affected > 0)
    }

    pub fn update_peer_online(
        &self,
        peer_id: &str,
        online: bool,
        last_seen: u64,
    ) -> Result<bool, StorageError> {
        let affected = self.conn.execute(
            "UPDATE peers SET online = ?1, last_seen_online = ?2 WHERE peer_id = ?3",
            params![online as i32, last_seen as i64, peer_id],
        )?;
        Ok(affected > 0)
    }

    // -----------------------------------------------------------------------
    // Messages CRUD
    // -----------------------------------------------------------------------

    pub fn insert_message(&self, row: &MessageRow) -> Result<(), StorageError> {
        self.conn.execute(
            "INSERT OR IGNORE INTO messages
             (message_id, sender_id, recipient_id, message_kind, group_id,
              body, timestamp, received_at, ttl_seconds, is_read, raw_envelope, reply_to)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                row.message_id,
                row.sender_id,
                row.recipient_id,
                row.message_kind,
                row.group_id,
                row.body,
                row.timestamp as i64,
                row.received_at as i64,
                row.ttl_seconds as i64,
                row.is_read as i32,
                row.raw_envelope,
                row.reply_to,
            ],
        )?;
        Ok(())
    }

    pub fn get_message(&self, message_id: &str) -> Result<Option<MessageRow>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT message_id, sender_id, recipient_id, message_kind, group_id,
                    body, timestamp, received_at, ttl_seconds, is_read, raw_envelope, reply_to
             FROM messages WHERE message_id = ?1",
        )?;
        let row = stmt
            .query_row(params![message_id], |row| {
                Ok(MessageRow {
                    message_id: row.get(0)?,
                    sender_id: row.get(1)?,
                    recipient_id: row.get(2)?,
                    message_kind: row.get(3)?,
                    group_id: row.get(4)?,
                    body: row.get(5)?,
                    timestamp: row.get::<_, i64>(6)? as u64,
                    received_at: row.get::<_, i64>(7)? as u64,
                    ttl_seconds: row.get::<_, i64>(8)? as u64,
                    is_read: row.get::<_, i32>(9)? != 0,
                    raw_envelope: row.get(10)?,
                    reply_to: row.get(11)?,
                })
            })
            .optional()?;
        Ok(row)
    }

    /// List messages with optional filters.
    pub fn list_messages(
        &self,
        kind: Option<&str>,
        group_id: Option<&str>,
        before: Option<u64>,
        limit: u32,
    ) -> Result<Vec<MessageRow>, StorageError> {
        let mut sql = String::from(
            "SELECT message_id, sender_id, recipient_id, message_kind, group_id,
                    body, timestamp, received_at, ttl_seconds, is_read, raw_envelope, reply_to
             FROM messages WHERE 1=1",
        );
        let mut bind_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        if let Some(k) = kind {
            sql.push_str(" AND message_kind = ?");
            bind_values.push(Box::new(k.to_string()));
        }
        if let Some(g) = group_id {
            sql.push_str(" AND group_id = ?");
            bind_values.push(Box::new(g.to_string()));
        }
        if let Some(b) = before {
            sql.push_str(" AND timestamp < ?");
            bind_values.push(Box::new(b as i64));
        }
        sql.push_str(" ORDER BY timestamp DESC LIMIT ?");
        bind_values.push(Box::new(limit as i64));

        let mut stmt = self.conn.prepare(&sql)?;
        let bind_refs: Vec<&dyn rusqlite::types::ToSql> =
            bind_values.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(bind_refs.as_slice(), |row| {
            Ok(MessageRow {
                message_id: row.get(0)?,
                sender_id: row.get(1)?,
                recipient_id: row.get(2)?,
                message_kind: row.get(3)?,
                group_id: row.get(4)?,
                body: row.get(5)?,
                timestamp: row.get::<_, i64>(6)? as u64,
                received_at: row.get::<_, i64>(7)? as u64,
                ttl_seconds: row.get::<_, i64>(8)? as u64,
                is_read: row.get::<_, i32>(9)? != 0,
                raw_envelope: row.get(10)?,
                reply_to: row.get(11)?,
            })
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    pub fn has_message(&self, message_id: &str) -> Result<bool, StorageError> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE message_id = ?1",
            params![message_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn mark_message_read(&self, message_id: &str) -> Result<bool, StorageError> {
        let affected = self.conn.execute(
            "UPDATE messages SET is_read = 1 WHERE message_id = ?1",
            params![message_id],
        )?;
        Ok(affected > 0)
    }

    // -----------------------------------------------------------------------
    // Conversations (Phase 5)
    // -----------------------------------------------------------------------

    /// List all direct message conversations grouped by peer.
    /// Returns one entry per peer with the most recent message details.
    pub fn list_conversations(
        &self,
        identity_id: &str,
    ) -> Result<Vec<ConversationSummary>, StorageError> {
        let sql = "
            SELECT
                CASE
                    WHEN sender_id = ?1 THEN recipient_id
                    ELSE sender_id
                END as peer_id,
                MAX(timestamp) as last_timestamp,
                (SELECT body FROM messages m2
                 WHERE message_kind = 'direct'
                   AND ((m2.sender_id = ?1 AND m2.recipient_id = peer_id)
                     OR (m2.sender_id = peer_id AND m2.recipient_id = ?1))
                 ORDER BY timestamp DESC LIMIT 1) as last_message,
                (SELECT COUNT(*) FROM messages m3
                 WHERE message_kind = 'direct'
                   AND m3.sender_id = peer_id
                   AND m3.recipient_id = ?1
                   AND is_read = 0) as unread_count
            FROM messages
            WHERE message_kind = 'direct'
              AND (sender_id = ?1 OR recipient_id = ?1)
            GROUP BY peer_id
            ORDER BY last_timestamp DESC
        ";

        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(params![identity_id], |row| {
            Ok(ConversationSummary {
                peer_id: row.get(0)?,
                last_timestamp: row.get::<_, i64>(1)? as u64,
                last_message: row.get(2)?,
                unread_count: row.get::<_, i64>(3)? as u32,
            })
        })?;

        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// List all messages in a conversation with a specific peer.
    pub fn list_conversation_messages(
        &self,
        identity_id: &str,
        peer_id: &str,
        before: Option<u64>,
        limit: u32,
    ) -> Result<Vec<MessageRow>, StorageError> {
        let mut sql = String::from(
            "SELECT message_id, sender_id, recipient_id, message_kind, group_id,
                    body, timestamp, received_at, ttl_seconds, is_read, raw_envelope, reply_to
             FROM messages
             WHERE message_kind = 'direct'
               AND ((sender_id = ?1 AND recipient_id = ?2)
                 OR (sender_id = ?2 AND recipient_id = ?1))",
        );

        let mut bind_values: Vec<Box<dyn rusqlite::types::ToSql>> = vec![
            Box::new(identity_id.to_string()),
            Box::new(peer_id.to_string()),
        ];

        if let Some(b) = before {
            sql.push_str(" AND timestamp < ?");
            bind_values.push(Box::new(b as i64));
        }

        sql.push_str(" ORDER BY timestamp DESC LIMIT ?");
        bind_values.push(Box::new(limit as i64));

        let mut stmt = self.conn.prepare(&sql)?;
        let bind_refs: Vec<&dyn rusqlite::types::ToSql> =
            bind_values.iter().map(|b| b.as_ref()).collect();

        let rows = stmt.query_map(bind_refs.as_slice(), |row| {
            Ok(MessageRow {
                message_id: row.get(0)?,
                sender_id: row.get(1)?,
                recipient_id: row.get(2)?,
                message_kind: row.get(3)?,
                group_id: row.get(4)?,
                body: row.get(5)?,
                timestamp: row.get::<_, i64>(6)? as u64,
                received_at: row.get::<_, i64>(7)? as u64,
                ttl_seconds: row.get::<_, i64>(8)? as u64,
                is_read: row.get::<_, i32>(9)? != 0,
                raw_envelope: row.get(10)?,
                reply_to: row.get(11)?,
            })
        })?;

        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    // -----------------------------------------------------------------------
    // Groups CRUD
    // -----------------------------------------------------------------------

    pub fn insert_group(&self, row: &GroupRow) -> Result<(), StorageError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO groups (group_id, group_key, creator_id, created_at, key_version)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                row.group_id,
                row.group_key,
                row.creator_id,
                row.created_at as i64,
                row.key_version as i64,
            ],
        )?;
        Ok(())
    }

    /// Insert a group and its members atomically within a transaction.
    /// This ensures that if any member insertion fails, the entire operation is rolled back.
    pub fn insert_group_with_members(
        &self,
        group: &GroupRow,
        members: &[GroupMemberRow],
    ) -> Result<(), StorageError> {
        let tx = self.conn.unchecked_transaction()?;

        // Insert group
        tx.execute(
            "INSERT OR REPLACE INTO groups (group_id, group_key, creator_id, created_at, key_version)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                group.group_id,
                group.group_key,
                group.creator_id,
                group.created_at as i64,
                group.key_version as i64,
            ],
        )?;

        // Insert all members
        for member in members {
            tx.execute(
                "INSERT OR IGNORE INTO group_members (group_id, peer_id, joined_at)
                 VALUES (?1, ?2, ?3)",
                params![member.group_id, member.peer_id, member.joined_at as i64],
            )?;
        }

        tx.commit()?;
        Ok(())
    }

    pub fn get_group(&self, group_id: &str) -> Result<Option<GroupRow>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT group_id, group_key, creator_id, created_at, key_version
             FROM groups WHERE group_id = ?1",
        )?;
        let row = stmt
            .query_row(params![group_id], |row| {
                Ok(GroupRow {
                    group_id: row.get(0)?,
                    group_key: row.get(1)?,
                    creator_id: row.get(2)?,
                    created_at: row.get::<_, i64>(3)? as u64,
                    key_version: row.get::<_, i64>(4)? as u32,
                })
            })
            .optional()?;
        Ok(row)
    }

    pub fn list_groups(&self) -> Result<Vec<GroupRow>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT group_id, group_key, creator_id, created_at, key_version
             FROM groups ORDER BY created_at",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(GroupRow {
                group_id: row.get(0)?,
                group_key: row.get(1)?,
                creator_id: row.get(2)?,
                created_at: row.get::<_, i64>(3)? as u64,
                key_version: row.get::<_, i64>(4)? as u32,
            })
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    pub fn delete_group(&self, group_id: &str) -> Result<bool, StorageError> {
        self.conn.execute(
            "DELETE FROM group_members WHERE group_id = ?1",
            params![group_id],
        )?;
        let affected = self
            .conn
            .execute("DELETE FROM groups WHERE group_id = ?1", params![group_id])?;
        Ok(affected > 0)
    }

    // -----------------------------------------------------------------------
    // Group members
    // -----------------------------------------------------------------------

    pub fn insert_group_member(&self, row: &GroupMemberRow) -> Result<(), StorageError> {
        self.conn.execute(
            "INSERT OR IGNORE INTO group_members (group_id, peer_id, joined_at)
             VALUES (?1, ?2, ?3)",
            params![row.group_id, row.peer_id, row.joined_at as i64],
        )?;
        Ok(())
    }

    pub fn list_group_members(&self, group_id: &str) -> Result<Vec<GroupMemberRow>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT group_id, peer_id, joined_at
             FROM group_members WHERE group_id = ?1 ORDER BY joined_at",
        )?;
        let rows = stmt.query_map(params![group_id], |row| {
            Ok(GroupMemberRow {
                group_id: row.get(0)?,
                peer_id: row.get(1)?,
                joined_at: row.get::<_, i64>(2)? as u64,
            })
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    pub fn remove_group_member(&self, group_id: &str, peer_id: &str) -> Result<bool, StorageError> {
        let affected = self.conn.execute(
            "DELETE FROM group_members WHERE group_id = ?1 AND peer_id = ?2",
            params![group_id, peer_id],
        )?;
        Ok(affected > 0)
    }

    // -----------------------------------------------------------------------
    // Outbox CRUD
    // -----------------------------------------------------------------------

    pub fn insert_outbox(&self, row: &OutboxRow) -> Result<(), StorageError> {
        self.conn.execute(
            "INSERT OR IGNORE INTO outbox (message_id, envelope, sent_at, delivered)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                row.message_id,
                row.envelope,
                row.sent_at as i64,
                row.delivered as i32,
            ],
        )?;
        Ok(())
    }

    pub fn list_outbox(&self) -> Result<Vec<OutboxRow>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT message_id, envelope, sent_at, delivered
             FROM outbox ORDER BY sent_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(OutboxRow {
                message_id: row.get(0)?,
                envelope: row.get(1)?,
                sent_at: row.get::<_, i64>(2)? as u64,
                delivered: row.get::<_, i32>(3)? != 0,
            })
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    pub fn mark_outbox_delivered(&self, message_id: &str) -> Result<bool, StorageError> {
        let affected = self.conn.execute(
            "UPDATE outbox SET delivered = 1 WHERE message_id = ?1",
            params![message_id],
        )?;
        Ok(affected > 0)
    }

    // -----------------------------------------------------------------------
    // Relays CRUD
    // -----------------------------------------------------------------------

    pub fn insert_relay(&self, row: &RelayRow) -> Result<(), StorageError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO relays (url, added_at, last_sync, enabled)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                row.url,
                row.added_at as i64,
                row.last_sync.map(|t| t as i64),
                row.enabled as i32,
            ],
        )?;
        Ok(())
    }

    pub fn list_relays(&self) -> Result<Vec<RelayRow>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT url, added_at, last_sync, enabled
             FROM relays ORDER BY added_at",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(RelayRow {
                url: row.get(0)?,
                added_at: row.get::<_, i64>(1)? as u64,
                last_sync: row.get::<_, Option<i64>>(2)?.map(|t| t as u64),
                enabled: row.get::<_, i32>(3)? != 0,
            })
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    pub fn list_enabled_relays(&self) -> Result<Vec<RelayRow>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT url, added_at, last_sync, enabled
             FROM relays WHERE enabled = 1 ORDER BY added_at",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(RelayRow {
                url: row.get(0)?,
                added_at: row.get::<_, i64>(1)? as u64,
                last_sync: row.get::<_, Option<i64>>(2)?.map(|t| t as u64),
                enabled: row.get::<_, i32>(3)? != 0,
            })
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    pub fn update_relay_last_sync(&self, url: &str, last_sync: u64) -> Result<bool, StorageError> {
        let affected = self.conn.execute(
            "UPDATE relays SET last_sync = ?1 WHERE url = ?2",
            params![last_sync as i64, url],
        )?;
        Ok(affected > 0)
    }

    pub fn delete_relay(&self, url: &str) -> Result<bool, StorageError> {
        let affected = self
            .conn
            .execute("DELETE FROM relays WHERE url = ?1", params![url])?;
        Ok(affected > 0)
    }

    // -----------------------------------------------------------------------
    // Attachments CRUD (Phase 7)
    // -----------------------------------------------------------------------

    /// Insert an attachment. Writes data to the filesystem and stores
    /// metadata in the database. Content-addressed, so duplicate inserts
    /// are silently ignored.
    pub fn insert_attachment(&self, row: &AttachmentRow) -> Result<(), StorageError> {
        let path = self.attachment_path(&row.content_hash, &row.content_type);
        if !path.exists() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, &row.data)?;
        }
        self.conn.execute(
            "INSERT OR IGNORE INTO attachments
             (content_hash, content_type, size_bytes, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                row.content_hash,
                row.content_type,
                row.size_bytes as i64,
                row.created_at as i64,
            ],
        )?;
        Ok(())
    }

    /// Retrieve an attachment by its content hash. Returns `None` if not
    /// found in the database or if the file is missing from disk.
    pub fn get_attachment(
        &self,
        content_hash: &str,
    ) -> Result<Option<AttachmentRow>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT content_hash, content_type, size_bytes, created_at
             FROM attachments WHERE content_hash = ?1",
        )?;
        let meta = stmt
            .query_row(params![content_hash], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)? as u64,
                    row.get::<_, i64>(3)? as u64,
                ))
            })
            .optional()?;
        match meta {
            None => Ok(None),
            Some((content_hash, content_type, size_bytes, created_at)) => {
                let path = self.attachment_path(&content_hash, &content_type);
                match std::fs::read(&path) {
                    Ok(data) => Ok(Some(AttachmentRow {
                        content_hash,
                        content_type,
                        size_bytes,
                        data,
                        created_at,
                    })),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(e) => Err(StorageError::Io(e)),
                }
            }
        }
    }

    /// Link an attachment to a message.
    pub fn insert_message_attachment(
        &self,
        row: &MessageAttachmentRow,
    ) -> Result<(), StorageError> {
        self.conn.execute(
            "INSERT OR IGNORE INTO message_attachments
             (message_id, content_hash, filename, position)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                row.message_id,
                row.content_hash,
                row.filename,
                row.position as i64
            ],
        )?;
        Ok(())
    }

    /// List all attachments for a given message.
    pub fn list_message_attachments(
        &self,
        message_id: &str,
    ) -> Result<Vec<MessageAttachmentRow>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT message_id, content_hash, filename, position
             FROM message_attachments
             WHERE message_id = ?1
             ORDER BY position",
        )?;
        let rows = stmt.query_map(params![message_id], |row| {
            Ok(MessageAttachmentRow {
                message_id: row.get(0)?,
                content_hash: row.get(1)?,
                filename: row.get(2)?,
                position: row.get::<_, i64>(3)? as u32,
            })
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    // -----------------------------------------------------------------------
    // Reactions CRUD (Phase 8)
    // -----------------------------------------------------------------------

    /// Insert or update a reaction. Uses PRIMARY KEY (target_id, sender_id)
    /// so each sender can have at most one reaction per target message.
    pub fn upsert_reaction(&self, row: &ReactionRow) -> Result<(), StorageError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO reactions
             (message_id, target_id, sender_id, reaction, timestamp)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                row.message_id,
                row.target_id,
                row.sender_id,
                row.reaction,
                row.timestamp as i64,
            ],
        )?;
        Ok(())
    }

    /// Remove a reaction by target_id and sender_id.
    pub fn delete_reaction(&self, target_id: &str, sender_id: &str) -> Result<bool, StorageError> {
        let affected = self.conn.execute(
            "DELETE FROM reactions WHERE target_id = ?1 AND sender_id = ?2",
            params![target_id, sender_id],
        )?;
        Ok(affected > 0)
    }

    /// Get a specific sender's reaction for a target message.
    pub fn get_reaction(
        &self,
        target_id: &str,
        sender_id: &str,
    ) -> Result<Option<ReactionRow>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT message_id, target_id, sender_id, reaction, timestamp
             FROM reactions WHERE target_id = ?1 AND sender_id = ?2",
        )?;
        let row = stmt
            .query_row(params![target_id, sender_id], |row| {
                Ok(ReactionRow {
                    message_id: row.get(0)?,
                    target_id: row.get(1)?,
                    sender_id: row.get(2)?,
                    reaction: row.get(3)?,
                    timestamp: row.get::<_, i64>(4)? as u64,
                })
            })
            .optional()?;
        Ok(row)
    }

    /// List all reactions for a target message.
    pub fn list_reactions(&self, target_id: &str) -> Result<Vec<ReactionRow>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT message_id, target_id, sender_id, reaction, timestamp
             FROM reactions WHERE target_id = ?1 ORDER BY timestamp",
        )?;
        let rows = stmt.query_map(params![target_id], |row| {
            Ok(ReactionRow {
                message_id: row.get(0)?,
                target_id: row.get(1)?,
                sender_id: row.get(2)?,
                reaction: row.get(3)?,
                timestamp: row.get::<_, i64>(4)? as u64,
            })
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// Get aggregated reaction counts for a target message.
    pub fn count_reactions(&self, target_id: &str) -> Result<(u32, u32), StorageError> {
        let upvotes: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM reactions WHERE target_id = ?1 AND reaction = 'upvote'",
            params![target_id],
            |row| row.get(0),
        )?;
        let downvotes: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM reactions WHERE target_id = ?1 AND reaction = 'downvote'",
            params![target_id],
            |row| row.get(0),
        )?;
        Ok((upvotes as u32, downvotes as u32))
    }

    // -----------------------------------------------------------------------
    // Replies (Phase 9)
    // -----------------------------------------------------------------------

    /// List replies to a given message.
    pub fn list_replies(
        &self,
        parent_id: &str,
        before: Option<u64>,
        limit: u32,
    ) -> Result<Vec<MessageRow>, StorageError> {
        let mut sql = String::from(
            "SELECT message_id, sender_id, recipient_id, message_kind, group_id,
                    body, timestamp, received_at, ttl_seconds, is_read, raw_envelope, reply_to
             FROM messages WHERE reply_to = ?",
        );
        let mut bind_values: Vec<Box<dyn rusqlite::types::ToSql>> =
            vec![Box::new(parent_id.to_string())];

        if let Some(b) = before {
            sql.push_str(" AND timestamp < ?");
            bind_values.push(Box::new(b as i64));
        }

        sql.push_str(" ORDER BY timestamp ASC LIMIT ?");
        bind_values.push(Box::new(limit as i64));

        let mut stmt = self.conn.prepare(&sql)?;
        let bind_refs: Vec<&dyn rusqlite::types::ToSql> =
            bind_values.iter().map(|b| b.as_ref()).collect();

        let rows = stmt.query_map(bind_refs.as_slice(), |row| {
            Ok(MessageRow {
                message_id: row.get(0)?,
                sender_id: row.get(1)?,
                recipient_id: row.get(2)?,
                message_kind: row.get(3)?,
                group_id: row.get(4)?,
                body: row.get(5)?,
                timestamp: row.get::<_, i64>(6)? as u64,
                received_at: row.get::<_, i64>(7)? as u64,
                ttl_seconds: row.get::<_, i64>(8)? as u64,
                is_read: row.get::<_, i32>(9)? != 0,
                raw_envelope: row.get(10)?,
                reply_to: row.get(11)?,
            })
        })?;

        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// Count the number of replies to a given message.
    pub fn count_replies(&self, parent_id: &str) -> Result<u32, StorageError> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE reply_to = ?1",
            params![parent_id],
            |row| row.get(0),
        )?;
        Ok(count as u32)
    }

    // -----------------------------------------------------------------------
    // Profiles CRUD (Phase 10)
    // -----------------------------------------------------------------------

    /// Insert or update a profile.
    pub fn upsert_profile(&self, row: &ProfileRow) -> Result<(), StorageError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO profiles
             (user_id, display_name, bio, avatar_hash, public_fields, friends_fields, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                row.user_id,
                row.display_name,
                row.bio,
                row.avatar_hash,
                row.public_fields,
                row.friends_fields,
                row.updated_at as i64,
            ],
        )?;
        Ok(())
    }

    /// Get a profile by user ID.
    pub fn get_profile(&self, user_id: &str) -> Result<Option<ProfileRow>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT user_id, display_name, bio, avatar_hash, public_fields, friends_fields, updated_at
             FROM profiles WHERE user_id = ?1",
        )?;
        let row = stmt
            .query_row(params![user_id], |row| {
                Ok(ProfileRow {
                    user_id: row.get(0)?,
                    display_name: row.get(1)?,
                    bio: row.get(2)?,
                    avatar_hash: row.get(3)?,
                    public_fields: row.get(4)?,
                    friends_fields: row.get(5)?,
                    updated_at: row.get::<_, i64>(6)? as u64,
                })
            })
            .optional()?;
        Ok(row)
    }

    /// Update a profile only if the incoming update is newer (or same timestamp,
    /// which allows a richer friends-only profile to overwrite a public-only one).
    pub fn upsert_profile_if_newer(&self, row: &ProfileRow) -> Result<bool, StorageError> {
        if let Some(existing) = self.get_profile(&row.user_id)? {
            if existing.updated_at > row.updated_at {
                return Ok(false);
            }
        }
        self.upsert_profile(row)?;
        Ok(true)
    }

    // -----------------------------------------------------------------------
    // Friend Requests CRUD
    // -----------------------------------------------------------------------

    pub fn insert_friend_request(&self, row: &FriendRequestRow) -> Result<i64, StorageError> {
        self.conn.execute(
            "INSERT INTO friend_requests
             (from_peer_id, to_peer_id, status, message, from_signing_key,
              from_encryption_key, direction, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                row.from_peer_id,
                row.to_peer_id,
                row.status,
                row.message,
                row.from_signing_key,
                row.from_encryption_key,
                row.direction,
                row.created_at as i64,
                row.updated_at as i64,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn get_friend_request(&self, id: i64) -> Result<Option<FriendRequestRow>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, from_peer_id, to_peer_id, status, message, from_signing_key,
                    from_encryption_key, direction, created_at, updated_at
             FROM friend_requests WHERE id = ?1",
        )?;
        let row = stmt
            .query_row(params![id], |row| {
                Ok(FriendRequestRow {
                    id: row.get(0)?,
                    from_peer_id: row.get(1)?,
                    to_peer_id: row.get(2)?,
                    status: row.get(3)?,
                    message: row.get(4)?,
                    from_signing_key: row.get(5)?,
                    from_encryption_key: row.get(6)?,
                    direction: row.get(7)?,
                    created_at: row.get::<_, i64>(8)? as u64,
                    updated_at: row.get::<_, i64>(9)? as u64,
                })
            })
            .optional()?;
        Ok(row)
    }

    pub fn list_friend_requests(
        &self,
        status_filter: Option<&str>,
        direction_filter: Option<&str>,
    ) -> Result<Vec<FriendRequestRow>, StorageError> {
        let base = "SELECT id, from_peer_id, to_peer_id, status, message, from_signing_key,
                           from_encryption_key, direction, created_at, updated_at
                    FROM friend_requests";
        let mut conditions = Vec::new();
        let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        if let Some(status) = status_filter {
            conditions.push(format!("status = ?{}", param_values.len() + 1));
            param_values.push(Box::new(status.to_string()));
        }
        if let Some(direction) = direction_filter {
            conditions.push(format!("direction = ?{}", param_values.len() + 1));
            param_values.push(Box::new(direction.to_string()));
        }

        let query = if conditions.is_empty() {
            format!("{} ORDER BY created_at DESC", base)
        } else {
            format!(
                "{} WHERE {} ORDER BY created_at DESC",
                base,
                conditions.join(" AND ")
            )
        };

        let mut stmt = self.conn.prepare(&query)?;
        let params_refs: Vec<&dyn rusqlite::types::ToSql> =
            param_values.iter().map(|p| p.as_ref()).collect();
        let rows = stmt.query_map(params_refs.as_slice(), |row| {
            Ok(FriendRequestRow {
                id: row.get(0)?,
                from_peer_id: row.get(1)?,
                to_peer_id: row.get(2)?,
                status: row.get(3)?,
                message: row.get(4)?,
                from_signing_key: row.get(5)?,
                from_encryption_key: row.get(6)?,
                direction: row.get(7)?,
                created_at: row.get::<_, i64>(8)? as u64,
                updated_at: row.get::<_, i64>(9)? as u64,
            })
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    pub fn update_friend_request_status(
        &self,
        id: i64,
        status: &str,
    ) -> Result<bool, StorageError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let affected = self.conn.execute(
            "UPDATE friend_requests SET status = ?1, updated_at = ?2 WHERE id = ?3",
            params![status, now as i64, id],
        )?;
        Ok(affected > 0)
    }

    /// Check if there is a pending friend request from a specific peer.
    pub fn has_pending_request_from(
        &self,
        from_peer_id: &str,
        to_peer_id: &str,
    ) -> Result<bool, StorageError> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM friend_requests
             WHERE from_peer_id = ?1 AND to_peer_id = ?2 AND status = 'pending'",
            params![from_peer_id, to_peer_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Find an existing friend request between two peers (any status).
    pub fn find_request_between(
        &self,
        from_peer_id: &str,
        to_peer_id: &str,
    ) -> Result<Option<FriendRequestRow>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, from_peer_id, to_peer_id, status, message, from_signing_key,
                    from_encryption_key, direction, created_at, updated_at
             FROM friend_requests
             WHERE from_peer_id = ?1 AND to_peer_id = ?2
             ORDER BY created_at DESC LIMIT 1",
        )?;
        let row = stmt
            .query_row(params![from_peer_id, to_peer_id], |row| {
                Ok(FriendRequestRow {
                    id: row.get(0)?,
                    from_peer_id: row.get(1)?,
                    to_peer_id: row.get(2)?,
                    status: row.get(3)?,
                    message: row.get(4)?,
                    from_signing_key: row.get(5)?,
                    from_encryption_key: row.get(6)?,
                    direction: row.get(7)?,
                    created_at: row.get::<_, i64>(8)? as u64,
                    updated_at: row.get::<_, i64>(9)? as u64,
                })
            })
            .optional()?;
        Ok(row)
    }

    /// Update the message and timestamps on an existing friend request,
    /// resetting it to pending.
    pub fn refresh_friend_request(
        &self,
        id: i64,
        message: Option<&str>,
        signing_key: &str,
        encryption_key: &str,
    ) -> Result<(), StorageError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.conn.execute(
            "UPDATE friend_requests
             SET message = ?1, from_signing_key = ?2, from_encryption_key = ?3,
                 updated_at = ?4, status = 'pending'
             WHERE id = ?5",
            params![message, signing_key, encryption_key, now as i64, id],
        )?;
        Ok(())
    }

    /// Check if a peer is blocked (has a blocked friend request).
    pub fn is_peer_blocked(&self, peer_id: &str) -> Result<bool, StorageError> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM friend_requests
             WHERE from_peer_id = ?1 AND status = 'blocked'",
            params![peer_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    // -----------------------------------------------------------------------
    // Notifications CRUD (Phase 11)
    // -----------------------------------------------------------------------

    /// Insert a new notification. Returns the new notification ID.
    pub fn insert_notification(&self, row: &NotificationRow) -> Result<i64, StorageError> {
        self.conn.execute(
            "INSERT INTO notifications (type, message_id, sender_id, created_at, seen, read)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                row.notification_type,
                row.message_id,
                row.sender_id,
                row.created_at as i64,
                row.seen as i32,
                row.read as i32,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Get a notification by ID.
    pub fn get_notification(&self, id: i64) -> Result<Option<NotificationRow>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, type, message_id, sender_id, created_at, seen, read
             FROM notifications WHERE id = ?1",
        )?;
        let row = stmt
            .query_row(params![id], |row| {
                Ok(NotificationRow {
                    id: row.get(0)?,
                    notification_type: row.get(1)?,
                    message_id: row.get(2)?,
                    sender_id: row.get(3)?,
                    created_at: row.get::<_, i64>(4)? as u64,
                    seen: row.get::<_, i32>(5)? != 0,
                    read: row.get::<_, i32>(6)? != 0,
                })
            })
            .optional()?;
        Ok(row)
    }

    /// List notifications with optional filters.
    pub fn list_notifications(
        &self,
        unread_only: bool,
        limit: u32,
    ) -> Result<Vec<NotificationRow>, StorageError> {
        let sql = if unread_only {
            "SELECT id, type, message_id, sender_id, created_at, seen, read
             FROM notifications WHERE read = 0
             ORDER BY created_at DESC LIMIT ?"
        } else {
            "SELECT id, type, message_id, sender_id, created_at, seen, read
             FROM notifications
             ORDER BY created_at DESC LIMIT ?"
        };

        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            Ok(NotificationRow {
                id: row.get(0)?,
                notification_type: row.get(1)?,
                message_id: row.get(2)?,
                sender_id: row.get(3)?,
                created_at: row.get::<_, i64>(4)? as u64,
                seen: row.get::<_, i32>(5)? != 0,
                read: row.get::<_, i32>(6)? != 0,
            })
        })?;

        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// Mark a notification as read.
    pub fn mark_notification_read(&self, id: i64) -> Result<bool, StorageError> {
        let affected = self.conn.execute(
            "UPDATE notifications SET read = 1 WHERE id = ?1",
            params![id],
        )?;
        Ok(affected > 0)
    }

    /// Mark all notifications as read.
    pub fn mark_all_notifications_read(&self) -> Result<u32, StorageError> {
        let affected = self
            .conn
            .execute("UPDATE notifications SET read = 1 WHERE read = 0", [])?;
        Ok(affected as u32)
    }

    /// Count unread notifications.
    pub fn count_unread_notifications(&self) -> Result<u32, StorageError> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM notifications WHERE read = 0",
            [],
            |row| row.get(0),
        )?;
        Ok(count as u32)
    }

    /// Count unseen notifications (not yet viewed in the notification panel).
    pub fn count_unseen_notifications(&self) -> Result<u32, StorageError> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM notifications WHERE seen = 0",
            [],
            |row| row.get(0),
        )?;
        Ok(count as u32)
    }

    /// Mark all notifications as seen (user opened the notification panel).
    pub fn mark_all_notifications_seen(&self) -> Result<u32, StorageError> {
        let affected = self
            .conn
            .execute("UPDATE notifications SET seen = 1 WHERE seen = 0", [])?;
        Ok(affected as u32)
    }

    /// Check if a notification already exists (to avoid duplicates).
    pub fn has_notification(
        &self,
        notification_type: &str,
        message_id: &str,
    ) -> Result<bool, StorageError> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM notifications WHERE type = ?1 AND message_id = ?2",
            params![notification_type, message_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    // -----------------------------------------------------------------------
    // Group Invites CRUD
    // -----------------------------------------------------------------------

    /// Insert a new group invite. Returns the new invite ID.
    /// Uses INSERT OR IGNORE so duplicate (group_id, from, to, direction) is a no-op.
    pub fn insert_group_invite(&self, row: &GroupInviteRow) -> Result<i64, StorageError> {
        let affected = self.conn.execute(
            "INSERT OR IGNORE INTO group_invites
             (group_id, from_peer_id, to_peer_id, status, message, direction, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                row.group_id,
                row.from_peer_id,
                row.to_peer_id,
                row.status,
                row.message,
                row.direction,
                row.created_at as i64,
                row.updated_at as i64,
            ],
        )?;
        if affected == 0 {
            // Row already existed; return the existing id.
            let existing_id: i64 = self.conn.query_row(
                "SELECT id FROM group_invites
                 WHERE group_id = ?1 AND from_peer_id = ?2 AND to_peer_id = ?3 AND direction = ?4",
                params![row.group_id, row.from_peer_id, row.to_peer_id, row.direction],
                |r| r.get(0),
            )?;
            return Ok(existing_id);
        }
        Ok(self.conn.last_insert_rowid())
    }

    /// Get a group invite by ID.
    pub fn get_group_invite(&self, id: i64) -> Result<Option<GroupInviteRow>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, group_id, from_peer_id, to_peer_id, status, message, direction,
                    created_at, updated_at
             FROM group_invites WHERE id = ?1",
        )?;
        let row = stmt
            .query_row(params![id], |row| {
                Ok(GroupInviteRow {
                    id: row.get(0)?,
                    group_id: row.get(1)?,
                    from_peer_id: row.get(2)?,
                    to_peer_id: row.get(3)?,
                    status: row.get(4)?,
                    message: row.get(5)?,
                    direction: row.get(6)?,
                    created_at: row.get::<_, i64>(7)? as u64,
                    updated_at: row.get::<_, i64>(8)? as u64,
                })
            })
            .optional()?;
        Ok(row)
    }

    /// List group invites with optional status filter.
    pub fn list_group_invites(
        &self,
        status_filter: Option<&str>,
        direction_filter: Option<&str>,
    ) -> Result<Vec<GroupInviteRow>, StorageError> {
        let sql = match (status_filter, direction_filter) {
            (Some(_), Some(_)) => {
                "SELECT id, group_id, from_peer_id, to_peer_id, status, message, direction,
                        created_at, updated_at
                 FROM group_invites WHERE status = ?1 AND direction = ?2
                 ORDER BY created_at DESC"
            }
            (Some(_), None) => {
                "SELECT id, group_id, from_peer_id, to_peer_id, status, message, direction,
                        created_at, updated_at
                 FROM group_invites WHERE status = ?1
                 ORDER BY created_at DESC"
            }
            (None, Some(_)) => {
                "SELECT id, group_id, from_peer_id, to_peer_id, status, message, direction,
                        created_at, updated_at
                 FROM group_invites WHERE direction = ?1
                 ORDER BY created_at DESC"
            }
            (None, None) => {
                "SELECT id, group_id, from_peer_id, to_peer_id, status, message, direction,
                        created_at, updated_at
                 FROM group_invites
                 ORDER BY created_at DESC"
            }
        };

        let mut stmt = self.conn.prepare(sql)?;

        let map_row = |row: &rusqlite::Row<'_>| {
            Ok(GroupInviteRow {
                id: row.get(0)?,
                group_id: row.get(1)?,
                from_peer_id: row.get(2)?,
                to_peer_id: row.get(3)?,
                status: row.get(4)?,
                message: row.get(5)?,
                direction: row.get(6)?,
                created_at: row.get::<_, i64>(7)? as u64,
                updated_at: row.get::<_, i64>(8)? as u64,
            })
        };

        let rows = match (status_filter, direction_filter) {
            (Some(s), Some(d)) => stmt.query_map(params![s, d], map_row)?,
            (Some(s), None) => stmt.query_map(params![s], map_row)?,
            (None, Some(d)) => stmt.query_map(params![d], map_row)?,
            (None, None) => stmt.query_map([], map_row)?,
        };

        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// Update the status of a group invite.
    pub fn update_group_invite_status(
        &self,
        id: i64,
        status: &str,
    ) -> Result<bool, StorageError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let affected = self.conn.execute(
            "UPDATE group_invites SET status = ?1, updated_at = ?2 WHERE id = ?3",
            params![status, now as i64, id],
        )?;
        Ok(affected > 0)
    }

    /// Find a group invite by the (group_id, from, to, direction) unique key.
    pub fn find_group_invite(
        &self,
        group_id: &str,
        from_peer_id: &str,
        to_peer_id: &str,
        direction: &str,
    ) -> Result<Option<GroupInviteRow>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, group_id, from_peer_id, to_peer_id, status, message, direction,
                    created_at, updated_at
             FROM group_invites
             WHERE group_id = ?1 AND from_peer_id = ?2 AND to_peer_id = ?3 AND direction = ?4
             LIMIT 1",
        )?;
        let row = stmt
            .query_row(params![group_id, from_peer_id, to_peer_id, direction], |row| {
                Ok(GroupInviteRow {
                    id: row.get(0)?,
                    group_id: row.get(1)?,
                    from_peer_id: row.get(2)?,
                    to_peer_id: row.get(3)?,
                    status: row.get(4)?,
                    message: row.get(5)?,
                    direction: row.get(6)?,
                    created_at: row.get::<_, i64>(7)? as u64,
                    updated_at: row.get::<_, i64>(8)? as u64,
                })
            })
            .optional()?;
        Ok(row)
    }

    // -----------------------------------------------------------------------
    // JSON-to-SQLite migration
    // -----------------------------------------------------------------------

    /// Migrate legacy JSON/JSONL files into the database.
    ///
    /// Reads identity.json, peers.json, inbox.jsonl, outbox.jsonl from
    /// `data_dir` and imports them. Renames migrated files to *.migrated.
    pub fn migrate_from_json(&self, data_dir: &Path) -> Result<MigrationReport, StorageError> {
        let mut report = MigrationReport::default();

        // 1. Migrate identity.json
        let identity_path = data_dir.join("identity.json");
        if identity_path.exists() {
            let data = std::fs::read_to_string(&identity_path)?;
            let kp: StoredKeypair = serde_json::from_str(&data)?;
            let row = IdentityRow::from(&kp);
            if self.get_identity()?.is_none() {
                self.insert_identity(&row)?;
                report.identity_migrated = true;
            }
            std::fs::rename(&identity_path, data_dir.join("identity.json.migrated"))?;
        }

        // 2. Migrate peers.json
        let peers_path = data_dir.join("peers.json");
        if peers_path.exists() {
            let data = std::fs::read_to_string(&peers_path)?;
            let peers: Vec<LegacyPeer> = serde_json::from_str(&data)?;
            for peer in &peers {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let row = PeerRow {
                    peer_id: peer.id.clone(),
                    display_name: Some(peer.name.clone()),
                    signing_public_key: peer.public_key_hex.clone(),
                    encryption_public_key: None,
                    added_at: now,
                    is_friend: true,
                    last_seen_online: None,
                    online: false,
                };
                self.insert_peer(&row)?;
            }
            report.peers_migrated = peers.len();
            std::fs::rename(&peers_path, data_dir.join("peers.json.migrated"))?;
        }

        // 3. Migrate inbox.jsonl
        let inbox_path = data_dir.join("inbox.jsonl");
        if inbox_path.exists() {
            let data = std::fs::read_to_string(&inbox_path)?;
            for line in data.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if let Ok(msg) = serde_json::from_str::<LegacyReceivedMessage>(line) {
                    let now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    let msg_id = format!("migrated-inbox-{}", report.inbox_migrated);
                    let row = MessageRow {
                        message_id: msg_id,
                        sender_id: msg.sender_id,
                        recipient_id: String::new(),
                        message_kind: "direct".to_string(),
                        group_id: None,
                        body: Some(msg.message),
                        timestamp: msg.timestamp,
                        received_at: now,
                        ttl_seconds: 3600,
                        is_read: false,
                        raw_envelope: None,
                        reply_to: None,
                    };
                    self.insert_message(&row)?;
                    report.inbox_migrated += 1;
                }
            }
            std::fs::rename(&inbox_path, data_dir.join("inbox.jsonl.migrated"))?;
        }

        // 4. Migrate outbox.jsonl
        let outbox_path = data_dir.join("outbox.jsonl");
        if outbox_path.exists() {
            let data = std::fs::read_to_string(&outbox_path)?;
            for line in data.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if let Ok(envelope) = serde_json::from_str::<Envelope>(line) {
                    let now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    let row = OutboxRow {
                        message_id: envelope.header.message_id.0.clone(),
                        envelope: line.to_string(),
                        sent_at: now,
                        delivered: true,
                    };
                    self.insert_outbox(&row)?;
                    report.outbox_migrated += 1;
                }
            }
            std::fs::rename(&outbox_path, data_dir.join("outbox.jsonl.migrated"))?;
        }

        Ok(report)
    }

    // -----------------------------------------------------------------------
    // Helpers for envelope ingestion
    // -----------------------------------------------------------------------

    /// Store a received envelope as a message row.
    pub fn store_received_envelope(
        &self,
        envelope: &Envelope,
        decrypted_body: Option<&str>,
    ) -> Result<(), StorageError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let kind_str = message_kind_to_str(&envelope.header.message_kind);
        let raw = serde_json::to_string(envelope)?;
        let row = MessageRow {
            message_id: envelope.header.message_id.0.clone(),
            sender_id: envelope.header.sender_id.clone(),
            recipient_id: envelope.header.recipient_id.clone(),
            message_kind: kind_str.to_string(),
            group_id: envelope.header.group_id.clone(),
            body: decrypted_body.map(|s| s.to_string()),
            timestamp: envelope.header.timestamp,
            received_at: now,
            ttl_seconds: envelope.header.ttl_seconds,
            is_read: false,
            raw_envelope: Some(raw),
            reply_to: None,
        };
        self.insert_message(&row)
    }
}

// ---------------------------------------------------------------------------
// Legacy types for migration
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct LegacyPeer {
    name: String,
    id: String,
    public_key_hex: String,
}

#[derive(Debug, Deserialize)]
struct LegacyReceivedMessage {
    sender_id: String,
    timestamp: u64,
    message: String,
}

// ---------------------------------------------------------------------------
// Migration report
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct MigrationReport {
    pub identity_migrated: bool,
    pub peers_migrated: usize,
    pub inbox_migrated: usize,
    pub outbox_migrated: usize,
}

impl MigrationReport {
    pub fn is_empty(&self) -> bool {
        !self.identity_migrated
            && self.peers_migrated == 0
            && self.inbox_migrated == 0
            && self.outbox_migrated == 0
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn message_kind_to_str(kind: &MessageKind) -> &'static str {
    match kind {
        MessageKind::Public => "public",
        MessageKind::Meta => "meta",
        MessageKind::Direct => "direct",
        MessageKind::FriendGroup => "friend_group",
        MessageKind::StoreForPeer => "store_for_peer",
    }
}

/// Resolve the database path: `{data_dir}/tenet.db`.
pub fn db_path(data_dir: &Path) -> PathBuf {
    data_dir.join("tenet.db")
}

/// Resolve the tenet home directory from environment or default.
pub fn resolve_data_dir() -> PathBuf {
    std::env::var("TENET_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dirs_or_default().join(".tenet"))
}

fn dirs_or_default() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// Map a MIME content type to a lowercase file extension.
/// Used to construct attachment filenames on disk.
fn content_type_to_ext(content_type: &str) -> &str {
    let base = content_type
        .split(';')
        .next()
        .unwrap_or(content_type)
        .trim();
    match base {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        "image/bmp" => "bmp",
        "image/tiff" => "tiff",
        "application/pdf" => "pdf",
        "text/plain" => "txt",
        "text/html" => "html",
        "text/css" => "css",
        "application/json" => "json",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "audio/mpeg" => "mp3",
        "audio/ogg" => "ogg",
        "audio/wav" => "wav",
        _ => "bin",
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn now_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    /// Create an in-memory storage with a per-invocation temp directory for
    /// attachment files. Each call gets a unique directory so parallel tests
    /// don't collide.
    fn test_storage() -> Storage {
        let pid = std::process::id();
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("tenet-test-{pid}-{ts}"));
        Storage::open_in_memory(&dir).unwrap()
    }

    #[test]
    fn test_schema_creation() {
        let storage = test_storage();
        // Schema should already be created — verify by inserting data
        let row = RelayRow {
            url: "http://localhost:8080".to_string(),
            added_at: now_secs(),
            last_sync: None,
            enabled: true,
        };
        storage.insert_relay(&row).unwrap();
    }

    #[test]
    fn test_identity_crud() {
        let storage = test_storage();

        // No identity initially
        assert!(storage.get_identity().unwrap().is_none());

        let row = IdentityRow {
            id: "test-id".to_string(),
            public_key: "pub-hex".to_string(),
            private_key: "priv-hex".to_string(),
            signing_pub: "sign-pub-hex".to_string(),
            signing_priv: "sign-priv-hex".to_string(),
            created_at: now_secs(),
        };
        storage.insert_identity(&row).unwrap();

        let loaded = storage.get_identity().unwrap().unwrap();
        assert_eq!(loaded.id, "test-id");
        assert_eq!(loaded.public_key, "pub-hex");
        assert_eq!(loaded.signing_pub, "sign-pub-hex");
    }

    #[test]
    fn test_peer_crud() {
        let storage = test_storage();
        let now = now_secs();

        let peer = PeerRow {
            peer_id: "peer-1".to_string(),
            display_name: Some("Alice".to_string()),
            signing_public_key: "sign-key".to_string(),
            encryption_public_key: Some("enc-key".to_string()),
            added_at: now,
            is_friend: true,
            last_seen_online: None,
            online: false,
        };
        storage.insert_peer(&peer).unwrap();

        // Get peer
        let loaded = storage.get_peer("peer-1").unwrap().unwrap();
        assert_eq!(loaded.display_name, Some("Alice".to_string()));
        assert!(loaded.is_friend);
        assert!(!loaded.online);

        // List peers
        let peers = storage.list_peers().unwrap();
        assert_eq!(peers.len(), 1);

        // Update online status
        storage.update_peer_online("peer-1", true, now).unwrap();
        let loaded = storage.get_peer("peer-1").unwrap().unwrap();
        assert!(loaded.online);
        assert_eq!(loaded.last_seen_online, Some(now));

        // Delete peer
        assert!(storage.delete_peer("peer-1").unwrap());
        assert!(storage.get_peer("peer-1").unwrap().is_none());
    }

    #[test]
    fn test_message_crud() {
        let storage = test_storage();
        let now = now_secs();

        let msg = MessageRow {
            message_id: "msg-1".to_string(),
            sender_id: "sender".to_string(),
            recipient_id: "recipient".to_string(),
            message_kind: "direct".to_string(),
            group_id: None,
            body: Some("Hello".to_string()),
            timestamp: now,
            received_at: now,
            ttl_seconds: 3600,
            is_read: false,
            raw_envelope: None,
            reply_to: None,
        };
        storage.insert_message(&msg).unwrap();

        // Get message
        let loaded = storage.get_message("msg-1").unwrap().unwrap();
        assert_eq!(loaded.body, Some("Hello".to_string()));
        assert!(!loaded.is_read);

        // Has message
        assert!(storage.has_message("msg-1").unwrap());
        assert!(!storage.has_message("msg-nonexistent").unwrap());

        // Mark read
        assert!(storage.mark_message_read("msg-1").unwrap());
        let loaded = storage.get_message("msg-1").unwrap().unwrap();
        assert!(loaded.is_read);

        // List messages
        let msgs = storage.list_messages(None, None, None, 100).unwrap();
        assert_eq!(msgs.len(), 1);

        // List with kind filter
        let msgs = storage
            .list_messages(Some("direct"), None, None, 100)
            .unwrap();
        assert_eq!(msgs.len(), 1);
        let msgs = storage
            .list_messages(Some("public"), None, None, 100)
            .unwrap();
        assert_eq!(msgs.len(), 0);
    }

    #[test]
    fn test_message_list_filters() {
        let storage = test_storage();
        let now = now_secs();

        // Insert messages at different timestamps
        for i in 0..5 {
            let msg = MessageRow {
                message_id: format!("msg-{i}"),
                sender_id: "sender".to_string(),
                recipient_id: "recipient".to_string(),
                message_kind: if i < 3 { "direct" } else { "public" }.to_string(),
                group_id: if i == 4 {
                    Some("group-1".to_string())
                } else {
                    None
                },
                body: Some(format!("Message {i}")),
                timestamp: now + i,
                received_at: now + i,
                ttl_seconds: 3600,
                is_read: false,
                raw_envelope: None,
                reply_to: None,
            };
            storage.insert_message(&msg).unwrap();
        }

        // Filter by kind
        let direct = storage
            .list_messages(Some("direct"), None, None, 100)
            .unwrap();
        assert_eq!(direct.len(), 3);

        // Filter by before timestamp
        let before = storage
            .list_messages(None, None, Some(now + 3), 100)
            .unwrap();
        assert_eq!(before.len(), 3);

        // Limit
        let limited = storage.list_messages(None, None, None, 2).unwrap();
        assert_eq!(limited.len(), 2);

        // Filter by group
        let group = storage
            .list_messages(None, Some("group-1"), None, 100)
            .unwrap();
        assert_eq!(group.len(), 1);
    }

    #[test]
    fn test_group_crud() {
        let storage = test_storage();
        let now = now_secs();

        let group = GroupRow {
            group_id: "grp-1".to_string(),
            group_key: vec![42u8; 32],
            creator_id: "creator".to_string(),
            created_at: now,
            key_version: 1,
        };
        storage.insert_group(&group).unwrap();

        let loaded = storage.get_group("grp-1").unwrap().unwrap();
        assert_eq!(loaded.group_key.len(), 32);
        assert_eq!(loaded.creator_id, "creator");

        // Add members
        storage
            .insert_group_member(&GroupMemberRow {
                group_id: "grp-1".to_string(),
                peer_id: "peer-a".to_string(),
                joined_at: now,
            })
            .unwrap();
        storage
            .insert_group_member(&GroupMemberRow {
                group_id: "grp-1".to_string(),
                peer_id: "peer-b".to_string(),
                joined_at: now,
            })
            .unwrap();

        let members = storage.list_group_members("grp-1").unwrap();
        assert_eq!(members.len(), 2);

        // Remove member
        assert!(storage.remove_group_member("grp-1", "peer-a").unwrap());
        let members = storage.list_group_members("grp-1").unwrap();
        assert_eq!(members.len(), 1);

        // Delete group
        assert!(storage.delete_group("grp-1").unwrap());
        assert!(storage.get_group("grp-1").unwrap().is_none());
    }

    #[test]
    fn test_outbox_crud() {
        let storage = test_storage();
        let now = now_secs();

        let entry = OutboxRow {
            message_id: "out-1".to_string(),
            envelope: r#"{"test": true}"#.to_string(),
            sent_at: now,
            delivered: false,
        };
        storage.insert_outbox(&entry).unwrap();

        let outbox = storage.list_outbox().unwrap();
        assert_eq!(outbox.len(), 1);
        assert!(!outbox[0].delivered);

        // Mark delivered
        assert!(storage.mark_outbox_delivered("out-1").unwrap());
        let outbox = storage.list_outbox().unwrap();
        assert!(outbox[0].delivered);
    }

    #[test]
    fn test_relay_crud() {
        let storage = test_storage();
        let now = now_secs();

        let relay = RelayRow {
            url: "http://localhost:8080".to_string(),
            added_at: now,
            last_sync: None,
            enabled: true,
        };
        storage.insert_relay(&relay).unwrap();

        let relays = storage.list_relays().unwrap();
        assert_eq!(relays.len(), 1);
        assert!(relays[0].last_sync.is_none());

        // Update last_sync
        storage
            .update_relay_last_sync("http://localhost:8080", now + 100)
            .unwrap();
        let relays = storage.list_relays().unwrap();
        assert_eq!(relays[0].last_sync, Some(now + 100));

        // Add a disabled relay
        storage
            .insert_relay(&RelayRow {
                url: "http://other:8080".to_string(),
                added_at: now,
                last_sync: None,
                enabled: false,
            })
            .unwrap();

        let all = storage.list_relays().unwrap();
        assert_eq!(all.len(), 2);
        let enabled = storage.list_enabled_relays().unwrap();
        assert_eq!(enabled.len(), 1);

        // Delete relay
        assert!(storage.delete_relay("http://other:8080").unwrap());
        assert_eq!(storage.list_relays().unwrap().len(), 1);
    }

    #[test]
    fn test_identity_from_stored_keypair() {
        let kp = StoredKeypair {
            id: "test-id".to_string(),
            public_key_hex: "pub".to_string(),
            private_key_hex: "priv".to_string(),
            signing_public_key_hex: "spub".to_string(),
            signing_private_key_hex: "spriv".to_string(),
        };
        let row = IdentityRow::from(&kp);
        assert_eq!(row.id, "test-id");

        let kp2 = row.to_stored_keypair();
        assert_eq!(kp2.id, kp.id);
        assert_eq!(kp2.public_key_hex, kp.public_key_hex);
    }

    #[test]
    fn test_duplicate_message_ignored() {
        let storage = test_storage();
        let now = now_secs();

        let msg = MessageRow {
            message_id: "msg-dup".to_string(),
            sender_id: "s".to_string(),
            recipient_id: "r".to_string(),
            message_kind: "direct".to_string(),
            group_id: None,
            body: Some("first".to_string()),
            timestamp: now,
            received_at: now,
            ttl_seconds: 3600,
            is_read: false,
            raw_envelope: None,
            reply_to: None,
        };
        storage.insert_message(&msg).unwrap();

        // Insert duplicate (should be silently ignored by INSERT OR IGNORE)
        let msg2 = MessageRow {
            body: Some("second".to_string()),
            ..msg
        };
        storage.insert_message(&msg2).unwrap();

        // Original body should remain
        let loaded = storage.get_message("msg-dup").unwrap().unwrap();
        assert_eq!(loaded.body, Some("first".to_string()));
    }

    #[test]
    fn test_json_migration() {
        use std::io::Write;

        let dir = std::env::temp_dir().join(format!("tenet-test-migration-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // Create identity.json
        let identity = StoredKeypair {
            id: "migrated-id".to_string(),
            public_key_hex: "pub-hex".to_string(),
            private_key_hex: "priv-hex".to_string(),
            signing_public_key_hex: "spub-hex".to_string(),
            signing_private_key_hex: "spriv-hex".to_string(),
        };
        std::fs::write(
            dir.join("identity.json"),
            serde_json::to_string(&identity).unwrap(),
        )
        .unwrap();

        // Create peers.json
        let peers = serde_json::json!([
            {"name": "alice", "id": "alice-id", "public_key_hex": "alice-key"},
            {"name": "bob", "id": "bob-id", "public_key_hex": "bob-key"}
        ]);
        std::fs::write(dir.join("peers.json"), peers.to_string()).unwrap();

        // Create inbox.jsonl
        let mut inbox = std::fs::File::create(dir.join("inbox.jsonl")).unwrap();
        writeln!(
            inbox,
            r#"{{"sender_id":"alice-id","timestamp":1000,"message":"hello"}}"#
        )
        .unwrap();
        writeln!(
            inbox,
            r#"{{"sender_id":"bob-id","timestamp":2000,"message":"world"}}"#
        )
        .unwrap();
        drop(inbox);

        // Run migration
        let storage = test_storage();
        let report = storage.migrate_from_json(&dir).unwrap();

        assert!(report.identity_migrated);
        assert_eq!(report.peers_migrated, 2);
        assert_eq!(report.inbox_migrated, 2);

        // Verify data
        let id = storage.get_identity().unwrap().unwrap();
        assert_eq!(id.id, "migrated-id");

        let peers = storage.list_peers().unwrap();
        assert_eq!(peers.len(), 2);

        let msgs = storage.list_messages(None, None, None, 100).unwrap();
        assert_eq!(msgs.len(), 2);

        // Verify files renamed
        assert!(dir.join("identity.json.migrated").exists());
        assert!(dir.join("peers.json.migrated").exists());
        assert!(dir.join("inbox.jsonl.migrated").exists());
        assert!(!dir.join("identity.json").exists());

        // Clean up
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_attachment_crud() {
        let storage = test_storage();
        let now = now_secs();

        let attachment = AttachmentRow {
            content_hash: "abc123hash".to_string(),
            content_type: "image/png".to_string(),
            size_bytes: 1024,
            data: vec![0u8; 1024],
            created_at: now,
        };
        storage.insert_attachment(&attachment).unwrap();

        // Get attachment
        let loaded = storage.get_attachment("abc123hash").unwrap().unwrap();
        assert_eq!(loaded.content_type, "image/png");
        assert_eq!(loaded.size_bytes, 1024);
        assert_eq!(loaded.data.len(), 1024);

        // Insert duplicate (should be silently ignored)
        let dup = AttachmentRow {
            content_type: "image/jpeg".to_string(),
            ..attachment
        };
        storage.insert_attachment(&dup).unwrap();
        let loaded = storage.get_attachment("abc123hash").unwrap().unwrap();
        assert_eq!(loaded.content_type, "image/png"); // original preserved

        // Not found
        assert!(storage.get_attachment("nonexistent").unwrap().is_none());
    }

    #[test]
    fn test_message_attachments() {
        let storage = test_storage();
        let now = now_secs();

        // Create a message first
        let msg = MessageRow {
            message_id: "msg-attach-1".to_string(),
            sender_id: "sender".to_string(),
            recipient_id: "recipient".to_string(),
            message_kind: "public".to_string(),
            group_id: None,
            body: Some("Check out this image".to_string()),
            timestamp: now,
            received_at: now,
            ttl_seconds: 3600,
            is_read: false,
            raw_envelope: None,
            reply_to: None,
        };
        storage.insert_message(&msg).unwrap();

        // Create attachments
        let att1 = AttachmentRow {
            content_hash: "hash-a".to_string(),
            content_type: "image/png".to_string(),
            size_bytes: 2048,
            data: vec![1u8; 2048],
            created_at: now,
        };
        let att2 = AttachmentRow {
            content_hash: "hash-b".to_string(),
            content_type: "application/pdf".to_string(),
            size_bytes: 4096,
            data: vec![2u8; 4096],
            created_at: now,
        };
        storage.insert_attachment(&att1).unwrap();
        storage.insert_attachment(&att2).unwrap();

        // Link attachments to message
        storage
            .insert_message_attachment(&MessageAttachmentRow {
                message_id: "msg-attach-1".to_string(),
                content_hash: "hash-a".to_string(),
                filename: Some("photo.png".to_string()),
                position: 0,
            })
            .unwrap();
        storage
            .insert_message_attachment(&MessageAttachmentRow {
                message_id: "msg-attach-1".to_string(),
                content_hash: "hash-b".to_string(),
                filename: Some("document.pdf".to_string()),
                position: 1,
            })
            .unwrap();

        // List message attachments
        let attachments = storage.list_message_attachments("msg-attach-1").unwrap();
        assert_eq!(attachments.len(), 2);
        assert_eq!(attachments[0].content_hash, "hash-a");
        assert_eq!(attachments[0].filename, Some("photo.png".to_string()));
        assert_eq!(attachments[0].position, 0);
        assert_eq!(attachments[1].content_hash, "hash-b");
        assert_eq!(attachments[1].position, 1);

        // Empty list for unknown message
        let empty = storage.list_message_attachments("nonexistent").unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn test_reactions_crud() {
        let storage = test_storage();
        let now = now_secs();

        // Create a target message
        let msg = MessageRow {
            message_id: "post-1".to_string(),
            sender_id: "author".to_string(),
            recipient_id: "*".to_string(),
            message_kind: "public".to_string(),
            group_id: None,
            body: Some("Hello world".to_string()),
            timestamp: now,
            received_at: now,
            ttl_seconds: 3600,
            is_read: true,
            raw_envelope: None,
            reply_to: None,
        };
        storage.insert_message(&msg).unwrap();

        // Add upvote
        let reaction = ReactionRow {
            message_id: "react-1".to_string(),
            target_id: "post-1".to_string(),
            sender_id: "voter-a".to_string(),
            reaction: "upvote".to_string(),
            timestamp: now,
        };
        storage.upsert_reaction(&reaction).unwrap();

        // Add downvote from another user
        let reaction2 = ReactionRow {
            message_id: "react-2".to_string(),
            target_id: "post-1".to_string(),
            sender_id: "voter-b".to_string(),
            reaction: "downvote".to_string(),
            timestamp: now,
        };
        storage.upsert_reaction(&reaction2).unwrap();

        // Get specific reaction
        let loaded = storage.get_reaction("post-1", "voter-a").unwrap().unwrap();
        assert_eq!(loaded.reaction, "upvote");

        // Count reactions
        let (up, down) = storage.count_reactions("post-1").unwrap();
        assert_eq!(up, 1);
        assert_eq!(down, 1);

        // List reactions
        let reactions = storage.list_reactions("post-1").unwrap();
        assert_eq!(reactions.len(), 2);

        // Change vote (upsert replaces)
        let changed = ReactionRow {
            message_id: "react-3".to_string(),
            target_id: "post-1".to_string(),
            sender_id: "voter-a".to_string(),
            reaction: "downvote".to_string(),
            timestamp: now + 1,
        };
        storage.upsert_reaction(&changed).unwrap();
        let (up, down) = storage.count_reactions("post-1").unwrap();
        assert_eq!(up, 0);
        assert_eq!(down, 2);

        // Delete reaction
        assert!(storage.delete_reaction("post-1", "voter-b").unwrap());
        let (up, down) = storage.count_reactions("post-1").unwrap();
        assert_eq!(up, 0);
        assert_eq!(down, 1);

        // Delete non-existent reaction
        assert!(!storage.delete_reaction("post-1", "nonexistent").unwrap());
    }

    #[test]
    fn test_replies() {
        let storage = test_storage();
        let now = now_secs();

        // Create parent post
        let parent = MessageRow {
            message_id: "parent-1".to_string(),
            sender_id: "author".to_string(),
            recipient_id: "*".to_string(),
            message_kind: "public".to_string(),
            group_id: None,
            body: Some("Original post".to_string()),
            timestamp: now,
            received_at: now,
            ttl_seconds: 3600,
            is_read: true,
            raw_envelope: None,
            reply_to: None,
        };
        storage.insert_message(&parent).unwrap();

        // Create replies
        let reply1 = MessageRow {
            message_id: "reply-1".to_string(),
            sender_id: "replier-a".to_string(),
            recipient_id: "*".to_string(),
            message_kind: "public".to_string(),
            group_id: None,
            body: Some("First reply".to_string()),
            timestamp: now + 1,
            received_at: now + 1,
            ttl_seconds: 3600,
            is_read: false,
            raw_envelope: None,
            reply_to: Some("parent-1".to_string()),
        };
        let reply2 = MessageRow {
            message_id: "reply-2".to_string(),
            sender_id: "replier-b".to_string(),
            recipient_id: "*".to_string(),
            message_kind: "public".to_string(),
            group_id: None,
            body: Some("Second reply".to_string()),
            timestamp: now + 2,
            received_at: now + 2,
            ttl_seconds: 3600,
            is_read: false,
            raw_envelope: None,
            reply_to: Some("parent-1".to_string()),
        };
        storage.insert_message(&reply1).unwrap();
        storage.insert_message(&reply2).unwrap();

        // Count replies
        assert_eq!(storage.count_replies("parent-1").unwrap(), 2);
        assert_eq!(storage.count_replies("reply-1").unwrap(), 0);

        // List replies (ascending order)
        let replies = storage.list_replies("parent-1", None, 100).unwrap();
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0].message_id, "reply-1");
        assert_eq!(replies[1].message_id, "reply-2");

        // reply_to field preserved on load
        let loaded = storage.get_message("reply-1").unwrap().unwrap();
        assert_eq!(loaded.reply_to, Some("parent-1".to_string()));
    }

    #[test]
    fn test_profiles_crud() {
        let storage = test_storage();
        let now = now_secs();

        // No profile initially
        assert!(storage.get_profile("user-1").unwrap().is_none());

        // Insert profile
        let profile = ProfileRow {
            user_id: "user-1".to_string(),
            display_name: Some("Alice".to_string()),
            bio: Some("Hello there".to_string()),
            avatar_hash: None,
            public_fields: r#"{"location":"Earth"}"#.to_string(),
            friends_fields: r#"{"email":"alice@example.com"}"#.to_string(),
            updated_at: now,
        };
        storage.upsert_profile(&profile).unwrap();

        // Get profile
        let loaded = storage.get_profile("user-1").unwrap().unwrap();
        assert_eq!(loaded.display_name, Some("Alice".to_string()));
        assert_eq!(loaded.bio, Some("Hello there".to_string()));
        assert_eq!(loaded.public_fields, r#"{"location":"Earth"}"#);

        // Update profile
        let updated = ProfileRow {
            display_name: Some("Alice Updated".to_string()),
            updated_at: now + 10,
            ..profile.clone()
        };
        storage.upsert_profile(&updated).unwrap();
        let loaded = storage.get_profile("user-1").unwrap().unwrap();
        assert_eq!(loaded.display_name, Some("Alice Updated".to_string()));

        // upsert_if_newer: older update should be rejected
        let older = ProfileRow {
            display_name: Some("Old Alice".to_string()),
            updated_at: now - 10,
            ..profile
        };
        assert!(!storage.upsert_profile_if_newer(&older).unwrap());
        let loaded = storage.get_profile("user-1").unwrap().unwrap();
        assert_eq!(loaded.display_name, Some("Alice Updated".to_string()));

        // upsert_if_newer: newer update should succeed
        let newer = ProfileRow {
            user_id: "user-1".to_string(),
            display_name: Some("Newest Alice".to_string()),
            bio: None,
            avatar_hash: None,
            public_fields: "{}".to_string(),
            friends_fields: "{}".to_string(),
            updated_at: now + 100,
        };
        assert!(storage.upsert_profile_if_newer(&newer).unwrap());
        let loaded = storage.get_profile("user-1").unwrap().unwrap();
        assert_eq!(loaded.display_name, Some("Newest Alice".to_string()));

        // upsert_if_newer: same-timestamp update should succeed (allows richer
        // friends-only profile to overwrite a public-only profile)
        let same_ts_richer = ProfileRow {
            user_id: "user-1".to_string(),
            display_name: Some("Newest Alice".to_string()),
            bio: None,
            avatar_hash: None,
            public_fields: "{}".to_string(),
            friends_fields: r#"{"email":"alice@secret.com"}"#.to_string(),
            updated_at: now + 100,
        };
        assert!(storage.upsert_profile_if_newer(&same_ts_richer).unwrap());
        let loaded = storage.get_profile("user-1").unwrap().unwrap();
        assert_eq!(loaded.friends_fields, r#"{"email":"alice@secret.com"}"#);
    }
}
