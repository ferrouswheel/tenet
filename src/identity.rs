//! Multi-identity management for tenet clients.
//!
//! Identities are stored in subdirectories under `{data_dir}/identities/{short_id}/`.
//! Each identity has its own SQLite database containing keys, peers, messages, and relays.
//!
//! A top-level `config.toml` specifies the default identity when multiple exist.
//! Legacy layouts (database directly in data_dir) are migrated automatically.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::crypto::{generate_keypair, StoredKeypair};
use crate::storage::{db_path, IdentityRow, Storage, StorageError};

/// Top-level configuration stored in `{data_dir}/config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TenetConfig {
    /// Short identity ID to use when no explicit selection is made.
    #[serde(default)]
    pub default_identity: Option<String>,
}

/// Describes a discovered identity directory.
#[derive(Debug, Clone)]
pub struct IdentityEntry {
    /// The short ID (directory name).
    pub short_id: String,
    /// Full path to the identity's data directory.
    pub path: PathBuf,
}

/// Result of identity resolution.
pub struct ResolvedIdentity {
    /// The keypair for the selected identity.
    pub keypair: StoredKeypair,
    /// The opened Storage handle.
    pub storage: Storage,
    /// Path to the identity's data directory.
    pub identity_dir: PathBuf,
    /// How many identities are available.
    pub total_identities: usize,
    /// Whether this identity was newly created.
    pub newly_created: bool,
    /// The default relay URL stored for this identity (from the identity's database).
    pub stored_relay_url: Option<String>,
}

/// Errors from identity resolution.
#[derive(Debug)]
pub enum IdentityError {
    Io(std::io::Error),
    Storage(StorageError),
    Toml(String),
    NotFound(String),
    Ambiguous { available: Vec<String> },
}

impl std::fmt::Display for IdentityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IdentityError::Io(e) => write!(f, "io error: {e}"),
            IdentityError::Storage(e) => write!(f, "storage error: {e}"),
            IdentityError::Toml(e) => write!(f, "config error: {e}"),
            IdentityError::NotFound(id) => write!(f, "identity not found: {id}"),
            IdentityError::Ambiguous { available } => {
                write!(
                    f,
                    "multiple identities available ({}), specify one or set a default",
                    available.join(", ")
                )
            }
        }
    }
}

impl std::error::Error for IdentityError {}

impl From<std::io::Error> for IdentityError {
    fn from(e: std::io::Error) -> Self {
        IdentityError::Io(e)
    }
}

impl From<StorageError> for IdentityError {
    fn from(e: StorageError) -> Self {
        IdentityError::Storage(e)
    }
}

/// Path to the identities subdirectory.
pub fn identities_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("identities")
}

/// Path to the top-level config file.
fn config_path(data_dir: &Path) -> PathBuf {
    data_dir.join("config.toml")
}

/// Load the top-level config, returning defaults if it doesn't exist.
pub fn load_config(data_dir: &Path) -> Result<TenetConfig, IdentityError> {
    let path = config_path(data_dir);
    if !path.exists() {
        return Ok(TenetConfig::default());
    }
    let contents = fs::read_to_string(&path)?;
    toml::from_str(&contents).map_err(|e| IdentityError::Toml(e.to_string()))
}

/// Save the top-level config.
pub fn save_config(data_dir: &Path, config: &TenetConfig) -> Result<(), IdentityError> {
    let path = config_path(data_dir);
    let contents =
        toml::to_string_pretty(config).map_err(|e| IdentityError::Toml(e.to_string()))?;
    fs::write(&path, contents)?;
    Ok(())
}

/// List all identity entries under `{data_dir}/identities/`.
pub fn list_identities(data_dir: &Path) -> Result<Vec<IdentityEntry>, IdentityError> {
    let dir = identities_dir(data_dir);
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut entries = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                // Only include directories that contain a database
                if db_path(&path).exists() {
                    entries.push(IdentityEntry {
                        short_id: name.to_string(),
                        path,
                    });
                }
            }
        }
    }

    entries.sort_by(|a, b| a.short_id.cmp(&b.short_id));
    Ok(entries)
}

/// Truncate an identity ID to a short form for use as a directory name.
fn short_id(full_id: &str) -> String {
    // Use first 12 chars of the base64 ID as a short identifier
    full_id.chars().take(12).collect()
}

/// Migrate a legacy data directory layout to the new multi-identity structure.
///
/// Legacy layout: `{data_dir}/tenet.db` (database at top level)
/// New layout: `{data_dir}/identities/{short_id}/tenet.db`
///
/// Returns the path to the migrated identity directory, or None if no migration was needed.
pub fn migrate_legacy_layout(data_dir: &Path) -> Result<Option<PathBuf>, IdentityError> {
    let legacy_db = db_path(data_dir);
    if !legacy_db.exists() {
        return Ok(None);
    }

    // Open legacy DB to read the identity
    let storage = Storage::open(&legacy_db)?;
    let identity = match storage.get_identity()? {
        Some(row) => row,
        None => return Ok(None),
    };
    drop(storage);

    let sid = short_id(&identity.id);
    let identity_dir = identities_dir(data_dir).join(&sid);
    fs::create_dir_all(&identity_dir)?;

    let new_db = db_path(&identity_dir);
    if !new_db.exists() {
        fs::rename(&legacy_db, &new_db)?;
        eprintln!("  migrated legacy database to identities/{}/", sid);
    }

    Ok(Some(identity_dir))
}

