use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::process::Command;

use crate::build::{BuildAdapter, PresetGroup};
use crate::config::{SecretsStore, TakoToml};

const DEFAULT_RUN_ENV: &str = "development";
const LOCAL_RUN_BUILD: &str = "local";

pub fn run(
    env: Option<&str>,
    eval: Option<&str>,
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

    let runtime = resolve_run_build_adapter(&config, &context.project_dir);
    let mut child_env = build_command_env(&config, env, runtime);
    inject_run_data_dir(&context.project_dir, &mut child_env)?;
    child_env.insert(
        tako_core::bootstrap::TAKO_BOOTSTRAP_DATA_ENV.to_string(),
        tako_core::bootstrap::envelope_string("", &secrets, &storages),
    );

    let plugin_context = tako_runtime::PluginContext {
        project_dir: &context.project_dir,
        package_manager: config.package_manager.as_deref(),
    };
    let runtime_def = runtime.runtime_def_with_context(&plugin_context);
    let resolved_command = resolve_run_command(
        runtime,
        runtime_def.as_ref(),
        eval,
        &command,
        &context.project_dir,
    )?;
    let (program, args) = resolved_command
        .command
        .split_first()
        .expect("resolve_run_command returns a non-empty command");

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

fn build_command_env(
    config: &TakoToml,
    env: &str,
    runtime: BuildAdapter,
) -> HashMap<String, String> {
    let mut child_env = HashMap::new();
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

#[derive(Debug)]
struct ResolvedRunCommand {
    command: Vec<String>,
    _eval_file: Option<tempfile::NamedTempFile>,
}

fn resolve_run_command(
    runtime: BuildAdapter,
    runtime_def: Option<&tako_runtime::RuntimeDef>,
    eval: Option<&str>,
    command: &[String],
    project_dir: &Path,
) -> Result<ResolvedRunCommand, Box<dyn std::error::Error>> {
    if let Some(source) = eval {
        let runtime_def = runtime_def.ok_or_else(|| {
            format!(
                "Cannot infer how to run inline code because the project runtime is '{}'. Set top-level `runtime` or use a script file with an explicit command after `--`.",
                runtime.id()
            )
        })?;
        return resolve_eval_command(runtime, runtime_def, source, command, project_dir);
    }

    let Some((first, rest)) = command.split_first() else {
        return Err("Missing command to run.".into());
    };

    if let Some(runtime_def) = runtime_def
        && let Some(command) = resolve_local_script_command(runtime_def, first, rest)?
    {
        return Ok(ResolvedRunCommand {
            command,
            _eval_file: None,
        });
    }

    if has_known_local_script_extension(first) {
        return Err(format!(
            "Runtime '{}' does not define a local run rule for '{}'. Pass an explicit command after `--`.",
            runtime.id(),
            display_extension(first)
        )
        .into());
    }

    Ok(ResolvedRunCommand {
        command: command.to_vec(),
        _eval_file: None,
    })
}

fn resolve_eval_command(
    runtime: BuildAdapter,
    runtime_def: &tako_runtime::RuntimeDef,
    source: &str,
    args: &[String],
    project_dir: &Path,
) -> Result<ResolvedRunCommand, Box<dyn std::error::Error>> {
    if source.trim().is_empty() {
        return Err("`--eval` cannot be empty.".into());
    }

    let Some(eval) = &runtime_def.local_run.eval else {
        return Err(format!(
            "Runtime '{}' does not support `tako run --eval`. Use a script file or pass an explicit command after `--`.",
            runtime.id()
        )
        .into());
    };

    let temp_dir = project_dir.join(".tako").join("run");
    std::fs::create_dir_all(&temp_dir)?;
    let mut file = tempfile::Builder::new()
        .prefix("eval-")
        .suffix(&eval.temp_suffix)
        .tempfile_in(temp_dir)?;
    file.write_all(source.as_bytes())?;
    file.flush()?;

    let script = file.path().to_string_lossy().to_string();
    let command = apply_local_run_template(&eval.command, &script, args)?;

    Ok(ResolvedRunCommand {
        command,
        _eval_file: Some(file),
    })
}

fn resolve_local_script_command(
    runtime_def: &tako_runtime::RuntimeDef,
    script: &str,
    args: &[String],
) -> Result<Option<Vec<String>>, Box<dyn std::error::Error>> {
    let Some(extension) = script_extension(script) else {
        return Ok(None);
    };

    let Some(rule) = runtime_def.local_run.scripts.iter().find(|rule| {
        rule.extensions
            .iter()
            .any(|candidate| candidate == extension)
    }) else {
        return Ok(None);
    };

    Ok(Some(apply_local_run_template(&rule.command, script, args)?))
}

fn apply_local_run_template(
    template: &[String],
    script: &str,
    args: &[String],
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    if !template.iter().any(|part| part == "{script}") {
        return Err("Runtime local run command must include `{script}`.".into());
    }

    let mut command: Vec<String> = template
        .iter()
        .map(|part| {
            if part == "{script}" {
                script.to_string()
            } else {
                part.clone()
            }
        })
        .collect();
    command.extend(args.iter().cloned());
    Ok(command)
}

fn has_known_local_script_extension(value: &str) -> bool {
    let Some(extension) = script_extension(value) else {
        return false;
    };

    tako_runtime::KNOWN_RUNTIME_IDS.iter().any(|runtime_id| {
        tako_runtime::runtime_def_for(runtime_id, None).is_some_and(|def| {
            def.local_run.scripts.iter().any(|rule| {
                rule.extensions
                    .iter()
                    .any(|candidate| candidate == extension)
            })
        })
    })
}

fn display_extension(value: &str) -> String {
    script_extension(value)
        .map(|extension| format!(".{extension}"))
        .unwrap_or_else(|| value.to_string())
}

fn script_extension(value: &str) -> Option<&str> {
    Path::new(value).extension().and_then(|ext| ext.to_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TakoToml;

    fn runtime_def(adapter: BuildAdapter) -> tako_runtime::RuntimeDef {
        adapter.runtime_def().unwrap()
    }

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
        let env = build_command_env(&config, "preview", BuildAdapter::Bun);

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

        let env = build_command_env(&config, "development", BuildAdapter::Node);

        assert_eq!(env.get("ENV").map(String::as_str), Some("development"));
        assert_eq!(env.get("NODE_ENV").map(String::as_str), Some("development"));
        assert_eq!(env.get("TAKO_BUILD").map(String::as_str), Some("local"));
    }

    #[test]
    fn command_env_detects_runtime_when_config_omits_runtime() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::write(temp.path().join("bun.lockb"), "").unwrap();
        let config = TakoToml::parse("name = \"demo\"\n").unwrap();

        let runtime = resolve_run_build_adapter(&config, temp.path());
        let env = build_command_env(&config, "development", runtime);

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

    #[test]
    fn run_command_wraps_bare_bun_script_with_runtime() {
        let command = vec!["scripts/foo.ts".to_string(), "--dry".to_string()];
        let def = runtime_def(BuildAdapter::Bun);
        let temp = tempfile::TempDir::new().unwrap();

        let resolved =
            resolve_run_command(BuildAdapter::Bun, Some(&def), None, &command, temp.path())
                .unwrap();

        assert_eq!(resolved.command, vec!["bun", "scripts/foo.ts", "--dry"]);
    }

    #[test]
    fn run_command_wraps_bare_node_typescript_script_with_strip_types() {
        let command = vec!["scripts/foo.ts".to_string()];
        let def = runtime_def(BuildAdapter::Node);
        let temp = tempfile::TempDir::new().unwrap();

        let resolved =
            resolve_run_command(BuildAdapter::Node, Some(&def), None, &command, temp.path())
                .unwrap();

        assert_eq!(
            resolved.command,
            vec!["node", "--experimental-strip-types", "scripts/foo.ts"]
        );
    }

    #[test]
    fn run_command_wraps_bare_node_javascript_script_without_strip_types() {
        let command = vec!["scripts/foo.mjs".to_string()];
        let def = runtime_def(BuildAdapter::Node);
        let temp = tempfile::TempDir::new().unwrap();

        let resolved =
            resolve_run_command(BuildAdapter::Node, Some(&def), None, &command, temp.path())
                .unwrap();

        assert_eq!(resolved.command, vec!["node", "scripts/foo.mjs"]);
    }

    #[test]
    fn run_command_keeps_explicit_commands_unchanged() {
        let command = vec![
            "node".to_string(),
            "--experimental-strip-types".to_string(),
            "scripts/foo.ts".to_string(),
        ];
        let def = runtime_def(BuildAdapter::Bun);
        let temp = tempfile::TempDir::new().unwrap();

        let resolved =
            resolve_run_command(BuildAdapter::Bun, Some(&def), None, &command, temp.path())
                .unwrap();

        assert_eq!(resolved.command, command);
    }

    #[test]
    fn run_command_errors_for_bare_known_script_with_unknown_runtime() {
        let command = vec!["scripts/foo.ts".to_string()];
        let temp = tempfile::TempDir::new().unwrap();

        let error = resolve_run_command(BuildAdapter::Unknown, None, None, &command, temp.path())
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("does not define a local run rule")
        );
    }

    #[test]
    fn run_command_wraps_bare_go_script_with_go_run() {
        let command = vec!["scripts/foo.go".to_string(), "--dry".to_string()];
        let def = runtime_def(BuildAdapter::Go);
        let temp = tempfile::TempDir::new().unwrap();

        let resolved =
            resolve_run_command(BuildAdapter::Go, Some(&def), None, &command, temp.path()).unwrap();

        assert_eq!(
            resolved.command,
            vec!["go", "run", "scripts/foo.go", "--dry"]
        );
    }

    #[test]
    fn run_command_rejects_go_script_with_non_go_runtime() {
        let command = vec!["scripts/foo.go".to_string()];
        let def = runtime_def(BuildAdapter::Bun);
        let temp = tempfile::TempDir::new().unwrap();

        let error = resolve_run_command(BuildAdapter::Bun, Some(&def), None, &command, temp.path())
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("does not define a local run rule")
        );
    }

    #[test]
    fn run_command_keeps_unknown_extension_as_explicit_command() {
        let command = vec!["scripts/foo.sh".to_string()];
        let def = runtime_def(BuildAdapter::Bun);
        let temp = tempfile::TempDir::new().unwrap();

        let resolved =
            resolve_run_command(BuildAdapter::Bun, Some(&def), None, &command, temp.path())
                .unwrap();

        assert_eq!(resolved.command, command);
    }

    #[test]
    fn run_command_eval_uses_runtime_inline_rule() {
        let args = vec!["--dry".to_string()];
        let def = runtime_def(BuildAdapter::Node);
        let temp = tempfile::TempDir::new().unwrap();

        let resolved = resolve_run_command(
            BuildAdapter::Node,
            Some(&def),
            Some("console.log(tako.env);"),
            &args,
            temp.path(),
        )
        .unwrap();

        assert_eq!(resolved.command[0], "node");
        assert_eq!(resolved.command[1], "--experimental-strip-types");
        assert!(resolved.command[2].ends_with(".ts"));
        assert_eq!(resolved.command[3], "--dry");
        assert_eq!(
            std::fs::read_to_string(&resolved.command[2]).unwrap(),
            "console.log(tako.env);"
        );
    }

    #[test]
    fn run_command_eval_errors_when_runtime_does_not_support_inline_code() {
        let def = runtime_def(BuildAdapter::Go);
        let temp = tempfile::TempDir::new().unwrap();

        let error = resolve_run_command(
            BuildAdapter::Go,
            Some(&def),
            Some("package main"),
            &[],
            temp.path(),
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("does not support `tako run --eval`")
        );
    }
}
