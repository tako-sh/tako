use super::schema::*;
use super::validation::validate_top_level_keys;
use crate::config::error::{ConfigError, Result};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

impl Config {
    /// Load tako.toml from a directory
    pub fn load_from_dir<P: AsRef<Path>>(dir: P) -> Result<Self> {
        let path = dir.as_ref().join("tako.toml");
        if !path.exists() {
            return Err(ConfigError::Validation(format!(
                "tako.toml not found at {}",
                path.display()
            )));
        }

        Self::load_from_file(&path)
    }

    /// Load tako.toml from a specific file
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let content = fs::read_to_string(path.as_ref())
            .map_err(|e| ConfigError::FileRead(path.as_ref().to_path_buf(), e))?;
        let config = Self::parse(&content)?;
        Ok(config)
    }

    /// Parse tako.toml content
    pub fn parse(content: &str) -> Result<Self> {
        if content.trim().is_empty() {
            return Ok(Self::default());
        }

        // First parse into a raw Value so the current schema can be validated.
        let raw: toml::Value = toml::from_str(content)?;
        validate_top_level_keys(&raw)?;

        // Parse top-level metadata
        let name = parse_optional_string(&raw, "name")?;
        let main = parse_optional_string(&raw, "main")?;
        let (runtime, runtime_version_pin) = parse_runtime_spec(&raw)?;
        let package_manager = parse_optional_string(&raw, "package_manager")?;
        let preset = parse_optional_string(&raw, "preset")?;
        let dev = parse_string_array(&raw, "dev")?.unwrap_or_default();
        let app_root = parse_optional_string(&raw, "app_root")?;
        let assets = parse_string_array(&raw, "assets")?.unwrap_or_default();
        let release = parse_optional_string(&raw, "release")?;
        let build = parse_build_config(&raw)?;
        let build_stages = parse_build_stages(&raw)?;
        let workflows = parse_workflows_config(&raw, "workflows")?.unwrap_or_default();
        let images = parse_images_config(&raw)?;
        let storages = parse_storage_resources(&raw)?;
        let mut config = Config {
            name,
            main,
            runtime,
            runtime_version_pin,
            package_manager,
            preset,
            dev,
            app_root,
            assets,
            release,
            build,
            build_stages,
            workflows,
            images,
            storages,
            ..Config::default()
        };

        // Parse [vars] section (global) and [vars.*] sections (per-environment)
        if let Some(vars) = raw.get("vars")
            && let Some(table) = vars.as_table()
        {
            for (key, value) in table {
                if let Some(nested_table) = value.as_table() {
                    // Nested table - per-environment vars [vars.production], etc.
                    let mut env_vars = HashMap::new();
                    for (var_name, var_value) in nested_table {
                        env_vars.insert(
                            var_name.clone(),
                            parse_var_value(var_value, &format!("[vars.{key}].{var_name}"))?,
                        );
                    }
                    config.vars_per_env.insert(key.clone(), env_vars);
                } else {
                    // Direct scalar value - global var
                    config.vars.insert(
                        key.clone(),
                        parse_var_value(value, &format!("[vars].{key}"))?,
                    );
                }
            }
        }

        // Parse [envs.*] sections
        if let Some(envs) = raw.get("envs")
            && let Some(table) = envs.as_table()
        {
            for (env_name, env_value) in table {
                let env_config: EnvConfig = toml::from_str(&toml::to_string(env_value)?)?;
                config.envs.insert(env_name.clone(), env_config);
            }
        }

        // Parse [servers.*] sections.
        if let Some(servers) = raw.get("servers")
            && let Some(table) = servers.as_table()
        {
            for (key, value) in table {
                if key == "workflows" {
                    return Err(ConfigError::Validation(
                        "[servers.workflows] is no longer valid. Use top-level [workflows] for app-wide workflow settings, or [servers.<name>.workflows] for a server override."
                            .to_string(),
                    ));
                }
                let server_config = parse_server_config(value)?;
                config.servers.per_server.insert(key.clone(), server_config);
            }
        }

        config.validate()?;
        Ok(config)
    }
}

fn parse_var_value(value: &toml::Value, path: &str) -> Result<String> {
    match value {
        toml::Value::String(value) => Ok(value.clone()),
        toml::Value::Integer(value) => Ok(value.to_string()),
        toml::Value::Float(value) => Ok(value.to_string()),
        toml::Value::Boolean(value) => Ok(value.to_string()),
        toml::Value::Datetime(value) => Ok(value.to_string()),
        toml::Value::Array(_) | toml::Value::Table(_) => Err(ConfigError::Validation(format!(
            "{path} must be a string, number, boolean, or datetime"
        ))),
    }
}

