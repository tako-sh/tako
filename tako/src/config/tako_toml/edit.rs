use super::schema::Config;
use super::schema::StorageResourceConfig;
use crate::config::error::{ConfigError, Result};
use std::fs;
use std::path::Path;

impl Config {
    /// Add a server to `[envs.<name>].servers` in `tako.toml` under the given directory.
    pub fn upsert_server_env_in_dir<P: AsRef<Path>>(
        dir: P,
        server_name: &str,
        env: &str,
    ) -> Result<()> {
        let path = dir.as_ref().join("tako.toml");
        Self::upsert_server_env_in_file(path, server_name, env)
    }

    pub fn upsert_server_env_in_file<P: AsRef<Path>>(
        path: P,
        server_name: &str,
        env: &str,
    ) -> Result<()> {
        let path = path.as_ref();
        let mut doc = load_or_create_toml_document(path)?;
        let root = doc
            .as_table_mut()
            .ok_or_else(|| ConfigError::Validation("tako.toml must be a TOML table".to_string()))?;

        let envs = root
            .entry("envs")
            .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
            .as_table_mut()
            .ok_or_else(|| {
                ConfigError::Validation(
                    "Invalid [envs] section: expected table structure".to_string(),
                )
            })?;

        for (env_name, env_value) in envs.iter_mut() {
            if env_name == "development" || env_name == env {
                continue;
            }
            let Some(env_table) = env_value.as_table_mut() else {
                return Err(ConfigError::Validation(format!(
                    "Cannot update env '{}': [envs.{}] is not a table",
                    env_name, env_name
                )));
            };
            if let Some(existing_servers) = env_table.get_mut("servers") {
                let Some(array) = existing_servers.as_array_mut() else {
                    return Err(ConfigError::Validation(format!(
                        "Cannot update env '{}': [envs.{}].servers must be an array",
                        env_name, env_name
                    )));
                };
                array.retain(|value| value.as_str() != Some(server_name));
            }
        }

        let env_entry = envs
            .entry(env.to_string())
            .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
        let Some(env_table) = env_entry.as_table_mut() else {
            return Err(ConfigError::Validation(format!(
                "Cannot map server '{}': [envs.{}] is not a table",
                server_name, env
            )));
        };

        match env_table.get_mut("servers") {
            Some(existing_servers) => {
                let Some(array) = existing_servers.as_array_mut() else {
                    return Err(ConfigError::Validation(format!(
                        "Cannot map server '{}': [envs.{}].servers must be an array",
                        server_name, env
                    )));
                };
                if !array
                    .iter()
                    .any(|value| value.as_str() == Some(server_name))
                {
                    array.push(toml::Value::String(server_name.to_string()));
                }
            }
            None => {
                env_table.insert(
                    "servers".to_string(),
                    toml::Value::Array(vec![toml::Value::String(server_name.to_string())]),
                );
            }
        }

        let rendered = toml::to_string_pretty(&doc)
            .map_err(|e| ConfigError::Validation(format!("Failed to render tako.toml: {}", e)))?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| ConfigError::FileWrite(parent.to_path_buf(), e))?;
        }
        fs::write(path, rendered).map_err(|e| ConfigError::FileWrite(path.to_path_buf(), e))?;
        Ok(())
    }

    pub fn upsert_storage_binding_in_file<P: AsRef<Path>>(
        path: P,
        env: &str,
        binding_name: &str,
        resource_name: &str,
        resource: Option<&StorageResourceConfig>,
    ) -> Result<()> {
        let path = path.as_ref();
        let mut doc = load_or_create_toml_document(path)?;
        let root = doc
            .as_table_mut()
            .ok_or_else(|| ConfigError::Validation("tako.toml must be a TOML table".to_string()))?;

        if let Some(resource) = resource {
            let storages = root
                .entry("storages")
                .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
                .as_table_mut()
                .ok_or_else(|| {
                    ConfigError::Validation(
                        "Invalid [storages] section: expected table structure".to_string(),
                    )
                })?;
            storages.insert(
                resource_name.to_string(),
                toml::Value::try_from(resource).map_err(|e| {
                    ConfigError::Validation(format!("Failed to encode storage resource: {e}"))
                })?,
            );
        }

        let envs = root
            .entry("envs")
            .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
            .as_table_mut()
            .ok_or_else(|| {
                ConfigError::Validation(
                    "Invalid [envs] section: expected table structure".to_string(),
                )
            })?;
        let env_entry = envs
            .entry(env.to_string())
            .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
        let env_table = env_entry.as_table_mut().ok_or_else(|| {
            ConfigError::Validation(format!("Cannot map storage: [envs.{env}] is not a table"))
        })?;
        let storage_map = env_table
            .entry("storages")
            .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
            .as_table_mut()
            .ok_or_else(|| {
                ConfigError::Validation(format!(
                    "Cannot map storage: [envs.{env}].storages must be an inline table or table"
                ))
            })?;
        storage_map.insert(
            binding_name.to_string(),
            toml::Value::String(resource_name.to_string()),
        );

        let rendered = toml::to_string_pretty(&doc)
            .map_err(|e| ConfigError::Validation(format!("Failed to render tako.toml: {}", e)))?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| ConfigError::FileWrite(parent.to_path_buf(), e))?;
        }
        fs::write(path, rendered).map_err(|e| ConfigError::FileWrite(path.to_path_buf(), e))?;
        Ok(())
    }
}

fn load_or_create_toml_document(path: &Path) -> Result<toml::Value> {
    if !path.exists() {
        return Ok(toml::Value::Table(toml::map::Map::new()));
    }

    let content =
        fs::read_to_string(path).map_err(|e| ConfigError::FileRead(path.to_path_buf(), e))?;
    if content.trim().is_empty() {
        return Ok(toml::Value::Table(toml::map::Map::new()));
    }

    toml::from_str::<toml::Value>(&content).map_err(ConfigError::TomlParse)
}
