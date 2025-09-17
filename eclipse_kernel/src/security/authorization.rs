//! Sistema de autorización para Eclipse OS

use alloc::string::String;
use alloc::vec::Vec;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Permission {
    Read,
    Write,
    Execute,
    Delete,
    Create,
    Modify,
}

#[derive(Debug, Clone)]
pub struct AccessControlEntry {
    pub user_id: u32,
    pub group_id: u32,
    pub permissions: Vec<Permission>,
    pub resource: String,
}

pub struct AuthorizationManager {
    access_control_list: Vec<AccessControlEntry>,
    initialized: bool,
}

impl AuthorizationManager {
    pub fn new() -> Self {
        Self {
            access_control_list: Vec::new(),
            initialized: false,
        }
    }

    pub fn initialize(&mut self) -> Result<(), &'static str> {
        if self.initialized {
            return Err("Authorization manager already initialized");
        }
        self.initialized = true;
        Ok(())
    }

    pub fn check_permission(&self, user_id: u32, group_id: u32, resource: &str, permission: Permission) -> bool {
        if !self.initialized {
            return false;
        }

        for entry in &self.access_control_list {
            if entry.resource == resource && (entry.user_id == user_id || entry.group_id == group_id) {
                return entry.permissions.contains(&permission);
            }
        }

        false
    }

    pub fn grant_permission(&mut self, user_id: u32, group_id: u32, resource: String, permissions: Vec<Permission>) -> Result<(), &'static str> {
        if !self.initialized {
            return Err("Authorization manager not initialized");
        }

        let entry = AccessControlEntry {
            user_id,
            group_id,
            permissions,
            resource,
        };

        self.access_control_list.push(entry);
        Ok(())
    }

    pub fn revoke_permission(&mut self, user_id: u32, resource: &str) -> Result<(), &'static str> {
        if !self.initialized {
            return Err("Authorization manager not initialized");
        }

        self.access_control_list.retain(|entry| !(entry.user_id == user_id && entry.resource == resource));
        Ok(())
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized
    }
}
