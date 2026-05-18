use super::schema::*;
use crate::build::BuildAdapter;
use crate::config::error::{ConfigError, Result};
use std::path::{Component, Path};

impl Config {
    /// Validate the configuration
    pub fn validate(&self) -> Result<()> {
        // Validate app name if specified
        if let Some(name) = &self.name {
            validate_app_name(name)?;
        }

        if let Some(main) = &self.main
            && main.trim().is_empty()
        {
            return Err(ConfigError::Validation("main cannot be empty".to_string()));
        }

        if let Some(preset) = &self.preset
            && preset.trim().is_empty()
        {
            return Err(ConfigError::Validation(
                "preset cannot be empty".to_string(),
            ));
        }
        if let Some(preset) = &self.preset {
            let trimmed = preset.trim();
            if trimmed.starts_with("github:") {
                return Err(ConfigError::Validation(
                    "github preset references are not supported; use official aliases only."
                        .to_string(),
                ));
            }
            if trimmed.contains(':') {
                return Err(ConfigError::Validation(
                    "preset must be an official alias (for example `tanstack-start`); ':' references are not supported."
                        .to_string(),
                ));
            }
            if !trimmed.is_empty() && trimmed.contains('/') {
                return Err(ConfigError::Validation(
                    "preset must not include runtime namespace; set top-level `runtime` and use a local preset name (for example `preset = \"tanstack-start\"`).".to_string(),
                ));
            }
        }
        if let Some(runtime) = &self.runtime {
            let trimmed = runtime.trim();
            if trimmed.is_empty() {
                return Err(ConfigError::Validation(
                    "runtime cannot be empty".to_string(),
                ));
            }
            if BuildAdapter::from_id(trimmed).is_none() {
                return Err(ConfigError::Validation(
                    "runtime must be one of: bun, node, go".to_string(),
                ));
            }
        }
        if let Some(version) = &self.runtime_version_pin
            && version.trim().is_empty()
        {
            return Err(ConfigError::Validation(
                "runtime version cannot be empty".to_string(),
            ));
        }
        for asset_path in &self.assets {
            validate_asset_path(asset_path)?;
        }
        if let Some(app_root) = &self.app_root {
            validate_relative_dir(app_root, "app_root")?;
        }
        for worker_name in self.workflows.groups.keys() {
            validate_workflow_worker_name(worker_name)?;
        }
        for (server_name, server) in &self.servers.per_server {
            validate_server_name(server_name)?;
            if let Some(workflows) = &server.workflows {
                for worker_name in workflows.groups.keys() {
                    validate_workflow_worker_name(worker_name)?;
                }
            }
        }
        self.images.validate().map_err(|error| {
            ConfigError::Validation(format!("invalid [images] configuration: {error}"))
        })?;
        if let Some(cwd) = &self.build.cwd {
            validate_relative_dir(cwd, "build.cwd")?;
        }
        for include in &self.build.include {
            validate_build_glob(include, "build.include")?;
        }
        for exclude in &self.build.exclude {
            validate_build_glob(exclude, "build.exclude")?;
        }
        // Mutual exclusion: [build] and [[build_stages]] cannot both be set
        let has_build_run = self
            .build
            .run
            .as_deref()
            .is_some_and(|r| !r.trim().is_empty());
        if has_build_run && !self.build_stages.is_empty() {
            return Err(ConfigError::Validation(
                "Cannot use both [build] with 'run' and [[build_stages]]; they are mutually exclusive."
                    .to_string(),
            ));
        }
        if !self.build_stages.is_empty()
            && (!self.build.include.is_empty() || !self.build.exclude.is_empty())
        {
            return Err(ConfigError::Validation(
                "Cannot use [build] include/exclude with [[build_stages]]; use per-stage exclude instead."
                    .to_string(),
            ));
        }
        for (index, stage) in self.build_stages.iter().enumerate() {
            validate_build_stage(stage, index)?;
            for exclude in &stage.exclude {
                validate_build_glob(exclude, &format!("build_stages[{index}].exclude"))?;
            }
        }

        // Validate each environment
        for (env_name, env_config) in &self.envs {
            let is_development = env_name == "development";

            // Cannot have both route and routes
            if env_config.route.is_some() && env_config.routes.is_some() {
                return Err(ConfigError::Validation(format!(
                    "Environment '{}' cannot have both 'route' and 'routes'",
                    env_name
                )));
            }

            if !is_development && env_config.route.is_none() && env_config.routes.is_none() {
                return Err(ConfigError::Validation(format!(
                    "Environment '{}' must define either 'route' or 'routes'",
                    env_name
                )));
            }

            if let Some(routes) = &env_config.routes
                && routes.is_empty()
                && !is_development
            {
                return Err(ConfigError::Validation(format!(
                    "Environment '{}' has empty 'routes'; define at least one route",
                    env_name
                )));
            }

            // Validate route patterns
            if let Some(route) = &env_config.route {
                validate_route_pattern(route)?;
            }
            if let Some(routes) = &env_config.routes {
                for route in routes {
                    validate_route_pattern(route)?;
                }
            }
            if env_config.idle_timeout == 0 {
                return Err(ConfigError::Validation(format!(
                    "Environment '{}' has invalid idle_timeout 0",
                    env_name
                )));
            }
            for server_name in &env_config.servers {
                validate_server_name(server_name)?;
            }
            for (binding_name, resource_name) in &env_config.storages {
                super::super::validate_storage_name(binding_name)?;
                super::super::validate_storage_name(resource_name)?;
                if !is_development
                    && resource_name != super::super::BUILTIN_LOCAL_STORAGE_RESOURCE_NAME
                    && !self.storages.contains_key(resource_name)
                {
                    return Err(ConfigError::Validation(format!(
                        "Environment '{}' storage '{}' references missing storage resource '{}'",
                        env_name, binding_name, resource_name
                    )));
                }
            }
        }

        for (resource_name, resource) in &self.storages {
            super::super::validate_storage_name(resource_name)?;
            validate_storage_resource(resource_name, resource)?;
        }

        Ok(())
    }
}

