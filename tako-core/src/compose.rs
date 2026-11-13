use serde::{Deserialize, Serialize};

/// One Compose project on one server, with one HTTP service exposed by Tako.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ComposeConfig {
    /// Compose file, relative to the app directory and inside it.
    pub file: String,
    /// Compose service that receives HTTP traffic.
    pub service: String,
    /// Port the service listens on inside its container.
    pub port: u16,
    /// HTTP health check path.
    #[serde(default = "default_health_path")]
    pub health_path: String,
    /// HTTP Host header sent with health checks.
    #[serde(default)]
    pub health_host: Option<String>,
}

fn default_health_path() -> String {
    "/".into()
}

impl ComposeConfig {
    pub fn validate(&self) -> Result<(), String> {
        crate::container::validate_health_host(self.health_host.as_deref())?;
        let file = std::path::Path::new(&self.file);
        if self.file.is_empty()
            || file.is_absolute()
            || file
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err("compose.file must be relative to the app directory without '..'".into());
        }
        if self.service.is_empty()
            || !self
                .service
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
        {
            return Err("compose.service must name a Compose service".into());
        }
        if self.port == 0
            || !self.health_path.starts_with('/')
            || self
                .health_path
                .chars()
                .any(|c| c.is_whitespace() || c.is_control())
        {
            return Err("compose requires a nonzero port and an absolute HTTP health_path without whitespace".into());
        }
        Ok(())
    }
}