/// Resolve which identity to use, creating one if needed.
///
/// Priority for selection:
/// 1. `explicit_id` (from CLI `--identity` flag or `TENET_IDENTITY` env)
/// 2. `default_identity` from config.toml
/// 3. If only one identity exists, use it
/// 4. If no identities exist, create one
pub fn resolve_identity(
    data_dir: &Path,
    explicit_id: Option<&str>,
) -> Result<ResolvedIdentity, IdentityError> {
    fs::create_dir_all(data_dir)?;

    // Migrate legacy layout if present
    migrate_legacy_layout(data_dir)?;

    // Also migrate legacy JSON files if they exist in data_dir
    // (for tenet-cli which used identity.json directly)
    migrate_legacy_json_to_identities(data_dir)?;

    let mut identities = list_identities(data_dir)?;
    let config = load_config(data_dir)?;

    // Determine which identity to select
    let selected_id = explicit_id
        .map(|s| s.to_string())
        .or(config.default_identity.clone());

    match selected_id {
        Some(ref id) => {
            // Find the matching identity
            let entry = identities
                .iter()
                .find(|e| e.short_id == *id || e.short_id.starts_with(id))
                .ok_or_else(|| IdentityError::NotFound(id.clone()))?;

            let db = db_path(&entry.path);
            let storage = Storage::open(&db)?;
            let row = storage
                .get_identity()?
                .ok_or_else(|| IdentityError::NotFound(id.clone()))?;

            let stored_relay = first_enabled_relay(&storage);

            Ok(ResolvedIdentity {
                keypair: row.to_stored_keypair(),
                storage,
                identity_dir: entry.path.clone(),
                total_identities: identities.len(),
                newly_created: false,
                stored_relay_url: stored_relay,
            })
        }
        None => {
            match identities.len() {
                0 => {
                    // Create a new identity
                    let kp = generate_keypair();
                    let sid = short_id(&kp.id);
                    let identity_dir = identities_dir(data_dir).join(&sid);
                    fs::create_dir_all(&identity_dir)?;

                    let db = db_path(&identity_dir);
                    let storage = Storage::open(&db)?;
                    let row = IdentityRow::from(&kp);
                    storage.insert_identity(&row)?;

                    // Set as default since it's the only one
                    let cfg = TenetConfig {
                        default_identity: Some(sid),
                    };
                    save_config(data_dir, &cfg)?;

                    Ok(ResolvedIdentity {
                        keypair: kp,
                        storage,
                        identity_dir,
                        total_identities: 1,
                        newly_created: true,
                        stored_relay_url: None,
                    })
                }
                1 => {
                    let entry = &identities[0];
                    let db = db_path(&entry.path);
                    let storage = Storage::open(&db)?;
                    let row = storage
                        .get_identity()?
                        .ok_or_else(|| IdentityError::NotFound(entry.short_id.clone()))?;

                    let stored_relay = first_enabled_relay(&storage);

                    Ok(ResolvedIdentity {
                        keypair: row.to_stored_keypair(),
                        storage,
                        identity_dir: entry.path.clone(),
                        total_identities: 1,
                        newly_created: false,
                        stored_relay_url: stored_relay,
                    })
                }
                _ => {
                    let ids: Vec<String> = identities.iter().map(|e| e.short_id.clone()).collect();
                    Err(IdentityError::Ambiguous { available: ids })
                }
            }
        }
    }
}

/// Create a new identity in the data directory and return its resolved state.
pub fn create_identity(data_dir: &Path) -> Result<ResolvedIdentity, IdentityError> {
    fs::create_dir_all(data_dir)?;

    let kp = generate_keypair();
    let sid = short_id(&kp.id);
    let identity_dir = identities_dir(data_dir).join(&sid);
    fs::create_dir_all(&identity_dir)?;

    let db = db_path(&identity_dir);
    let storage = Storage::open(&db)?;
    let row = IdentityRow::from(&kp);
    storage.insert_identity(&row)?;

    let identities = list_identities(data_dir)?;

    // If this is the only identity, set it as default
    if identities.len() == 1 {
        let cfg = TenetConfig {
            default_identity: Some(sid),
        };
        save_config(data_dir, &cfg)?;
    }

    Ok(ResolvedIdentity {
        keypair: kp,
        storage,
        identity_dir,
        total_identities: identities.len(),
        newly_created: true,
        stored_relay_url: None,
    })
}

