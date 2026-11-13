use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

use super::error::{ConfigError, Result};

/// Server inventory resolved from authenticated network membership.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ServerInventory {
    /// Map of server name to server entry
    #[serde(default)]
    pub servers: HashMap<String, ServerEntry>,

    /// Optional detected runtime build targets by server name.
    #[serde(default)]
    pub server_targets: HashMap<String, ServerTarget>,
}

/// Detected server target metadata used for runtime-specific build planning.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServerTarget {
    /// CPU architecture (for example: x86_64, aarch64).
    pub arch: String,
    /// C library family (glibc).
    pub libc: String,
}

impl ServerTarget {
    /// Canonicalize target metadata values.
    pub fn normalized(arch: &str, libc: &str) -> std::result::Result<Self, String> {
        let arch = Self::normalize_arch(arch)
            .ok_or_else(|| format!("Unsupported server target architecture '{}'", arch.trim()))?;
        let libc = Self::normalize_libc(libc)
            .ok_or_else(|| format!("Unsupported server target libc '{}'", libc.trim()))?;
        Ok(Self { arch, libc })
    }

    /// Normalize architecture aliases.
    pub fn normalize_arch(value: &str) -> Option<String> {
        let normalized = value.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "x86_64" | "amd64" => Some("x86_64".to_string()),
            "aarch64" | "arm64" => Some("aarch64".to_string()),
            _ => None,
        }
    }

    /// Normalize libc aliases.
    pub fn normalize_libc(value: &str) -> Option<String> {
        let normalized = value.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "glibc" | "gnu" | "gnu-libc" | "gnu_libc" | "gnu libc" | "gnu c library" => {
                Some("glibc".to_string())
            }
            _ => None,
        }
    }

    /// Stable human-readable target label.
    pub fn label(&self) -> String {
        format!("linux-{}-{}", self.arch, self.libc)
    }
}

/// Server endpoint metadata and optional native network identity.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServerEntry {
    #[serde(skip)]
    pub(crate) native: Option<crate::management_http::NativeTarget>,

    /// Server hostname or IP address
    pub host: String,

    /// SSH port (default: 22)
    #[serde(default = "default_ssh_port")]
    pub port: u16,

    /// Public HTTP proxy port (default: 80).
    #[serde(default = "default_http_port")]
    pub http_port: u16,

    /// Public HTTPS proxy port (default: 443).
    #[serde(default = "default_https_port")]
    pub https_port: u16,

    /// Optional human-readable server description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Optional path to the SSH private key used to connect (stored as written, `~` allowed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_path: Option<PathBuf>,
}

fn default_ssh_port() -> u16 {
    22
}

fn default_http_port() -> u16 {
    80
}

fn default_https_port() -> u16 {
    443
}

impl Default for ServerEntry {
    fn default() -> Self {
        Self {
            native: None,
            host: String::new(),
            port: default_ssh_port(),
            http_port: default_http_port(),
            https_port: default_https_port(),
            description: None,
            key_path: None,
        }
    }
}

impl ServerEntry {
    pub(crate) fn management_target(&self) -> crate::management_http::ManagementTarget {
        match &self.native {
            Some(target) => crate::management_http::ManagementTarget::Native(target.clone()),
            None => crate::management_http::ManagementTarget::Legacy {
                host: self.host.clone(),
            },
        }
    }
}

impl ServerInventory {
    /// Resolve the last authenticated membership cached on this machine.
    /// Commands that change remote state refresh it before selecting targets.
    pub fn load() -> Result<Self> {
        let mut inventory = Self::default();
        for local in super::LocalNetworks::load()?.networks.into_values() {
            let profile = crate::network::load_profile(&local)
                .map_err(|error| ConfigError::Validation(error.to_string()))?;
            inventory.servers.extend(
                crate::commands::helpers::native_server_entries(&local, &profile)
                    .map_err(ConfigError::Validation)?,
            );
        }
        Ok(inventory)
    }

