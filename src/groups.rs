//! Group management for encrypted group messaging.
//!
//! This module provides types and functions for managing groups of peers who
//! share a symmetric encryption key for group messaging. Each group has a
//! unique ID, a symmetric key, and a set of member peer IDs.

use rand::rngs::OsRng;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

/// Error types for group operations
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupError {
    GroupNotFound(String),
    MemberAlreadyExists(String),
    MemberNotFound(String),
    InvalidGroupId,
    KeyGenerationFailed,
}

impl std::fmt::Display for GroupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GroupError::GroupNotFound(id) => write!(f, "group not found: {}", id),
            GroupError::MemberAlreadyExists(id) => write!(f, "member already exists: {}", id),
            GroupError::MemberNotFound(id) => write!(f, "member not found: {}", id),
            GroupError::InvalidGroupId => write!(f, "invalid group ID"),
            GroupError::KeyGenerationFailed => write!(f, "failed to generate group key"),
        }
    }
}

impl std::error::Error for GroupError {}

/// Information about a group
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupInfo {
    pub group_id: String,
    pub group_key: [u8; 32],
    pub members: HashSet<String>,
    pub created_at: u64,
    pub key_version: u32,
    pub creator_id: String,
}

impl GroupInfo {
    /// Create a new group with the given ID, members, and creator
    pub fn new(group_id: String, members: Vec<String>, creator_id: String) -> Self {
        let mut group_key = [0u8; 32];
        OsRng.fill_bytes(&mut group_key);

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        Self {
            group_id,
            group_key,
            members: members.into_iter().collect(),
            created_at: timestamp,
            key_version: 1,
            creator_id,
        }
    }

    /// Check if a peer is a member of this group
    pub fn is_member(&self, peer_id: &str) -> bool {
        self.members.contains(peer_id)
    }

    /// Add a member to the group
    pub fn add_member(&mut self, peer_id: String) -> Result<(), GroupError> {
        if self.members.contains(&peer_id) {
            return Err(GroupError::MemberAlreadyExists(peer_id));
        }
        self.members.insert(peer_id);
        Ok(())
    }

    /// Remove a member from the group
    pub fn remove_member(&mut self, peer_id: &str) -> Result<(), GroupError> {
        if !self.members.remove(peer_id) {
            return Err(GroupError::MemberNotFound(peer_id.to_string()));
        }
        Ok(())
    }

    /// Rotate the group key (generates a new key and increments version)
    pub fn rotate_key(&mut self) {
        let mut new_key = [0u8; 32];
        OsRng.fill_bytes(&mut new_key);
        self.group_key = new_key;
        self.key_version = self.key_version.saturating_add(1);
    }
}

/// Manager for handling multiple groups
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupManager {
    groups: HashMap<String, GroupInfo>,
}

impl GroupManager {
    /// Create a new empty group manager
    pub fn new() -> Self {
        Self {
            groups: HashMap::new(),
        }
    }

    /// Create a new group with the given members
    pub fn create_group(
        &mut self,
        group_id: String,
        members: Vec<String>,
        creator_id: String,
    ) -> Result<GroupInfo, GroupError> {
        if group_id.trim().is_empty() {
            return Err(GroupError::InvalidGroupId);
        }

        let group_info = GroupInfo::new(group_id.clone(), members, creator_id);
        self.groups.insert(group_id, group_info.clone());
        Ok(group_info)
    }

    /// Add a member to an existing group
    pub fn add_member(&mut self, group_id: &str, peer_id: &str) -> Result<(), GroupError> {
        let group = self
            .groups
            .get_mut(group_id)
            .ok_or_else(|| GroupError::GroupNotFound(group_id.to_string()))?;
        group.add_member(peer_id.to_string())
    }

    /// Remove a member from a group
    pub fn remove_member(&mut self, group_id: &str, peer_id: &str) -> Result<(), GroupError> {
        let group = self
            .groups
            .get_mut(group_id)
            .ok_or_else(|| GroupError::GroupNotFound(group_id.to_string()))?;
        group.remove_member(peer_id)
    }

    /// Rotate the key for a group
    pub fn rotate_key(&mut self, group_id: &str) -> Result<(), GroupError> {
        let group = self
            .groups
            .get_mut(group_id)
            .ok_or_else(|| GroupError::GroupNotFound(group_id.to_string()))?;
        group.rotate_key();
        Ok(())
    }

    /// Get the group key for a specific group
    pub fn get_group_key(&self, group_id: &str) -> Option<&[u8; 32]> {
        self.groups.get(group_id).map(|g| &g.group_key)
    }

    /// Get group information
    pub fn get_group(&self, group_id: &str) -> Option<&GroupInfo> {
        self.groups.get(group_id)
    }

    /// Get mutable group information
    pub fn get_group_mut(&mut self, group_id: &str) -> Option<&mut GroupInfo> {
        self.groups.get_mut(group_id)
    }

    /// Check if a group exists
    pub fn has_group(&self, group_id: &str) -> bool {
        self.groups.contains_key(group_id)
    }

    /// List all group IDs
    pub fn list_groups(&self) -> Vec<&str> {
        self.groups.keys().map(|s| s.as_str()).collect()
    }