/// Store a relay URL in the identity's database.
pub fn store_relay_for_identity(storage: &Storage, url: &str) -> Result<(), StorageError> {
    use crate::storage::RelayRow;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    storage.insert_relay(&RelayRow {
        url: url.to_string(),
        added_at: now,
        last_sync: None,
        enabled: true,
    })
}

/// Get the first enabled relay URL from the identity's database.
fn first_enabled_relay(storage: &Storage) -> Option<String> {
    storage
        .list_enabled_relays()
        .ok()
        .and_then(|relays| relays.into_iter().next().map(|r| r.url))
}

/// Migrate legacy JSON files (identity.json, peers.json) into the identities structure.
fn migrate_legacy_json_to_identities(data_dir: &Path) -> Result<(), IdentityError> {
    let identity_json = data_dir.join("identity.json");
    if !identity_json.exists() {
        return Ok(());
    }

    // Don't migrate if there's also a legacy tenet.db (that path is handled separately)
    if db_path(data_dir).exists() {
        return Ok(());
    }

    // Load the identity from JSON
    let kp = crate::crypto::load_keypair(&identity_json).map_err(|e| {
        IdentityError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            e.to_string(),
        ))
    })?;
    let sid = short_id(&kp.id);
    let identity_dir = identities_dir(data_dir).join(&sid);
    fs::create_dir_all(&identity_dir)?;

    let db = db_path(&identity_dir);
    let storage = Storage::open(&db)?;

    // Migrate identity
    if storage.get_identity()?.is_none() {
        let row = IdentityRow::from(&kp);
        storage.insert_identity(&row)?;
    }

    // Run full JSON migration (handles peers, inbox, outbox too)
    let _ = storage.migrate_from_json(data_dir);

    eprintln!("  migrated legacy JSON files to identities/{}/", sid);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_resolve_creates_new_identity() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path();

        let resolved = resolve_identity(data_dir, None).unwrap();
        assert!(resolved.newly_created);
        assert_eq!(resolved.total_identities, 1);
        assert!(!resolved.keypair.id.is_empty());

        // Resolving again should find the same identity
        let resolved2 = resolve_identity(data_dir, None).unwrap();
        assert!(!resolved2.newly_created);
        assert_eq!(resolved2.keypair.id, resolved.keypair.id);
    }

    #[test]
    fn test_resolve_with_explicit_id() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path();

        // Create an identity first
        let created = create_identity(data_dir).unwrap();
        let sid = short_id(&created.keypair.id);

        // Resolve with explicit ID
        let resolved = resolve_identity(data_dir, Some(&sid)).unwrap();
        assert_eq!(resolved.keypair.id, created.keypair.id);
    }

    #[test]
    fn test_multiple_identities_require_selection() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path();

        // Create two identities
        let _id1 = create_identity(data_dir).unwrap();
        let _id2 = create_identity(data_dir).unwrap();

        // Clear the default so no identity is auto-selected
        let cfg = TenetConfig {
            default_identity: None,
        };
        save_config(data_dir, &cfg).unwrap();

        // Resolving without selection should fail
        let result = resolve_identity(data_dir, None);
        assert!(result.is_err());
    }

    #[test]
    fn test_config_default_identity() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path();

        // Create two identities
        let id1 = create_identity(data_dir).unwrap();
        let id2 = create_identity(data_dir).unwrap();
        let sid2 = short_id(&id2.keypair.id);

        // Set second as default
        let cfg = TenetConfig {
            default_identity: Some(sid2.clone()),
        };
        save_config(data_dir, &cfg).unwrap();

        // Now resolving without explicit should pick id2
        let resolved = resolve_identity(data_dir, None).unwrap();
        assert_eq!(resolved.keypair.id, id2.keypair.id);
    }

    #[test]
    fn test_legacy_db_migration() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path();

        // Create a legacy-style database directly in data_dir
        let legacy_db = db_path(data_dir);
        let storage = Storage::open(&legacy_db).unwrap();
        let kp = generate_keypair();
        let row = IdentityRow::from(&kp);
        storage.insert_identity(&row).unwrap();
        drop(storage);

        // Resolve should migrate it
        let resolved = resolve_identity(data_dir, None).unwrap();
        assert_eq!(resolved.keypair.id, kp.id);
        // Legacy DB should be moved
        assert!(!legacy_db.exists());
    }

    #[test]
    fn test_store_and_retrieve_relay() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path();

        let resolved = resolve_identity(data_dir, None).unwrap();
        store_relay_for_identity(&resolved.storage, "http://relay.example.com:8080").unwrap();

        // Resolve again and check stored relay
        let resolved2 = resolve_identity(data_dir, None).unwrap();
        assert_eq!(
            resolved2.stored_relay_url,
            Some("http://relay.example.com:8080".to_string())
        );
    }

    #[test]
    fn test_list_identities_empty() {
        let tmp = TempDir::new().unwrap();
        let entries = list_identities(tmp.path()).unwrap();
        assert!(entries.is_empty());
    }
}