    /// Validate the configuration
    pub fn validate(&self) -> Result<()> {
        for (name, entry) in &self.servers {
            // Validate server name
            validate_server_name(name)?;

            // Validate host
            if entry.host.is_empty() {
                return Err(ConfigError::Validation(format!(
                    "Server '{}' has empty host",
                    name
                )));
            }

            // Validate port
            if entry.port == 0 {
                return Err(ConfigError::Validation(format!(
                    "Server '{}' has invalid port 0",
                    name
                )));
            }
            if entry.http_port == 0 {
                return Err(ConfigError::Validation(format!(
                    "Server '{}' has invalid HTTP port 0",
                    name
                )));
            }
            if entry.https_port == 0 {
                return Err(ConfigError::Validation(format!(
                    "Server '{}' has invalid HTTPS port 0",
                    name
                )));
            }
            if entry.http_port == entry.https_port {
                return Err(ConfigError::Validation(format!(
                    "Server '{}' HTTP and HTTPS ports must differ",
                    name
                )));
            }
        }

        for name in self.server_targets.keys() {
            if !self.servers.contains_key(name) {
                return Err(ConfigError::Validation(format!(
                    "Target metadata references unknown server '{}'",
                    name
                )));
            }
        }

        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&ServerEntry> {
        self.servers.get(name)
    }

    /// Check if a server exists by name
    pub fn contains(&self, name: &str) -> bool {
        self.servers.contains_key(name)
    }

    /// Check if a host already exists
    pub fn contains_host(&self, host: &str) -> bool {
        self.servers.values().any(|e| e.host == host)
    }

    /// Find server name by host
    pub fn find_by_host(&self, host: &str) -> Option<&str> {
        self.servers
            .iter()
            .find(|(_, e)| e.host == host)
            .map(|(name, _)| name.as_str())
    }

    /// Add a new server
    pub fn add(&mut self, name: String, entry: ServerEntry) -> Result<()> {
        if self.servers.contains_key(&name) {
            return Err(ConfigError::DuplicateServerName(name));
        }
        if self.contains_host(&entry.host) {
            return Err(ConfigError::DuplicateServerHost(entry.host.clone()));
        }
        validate_server_name(&name)?;
        self.servers.insert(name, entry);
        Ok(())
    }

    /// Remove a server by name
    pub fn remove(&mut self, name: &str) -> Result<ServerEntry> {
        let removed = self
            .servers
            .remove(name)
            .ok_or_else(|| ConfigError::ServerNotFound(name.to_string()))?;
        self.server_targets.remove(name);
        Ok(removed)
    }

    /// Update an existing server (by name, allows changing host)
    pub fn update(&mut self, name: &str, entry: ServerEntry) -> Result<()> {
        if !self.servers.contains_key(name) {
            return Err(ConfigError::ServerNotFound(name.to_string()));
        }

        // Check if new host conflicts with another server
        if let Some(existing_name) = self.find_by_host(&entry.host)
            && existing_name != name
        {
            return Err(ConfigError::DuplicateServerHost(entry.host.clone()));
        }

        self.servers.insert(name.to_string(), entry);
        Ok(())
    }

    /// Read detected target metadata for a server.
    pub fn get_target(&self, name: &str) -> Option<&ServerTarget> {
        self.server_targets.get(name)
    }

    /// Set detected target metadata for a server.
    pub fn set_target(&mut self, name: &str, target: ServerTarget) -> Result<()> {
        if !self.servers.contains_key(name) {
            return Err(ConfigError::ServerNotFound(name.to_string()));
        }
        let normalized = ServerTarget::normalized(&target.arch, &target.libc).map_err(|e| {
            ConfigError::Validation(format!(
                "Invalid target metadata for server '{}': {}",
                name, e
            ))
        })?;
        self.server_targets.insert(name.to_string(), normalized);
        Ok(())
    }

    /// Get all server names
    pub fn names(&self) -> Vec<&str> {
        self.servers.keys().map(|s| s.as_str()).collect()
    }

    /// Get number of servers
    pub fn len(&self) -> usize {
        self.servers.len()
    }

    /// Check if empty
    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }
}

/// Validate server name format
pub(super) fn validate_server_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(ConfigError::Validation(
            "Server name cannot be empty".to_string(),
        ));
    }

    if name.len() > 63 {
        return Err(ConfigError::Validation(
            "Server name cannot exceed 63 characters".to_string(),
        ));
    }

    // Must start with lowercase letter
    if !name
        .chars()
        .next()
        .map(|c| c.is_ascii_lowercase())
        .unwrap_or(false)
    {
        return Err(ConfigError::Validation(
            "Server name must start with a lowercase letter".to_string(),
        ));
    }

    // Only lowercase letters, numbers, and hyphens
    for c in name.chars() {
        if !c.is_ascii_lowercase() && !c.is_ascii_digit() && c != '-' {
            return Err(ConfigError::Validation(format!(
                "Server name can only contain lowercase letters, numbers, and hyphens. Found: '{}'",
                c
            )));
        }
    }

    // Cannot end with hyphen
    if name.ends_with('-') {
        return Err(ConfigError::Validation(
            "Server name cannot end with a hyphen".to_string(),
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests;