fn validate_storage_resource(name: &str, resource: &StorageResourceConfig) -> Result<()> {
    if name == super::super::BUILTIN_LOCAL_STORAGE_RESOURCE_NAME {
        return Err(ConfigError::Validation(
            "Storage resource 'local' is built in and cannot be declared; use storages = { <binding> = \"local\" } to select local storage"
                .to_string(),
        ));
    }

    match resource.provider {
        tako_core::StorageProvider::Local => {
            return Err(ConfigError::Validation(format!(
                "Storage resource '{name}' cannot set provider 'local'; use storages = {{ <binding> = \"local\" }} to select built-in local storage"
            )));
        }
        tako_core::StorageProvider::S3 => {
            validate_required_storage_field(name, "bucket", resource.bucket.as_deref())?;
            validate_required_storage_field(name, "endpoint", resource.endpoint.as_deref())?;
            validate_required_storage_field(name, "region", resource.region.as_deref())?;
            validate_https_url(
                resource.endpoint.as_deref().unwrap_or_default(),
                &format!("storages.{name}.endpoint"),
            )?;
            if let Some(public_base_url) = &resource.public_base_url {
                validate_https_url(public_base_url, &format!("storages.{name}.public_base_url"))?;
            }
        }
    }
    Ok(())
}

fn validate_required_storage_field(name: &str, field: &str, value: Option<&str>) -> Result<()> {
    if value.is_none_or(|value| value.trim().is_empty()) {
        return Err(ConfigError::Validation(format!(
            "Storage resource '{name}' with provider 's3' must set '{field}'"
        )));
    }
    Ok(())
}

fn validate_https_url(value: &str, field: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(value)
        .map_err(|e| ConfigError::Validation(format!("'{field}' must be a valid URL: {e}")))?;
    if parsed.scheme() != "https" {
        return Err(ConfigError::Validation(format!("'{field}' must use https")));
    }
    if parsed.host_str().is_none() {
        return Err(ConfigError::Validation(format!(
            "'{field}' must include a host"
        )));
    }
    Ok(())
}

pub(super) fn validate_top_level_keys(raw: &toml::Value) -> Result<()> {
    let Some(table) = raw.as_table() else {
        return Err(ConfigError::Validation(
            "tako.toml must be a TOML table".to_string(),
        ));
    };

    for key in table.keys() {
        if !matches!(
            key.as_str(),
            "name"
                | "runtime"
                | "package_manager"
                | "preset"
                | "dev"
                | "app_root"
                | "main"
                | "assets"
                | "release"
                | "build"
                | "build_stages"
                | "workflows"
                | "images"
                | "vars"
                | "envs"
                | "storages"
                | "servers"
        ) {
            return Err(ConfigError::Validation(format!("Unknown key '{}'", key)));
        }
    }

    Ok(())
}

fn validate_relative_dir(value: &str, field: &str) -> Result<()> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(ConfigError::Validation(format!(
            "'{field}' cannot be empty"
        )));
    }
    let path = Path::new(trimmed);
    if path.is_absolute() {
        return Err(ConfigError::Validation(format!(
            "'{field}' must be a relative path"
        )));
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(ConfigError::Validation(format!(
            "'{field}' must not contain '..'"
        )));
    }
    Ok(())
}

