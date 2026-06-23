use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use crate::build::{BuildAdapter, PresetGroup};
use crate::config::{SecretsStore, TakoToml};

const DEFAULT_RUN_ENV: &str = "development";
const LOCAL_RUN_BUILD: &str = "local";

pub fn run(
    env: Option<&str>,
    secrets_as_env: bool,
    command: Vec<String>,
    config_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let env = env.unwrap_or(DEFAULT_RUN_ENV);
    crate::config::validate_environment_name(env)?;
    let context = crate::commands::project_context::resolve_existing(config_path)?;
    let config = TakoToml::load_from_file(&context.config_path)?;
    let secrets_store = SecretsStore::load_from_dir(&context.project_dir)?;
    let secrets = decrypt_run_secrets(env, &secrets_store, Some(&context.project_dir))?;
    let storages = crate::commands::storage::decrypt_storage_bindings(
        env,
        &config,
        &secrets_store,
        Some(&context.project_dir),
    )?;

    let mut child_env =
        build_child_env(&config, &context.project_dir, env, &secrets, secrets_as_env);
    inject_run_data_dir(&context.project_dir, &mut child_env)?;
    child_env.insert(
        tako_core::bootstrap::TAKO_BOOTSTRAP_DATA_ENV.to_string(),
        tako_core::bootstrap::envelope_string("", &secrets, &storages),
    );

    let Some((program, args)) = command.split_first() else {
        return Err("Missing command to run.".into());
    };

    let status = Command::new(program)
        .args(args)
        .current_dir(&context.project_dir)
        .envs(child_env)
        .status()?;

    if status.success() {
        return Ok(());
    }

    crate::output::restore_cursor();
    std::process::exit(status.code().unwrap_or(1));
}

fn decrypt_run_secrets(
    env: &str,
    secrets: &SecretsStore,
    usage_path: Option<&Path>,
) -> Result<HashMap<String, String>, Box<dyn std::error::Error>> {
    let encrypted = match secrets.get_env(env) {
        Some(entries) if !entries.is_empty() => entries,
        _ => return Ok(HashMap::new()),
    };

    let key = crate::commands::secret::load_secret_key(env, secrets, usage_path)?;
    let mut decrypted = HashMap::new();
    for (name, encrypted_value) in encrypted {
        let value = crate::crypto::decrypt(&encrypted_value.value, &key)
            .map_err(|e| format!("Failed to decrypt secret '{name}': {e}"))?;
        decrypted.insert(name.clone(), value);
    }
    Ok(decrypted)
}

fn build_child_env(
    config: &TakoToml,
    project_dir: &Path,
    env: &str,
    secrets: &HashMap<String, String>,
    secrets_as_env: bool,
) -> HashMap<String, String> {
    let mut child_env = build_command_env(config, project_dir, env);
    if secrets_as_env {
        child_env.extend(secrets.clone());
    }
    child_env
}

fn build_command_env(config: &TakoToml, project_dir: &Path, env: &str) -> HashMap<String, String> {
    let mut child_env = HashMap::new();
    let runtime = resolve_run_build_adapter(config, project_dir);
    let runtime_env_name = runtime_env_name(env);

    if let Some(def) = tako_runtime::runtime_def_for(runtime.id(), None)
        && let Some(defaults) = def.envs.environments.get(runtime_env_name)
    {
        child_env.extend(defaults.clone());
    }

    child_env.extend(config.get_merged_vars(env));

    if runtime.preset_group() == PresetGroup::Js {
        child_env.insert(
            "TAKO_APP_ROOT".to_string(),
            config.js_app_root().to_string(),
        );
    }

    child_env.insert("ENV".to_string(), env.to_string());
    child_env.insert("TAKO_BUILD".to_string(), LOCAL_RUN_BUILD.to_string());
    child_env
}

fn inject_run_data_dir(
    project_dir: &Path,
    env: &mut HashMap<String, String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let data_dir = project_dir.join(".tako").join("data").join("app");
    std::fs::create_dir_all(&data_dir)?;
    env.insert(
        "TAKO_DATA_DIR".to_string(),
        data_dir.to_string_lossy().to_string(),
    );
    Ok(())
}

fn runtime_env_name(env: &str) -> &str {
    if env == "development" {
        "development"
    } else {
        "production"
    }
}

fn resolve_run_build_adapter(config: &TakoToml, project_dir: &Path) -> BuildAdapter {
    config
        .runtime
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(BuildAdapter::from_id)
        .unwrap_or_else(|| crate::build::detect_build_adapter(project_dir))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TakoToml;

    #[test]
    fn command_env_merges_vars_runtime_defaults_and_derived_values() {
        let config = TakoToml::parse(
            r#"
runtime = "bun"

[vars]
API_URL = "https://api.example.com"
ENV = "ignored"

[vars.preview]
API_URL = "https://preview.example.com"
"#,
        )
        .unwrap();
        let temp = tempfile::TempDir::new().unwrap();

        let env = build_command_env(&config, temp.path(), "preview");

        assert_eq!(
            env.get("API_URL").map(String::as_str),
            Some("https://preview.example.com")
        );
        assert_eq!(env.get("ENV").map(String::as_str), Some("preview"));
        assert_eq!(env.get("NODE_ENV").map(String::as_str), Some("production"));
        assert_eq!(env.get("BUN_ENV").map(String::as_str), Some("production"));
        assert_eq!(env.get("TAKO_APP_ROOT").map(String::as_str), Some("src"));
    }

    #[test]
    fn command_env_uses_development_runtime_defaults_for_development() {
        let config = TakoToml::parse("runtime = \"node\"\n").unwrap();
        let temp = tempfile::TempDir::new().unwrap();

        let env = build_command_env(&config, temp.path(), "development");

        assert_eq!(env.get("ENV").map(String::as_str), Some("development"));
        assert_eq!(env.get("NODE_ENV").map(String::as_str), Some("development"));
        assert_eq!(env.get("TAKO_BUILD").map(String::as_str), Some("local"));
    }

    #[test]
    fn command_env_can_expose_secrets_as_env_when_requested() {
        let config = TakoToml::parse("name = \"demo\"\n").unwrap();
        let temp = tempfile::TempDir::new().unwrap();
        let secrets = HashMap::from([("DATABASE_URL".to_string(), "postgres://db".to_string())]);

        let env = build_child_env(&config, temp.path(), "production", &secrets, true);

        assert_eq!(
            env.get("DATABASE_URL").map(String::as_str),
            Some("postgres://db")
        );
    }

    #[test]
    fn command_env_detects_runtime_when_config_omits_runtime() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::write(temp.path().join("bun.lockb"), "").unwrap();
        let config = TakoToml::parse("name = \"demo\"\n").unwrap();

        let env = build_command_env(&config, temp.path(), "development");

        assert_eq!(env.get("NODE_ENV").map(String::as_str), Some("development"));
        assert_eq!(env.get("BUN_ENV").map(String::as_str), Some("development"));
    }

    #[test]
    fn inject_run_data_dir_creates_local_app_data_dir() {
        let temp = tempfile::TempDir::new().unwrap();
        let mut env = HashMap::new();

        inject_run_data_dir(temp.path(), &mut env).unwrap();

        let data_dir = temp.path().join(".tako").join("data").join("app");
        assert_eq!(
            env.get("TAKO_DATA_DIR").map(String::as_str),
            Some(data_dir.to_str().unwrap())
        );
        assert!(data_dir.is_dir());
    }
}