fn parse_storage_resources(raw: &toml::Value) -> Result<HashMap<String, StorageResourceConfig>> {
    let mut resources = HashMap::new();
    let Some(storages) = raw.get("storages") else {
        return Ok(resources);
    };
    let table = storages
        .as_table()
        .ok_or_else(|| ConfigError::Validation("'storages' must be a table".to_string()))?;

    for (name, value) in table {
        let resource: StorageResourceConfig = toml::from_str(&toml::to_string(value)?)?;
        resources.insert(name.clone(), resource);
    }

    Ok(resources)
}

fn parse_images_config(raw: &toml::Value) -> Result<tako_images::ImagesConfig> {
    let Some(value) = raw.get("images") else {
        return Ok(tako_images::ImagesConfig::default());
    };
    toml::from_str(&toml::to_string(value)?).map_err(ConfigError::TomlParse)
}

fn parse_server_config(value: &toml::Value) -> Result<ServerConfig> {
    let table = value
        .as_table()
        .ok_or_else(|| ConfigError::Validation("[servers.<name>] must be a table".to_string()))?;
    for key in table.keys() {
        if key != "workflows" {
            return Err(ConfigError::Validation(format!(
                "Unknown key 'servers.<name>.{key}'"
            )));
        }
    }
    let workflows = parse_workflows_config(value, "workflows")?;
    Ok(ServerConfig { workflows })
}

fn parse_workflows_config(raw: &toml::Value, key: &str) -> Result<Option<WorkflowsConfig>> {
    let Some(value) = raw.get(key) else {
        return Ok(None);
    };
    let table = value
        .as_table()
        .ok_or_else(|| ConfigError::Validation(format!("'{key}' must be a table")))?;

    let mut config = WorkflowsConfig::default();
    for (field, field_value) in table {
        match field.as_str() {
            "workers" => {
                config.base.workers = Some(parse_u32_field(field_value, &format!("{key}.workers"))?)
            }
            "concurrency" => {
                config.base.concurrency =
                    Some(parse_u32_field(field_value, &format!("{key}.concurrency"))?)
            }
            worker => {
                let group_table = field_value.as_table().ok_or_else(|| {
                    ConfigError::Validation(format!("'{key}.{worker}' must be a table"))
                })?;
                config.groups.insert(
                    worker.to_string(),
                    parse_workflow_worker_config(group_table, &format!("{key}.{worker}"))?,
                );
            }
        }
    }

    Ok(Some(config))
}

fn parse_workflow_worker_config(
    table: &toml::value::Table,
    path: &str,
) -> Result<WorkflowWorkerConfig> {
    let mut config = WorkflowWorkerConfig::default();
    for (field, value) in table {
        match field.as_str() {
            "workers" => config.workers = Some(parse_u32_field(value, &format!("{path}.workers"))?),
            "concurrency" => {
                config.concurrency = Some(parse_u32_field(value, &format!("{path}.concurrency"))?)
            }
            other => {
                return Err(ConfigError::Validation(format!(
                    "Unknown key '{path}.{other}'"
                )));
            }
        }
    }
    Ok(config)
}

fn parse_u32_field(value: &toml::Value, path: &str) -> Result<u32> {
    let n = value
        .as_integer()
        .ok_or_else(|| ConfigError::Validation(format!("'{path}' must be an integer")))?;
    u32::try_from(n).map_err(|_| {
        ConfigError::Validation(format!("'{path}' must be between 0 and {}", u32::MAX))
    })
}

fn parse_build_config(raw: &toml::Value) -> Result<BuildConfig> {
    let Some(value) = raw.get("build") else {
        return Ok(BuildConfig::default());
    };

    let table = value
        .as_table()
        .ok_or_else(|| ConfigError::Validation("'build' must be a table ([build])".to_string()))?;
    validate_build_keys(table)?;
    let table_value = toml::Value::Table(table.clone());

    let run = parse_optional_string(&table_value, "run")?;
    let install = parse_optional_string(&table_value, "install")?;
    let cwd = parse_optional_string(&table_value, "cwd")?;
    let include = parse_string_array(&table_value, "include")?.unwrap_or_default();
    let exclude = parse_string_array(&table_value, "exclude")?.unwrap_or_default();

    Ok(BuildConfig {
        run,
        install,
        cwd,
        include,
        exclude,
    })
}

fn validate_build_keys(table: &toml::value::Table) -> Result<()> {
    for key in table.keys() {
        if !matches!(
            key.as_str(),
            "run" | "install" | "cwd" | "include" | "exclude"
        ) {
            return Err(ConfigError::Validation(format!(
                "Unknown key 'build.{key}'"
            )));
        }
    }

    Ok(())
}

fn parse_optional_string(raw: &toml::Value, key: &str) -> Result<Option<String>> {
    let Some(value) = raw.get(key) else {
        return Ok(None);
    };
    value
        .as_str()
        .map(|s| Some(s.to_string()))
        .ok_or_else(|| ConfigError::Validation(format!("'{}' must be a string", key)))
}