fn validate_build_glob(pattern: &str, field: &str) -> Result<()> {
    let trimmed = pattern.trim();
    if trimmed.is_empty() {
        return Err(ConfigError::Validation(format!(
            "{field} entries cannot be empty"
        )));
    }

    let path = Path::new(trimmed);
    if path.is_absolute() {
        return Err(ConfigError::Validation(format!(
            "{field} entry '{}' must be relative to project root",
            pattern
        )));
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(ConfigError::Validation(format!(
            "{field} entry '{}' must not contain '..'",
            pattern
        )));
    }

    Ok(())
}

fn validate_asset_path(asset_path: &str) -> Result<()> {
    let trimmed = asset_path.trim();
    if trimmed.is_empty() {
        return Err(ConfigError::Validation(
            "assets entry cannot be empty".to_string(),
        ));
    }

    let path = Path::new(trimmed);
    if path.is_absolute() {
        return Err(ConfigError::Validation(format!(
            "assets entry '{}' must be relative to project root",
            asset_path
        )));
    }

    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(ConfigError::Validation(format!(
            "assets entry '{}' must not contain '..'",
            asset_path
        )));
    }

    Ok(())
}

fn validate_build_stage(stage: &BuildStage, index: usize) -> Result<()> {
    if let Some(cwd) = &stage.cwd {
        validate_build_stage_cwd(cwd, index)?;
    }
    if stage.run.trim().is_empty() {
        return Err(ConfigError::Validation(format!(
            "'build_stages[{index}].run' cannot be empty"
        )));
    }
    Ok(())
}

fn validate_build_stage_cwd(cwd: &str, index: usize) -> Result<()> {
    let trimmed = cwd.trim();
    if trimmed.is_empty() {
        return Err(ConfigError::Validation(format!(
            "'build_stages[{index}].cwd' cannot be empty"
        )));
    }
    let path = Path::new(trimmed);
    if path.is_absolute() {
        return Err(ConfigError::Validation(format!(
            "'build_stages[{index}].cwd' must be relative"
        )));
    }
    // Allow ".." for monorepo traversal. The workspace-root escape guard runs at deploy
    // time when the actual workspace root is known (see resolve_stage_working_dir_for_local_build).
    Ok(())
}

/// Validate app name format
pub(super) fn validate_app_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(ConfigError::Validation(
            "App name cannot be empty".to_string(),
        ));
    }

    if name.len() > 63 {
        return Err(ConfigError::Validation(
            "App name cannot exceed 63 characters".to_string(),
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
            "App name must start with a lowercase letter".to_string(),
        ));
    }

    // Only lowercase letters, numbers, and hyphens
    for c in name.chars() {
        if !c.is_ascii_lowercase() && !c.is_ascii_digit() && c != '-' {
            return Err(ConfigError::Validation(format!(
                "App name can only contain lowercase letters, numbers, and hyphens. Found: '{}'",
                c
            )));
        }
    }

    // Cannot end with hyphen
    if name.ends_with('-') {
        return Err(ConfigError::Validation(
            "App name cannot end with a hyphen".to_string(),
        ));
    }

    Ok(())
}

/// Validate server name format (same rules as app name)
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

fn validate_workflow_worker_name(name: &str) -> Result<()> {
    validate_server_name(name).map_err(|_| {
        ConfigError::Validation(format!(
            "Workflow worker group '{}' must start with a lowercase letter and contain only lowercase letters, numbers, and hyphens",
            name
        ))
    })
}

/// Validate route pattern format
pub(super) fn validate_route_pattern(pattern: &str) -> Result<()> {
    if pattern.is_empty() {
        return Err(ConfigError::InvalidRoutePattern(
            "Route pattern cannot be empty".to_string(),
        ));
    }

    // Basic validation - routes can be:
    // - Exact hostname: api.example.com
    // - Wildcard subdomain: *.example.com
    // - Path-based: example.com/api/*
    // - Combined: *.example.com/admin/*

    // Check for invalid characters
    for c in pattern.chars() {
        if !c.is_ascii_alphanumeric() && c != '.' && c != '-' && c != '*' && c != '/' {
            return Err(ConfigError::InvalidRoutePattern(format!(
                "Invalid character in route pattern: '{}'",
                c
            )));
        }
    }

    // Wildcard must be at start of a segment
    if pattern.contains('*') {
        let parts: Vec<&str> = pattern.split('/').collect();
        let hostname = parts[0];

        // Check hostname wildcards
        if hostname.contains('*') && !hostname.starts_with("*.") {
            return Err(ConfigError::InvalidRoutePattern(
                "Wildcard in hostname must be at the start (e.g., *.example.com)".to_string(),
            ));
        }

        // Check path wildcards
        for part in parts.iter().skip(1) {
            if part.contains('*') && *part != "*" {
                return Err(ConfigError::InvalidRoutePattern(
                    "Wildcard in path must be a complete segment (e.g., /api/*)".to_string(),
                ));
            }
        }
    }

    Ok(())
}