    /// Get the number of groups
    pub fn group_count(&self) -> usize {
        self.groups.len()
    }

    /// Remove a group entirely
    pub fn remove_group(&mut self, group_id: &str) -> Option<GroupInfo> {
        self.groups.remove(group_id)
    }
}

impl Default for GroupManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_group_info_new() {
        let members = vec!["alice".to_string(), "bob".to_string()];
        let group = GroupInfo::new("group1".to_string(), members.clone(), "alice".to_string());

        assert_eq!(group.group_id, "group1");
        assert_eq!(group.members.len(), 2);
        assert!(group.is_member("alice"));
        assert!(group.is_member("bob"));
        assert_eq!(group.key_version, 1);
        assert_eq!(group.creator_id, "alice");
    }

    #[test]
    fn test_group_info_add_member() {
        let members = vec!["alice".to_string()];
        let mut group = GroupInfo::new("group1".to_string(), members, "alice".to_string());

        assert!(group.add_member("bob".to_string()).is_ok());
        assert!(group.is_member("bob"));
        assert_eq!(group.members.len(), 2);

        // Adding same member again should fail
        assert!(group.add_member("bob".to_string()).is_err());
    }

    #[test]
    fn test_group_info_remove_member() {
        let members = vec!["alice".to_string(), "bob".to_string()];
        let mut group = GroupInfo::new("group1".to_string(), members, "alice".to_string());

        assert!(group.remove_member("bob").is_ok());
        assert!(!group.is_member("bob"));
        assert_eq!(group.members.len(), 1);

        // Removing non-existent member should fail
        assert!(group.remove_member("charlie").is_err());
    }

    #[test]
    fn test_group_info_rotate_key() {
        let members = vec!["alice".to_string()];
        let mut group = GroupInfo::new("group1".to_string(), members, "alice".to_string());

        let original_key = group.group_key;
        let original_version = group.key_version;

        group.rotate_key();

        assert_ne!(group.group_key, original_key);
        assert_eq!(group.key_version, original_version + 1);
    }

    #[test]
    fn test_group_manager_create_group() {
        let mut manager = GroupManager::new();
        let members = vec!["alice".to_string(), "bob".to_string()];

        let group = manager
            .create_group("group1".to_string(), members, "alice".to_string())
            .unwrap();

        assert_eq!(group.group_id, "group1");
        assert_eq!(manager.group_count(), 1);
        assert!(manager.has_group("group1"));
    }

    #[test]
    fn test_group_manager_add_remove_member() {
        let mut manager = GroupManager::new();
        let members = vec!["alice".to_string()];
        manager
            .create_group("group1".to_string(), members, "alice".to_string())
            .unwrap();

        // Add member
        assert!(manager.add_member("group1", "bob").is_ok());
        assert!(manager.get_group("group1").unwrap().is_member("bob"));

        // Remove member
        assert!(manager.remove_member("group1", "bob").is_ok());
        assert!(!manager.get_group("group1").unwrap().is_member("bob"));
    }

    #[test]
    fn test_group_manager_get_group_key() {
        let mut manager = GroupManager::new();
        let members = vec!["alice".to_string()];
        let group = manager
            .create_group("group1".to_string(), members, "alice".to_string())
            .unwrap();

        let key = manager.get_group_key("group1").unwrap();
        assert_eq!(key, &group.group_key);

        assert!(manager.get_group_key("nonexistent").is_none());
    }

    #[test]
    fn test_group_manager_rotate_key() {
        let mut manager = GroupManager::new();
        let members = vec!["alice".to_string()];
        manager
            .create_group("group1".to_string(), members, "alice".to_string())
            .unwrap();

        let original_key = *manager.get_group_key("group1").unwrap();

        assert!(manager.rotate_key("group1").is_ok());

        let new_key = manager.get_group_key("group1").unwrap();
        assert_ne!(new_key, &original_key);
    }

    #[test]
    fn test_group_manager_list_groups() {
        let mut manager = GroupManager::new();
        manager
            .create_group(
                "group1".to_string(),
                vec!["alice".to_string()],
                "alice".to_string(),
            )
            .unwrap();
        manager
            .create_group(
                "group2".to_string(),
                vec!["bob".to_string()],
                "bob".to_string(),
            )
            .unwrap();

        let groups = manager.list_groups();
        assert_eq!(groups.len(), 2);
        assert!(groups.contains(&"group1"));
        assert!(groups.contains(&"group2"));
    }

    #[test]
    fn test_group_manager_remove_group() {
        let mut manager = GroupManager::new();
        manager
            .create_group(
                "group1".to_string(),
                vec!["alice".to_string()],
                "alice".to_string(),
            )
            .unwrap();

        assert!(manager.has_group("group1"));

        let removed = manager.remove_group("group1");
        assert!(removed.is_some());
        assert!(!manager.has_group("group1"));
        assert_eq!(manager.group_count(), 0);
    }

    #[test]
    fn test_group_manager_invalid_group_id() {
        let mut manager = GroupManager::new();
        let result = manager.create_group("".to_string(), vec![], "alice".to_string());
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), GroupError::InvalidGroupId));
    }
}