fn parse_runtime_spec(raw: &toml::Value) -> Result<(Option<String>, Option<String>)> {
    let Some(runtime) = parse_optional_string(raw, "runtime")? else {
        return Ok((None, None));
    };
    let trimmed = runtime.trim();
    if trimmed.is_empty() {
        return Ok((Some(String::new()), None));
    }

    let Some((id, version)) = trimmed.split_once('@') else {
        return Ok((Some(trimmed.to_string()), None));
    };
    let id = id.trim();
    let version = version.trim();
    if version.is_empty() {
        return Err(ConfigError::Validation(
            "runtime version cannot be empty".to_string(),
        ));
    }
    Ok((Some(id.to_string()), Some(version.to_string())))
}

fn parse_build_stages(raw: &toml::Value) -> Result<Vec<BuildStage>> {
    let Some(value) = raw.get("build_stages") else {
        return Ok(Vec::new());
    };
    let Some(stages) = value.as_array() else {
        return Err(ConfigError::Validation(
            "'build_stages' must be an array of tables ([[build_stages]])".to_string(),
        ));
    };

    let mut parsed = Vec::with_capacity(stages.len());
    for (index, stage_value) in stages.iter().enumerate() {
        let Some(stage_table) = stage_value.as_table() else {
            return Err(ConfigError::Validation(format!(
                "'build_stages[{index}]' must be a table"
            )));
        };

        for key in stage_table.keys() {
            if !matches!(key.as_str(), "name" | "cwd" | "install" | "run" | "exclude") {
                return Err(ConfigError::Validation(format!(
                    "Unknown key 'build_stages[{index}].{key}'"
                )));
            }
        }

        let name = parse_build_stage_optional_string(stage_table, index, "name")?;
        let cwd = parse_build_stage_optional_string(stage_table, index, "cwd")?;
        let install = parse_build_stage_optional_string(stage_table, index, "install")?;
        let run = parse_build_stage_required_string(stage_table, index, "run")?;
        let exclude =
            parse_build_stage_string_array(stage_table, index, "exclude")?.unwrap_or_default();

        parsed.push(BuildStage {
            name,
            cwd,
            install,
            run,
            exclude,
        });
    }

    Ok(parsed)
}

fn parse_build_stage_optional_string(
    stage_table: &toml::value::Table,
    index: usize,
    key: &str,
) -> Result<Option<String>> {
    let Some(value) = stage_table.get(key) else {
        return Ok(None);
    };
    let Some(value) = value.as_str() else {
        return Err(ConfigError::Validation(format!(
            "'build_stages[{index}].{key}' must be a string"
        )));
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(ConfigError::Validation(format!(
            "'build_stages[{index}].{key}' cannot be empty"
        )));
    }
    Ok(Some(trimmed.to_string()))
}

fn parse_build_stage_required_string(
    stage_table: &toml::value::Table,
    index: usize,
    key: &str,
) -> Result<String> {
    let Some(value) = stage_table.get(key) else {
        return Err(ConfigError::Validation(format!(
            "'build_stages[{index}].{key}' is required"
        )));
    };
    let Some(value) = value.as_str() else {
        return Err(ConfigError::Validation(format!(
            "'build_stages[{index}].{key}' must be a string"
        )));
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(ConfigError::Validation(format!(
            "'build_stages[{index}].{key}' cannot be empty"
        )));
    }
    Ok(trimmed.to_string())
}

fn parse_build_stage_string_array(
    stage_table: &toml::value::Table,
    index: usize,
    key: &str,
) -> Result<Option<Vec<String>>> {
    let Some(value) = stage_table.get(key) else {
        return Ok(None);
    };
    let Some(arr) = value.as_array() else {
        return Err(ConfigError::Validation(format!(
            "'build_stages[{index}].{key}' must be an array of strings"
        )));
    };
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let Some(s) = item.as_str() else {
            return Err(ConfigError::Validation(format!(
                "'build_stages[{index}].{key}' must be an array of strings"
            )));
        };
        out.push(s.to_string());
    }
    Ok(Some(out))
}

fn parse_string_array(raw: &toml::Value, key: &str) -> Result<Option<Vec<String>>> {
    let Some(value) = raw.get(key) else {
        return Ok(None);
    };
    let arr = value
        .as_array()
        .ok_or_else(|| ConfigError::Validation(format!("'{}' must be an array of strings", key)))?;
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let Some(s) = item.as_str() else {
            return Err(ConfigError::Validation(format!(
                "'{}' must be an array of strings",
                key
            )));
        };
        out.push(s.to_string());
    }
    Ok(Some(out))
}
