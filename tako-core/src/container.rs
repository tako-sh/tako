//! Container runtime settings shared by config, release artifacts, and the server.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(default, deny_unknown_fields)]
pub struct ContainerOptions {
    /// Arguments that replace the image command.
    pub command: Vec<String>,
    /// Program that replaces the image entrypoint.
    pub entrypoint: Option<String>,
    /// Port the app listens on inside the container.
    pub port: u16,
    /// Use the Tako SDK bootstrap and authenticated health contract.
    pub sdk: bool,
    /// HTTP health check path.
    pub health_path: String,
    /// HTTP Host header sent with health checks. Only for plain images (`sdk = false`).
    pub health_host: Option<String>,
    /// Explicit secret names to deliver as environment variables for plain images.
    pub secret_env: Vec<String>,
    /// The app's persistent data directory inside the container.
    pub data_dir: String,
    /// Map this container identity to the service user's host identity.
    /// Useful for images whose USER is non-root, without chowning shared data.
    pub user: Option<ContainerUser>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ContainerUser {
    pub uid: u32,
    pub gid: u32,
}

impl Default for ContainerOptions {
    fn default() -> Self {
        Self {
            command: Vec::new(),
            entrypoint: None,
            port: 3000,
            sdk: true,
            health_path: "/status".into(),
            health_host: None,
            secret_env: Vec::new(),
            data_dir: "/tako/data".to_string(),
            user: None,
        }
    }
}

impl ContainerOptions {
    pub fn validate(&self) -> Result<(), String> {
        validate_health_host(self.health_host.as_deref())?;
        if self.sdk && self.health_host.is_some() {
            return Err("SDK containers use Tako's internal health host".into());
        }
        if self
            .entrypoint
            .as_ref()
            .is_some_and(|value| value.trim().is_empty() || value.contains('\0'))
            || self.command.iter().any(|value| value.contains('\0'))
        {
            return Err("invalid container command or entrypoint".into());
        }
        if self.port == 0 {
            return Err("container_options.port must be between 1 and 65535".into());
        }
        if !self.health_path.starts_with('/')
            || self
                .health_path
                .chars()
                .any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(
                "container_options.health_path must be an absolute HTTP path without whitespace"
                    .into(),
            );
        }
        if self.sdk && self.health_path != "/status" {
            return Err("SDK containers use the /status health endpoint".into());
        }
        for key in &self.secret_env {
            if key.is_empty()
                || key.starts_with("TAKO_")
                || matches!(key.as_str(), "HOST" | "PORT")
                || !key.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
            {
                return Err(format!("invalid container secret environment name '{key}'"));
            }
        }
        let path = std::path::Path::new(&self.data_dir);
        if !path.is_absolute()
            || path.parent().is_none()
            || self.data_dir.chars().any(|c| c == ',' || c.is_control())
            || path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err("container_options.data_dir must be an absolute non-root path without '..', commas, or control characters".to_string());
        }
        Ok(())
    }
}

pub fn validate_health_host(host: Option<&str>) -> Result<(), String> {
    if host.is_some_and(|host| {
        host.is_empty()
            || !host.is_ascii()
            || host
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || c == '/')
    }) {
        return Err(
            "health_host must be an HTTP host without a scheme, path, or whitespace".into(),
        );
    }
    Ok(())
}
