mod artifacts;
mod cache;
mod config;
mod format;
mod manifest;
mod preflight;
mod remote;
mod task_tree;

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::app::require_app_name_from_config_path;
use crate::build::{PresetGroup, js};
use crate::commands::project_context;
use crate::config::{SecretsStore, ServerEntry, ServerTarget, ServersToml, TakoToml};
use crate::output;
use crate::ssh::SshClient;
use crate::validation::{validate_full_config, validate_secrets_for_deployment};
use tracing::Instrument;

use artifacts::prepare_build_phase;
use config::{
    confirm_production_deploy, required_env_routes, resolve_build_preset_ref,
    resolve_deploy_environment, resolve_deploy_server_names,
    resolve_deploy_server_names_with_setup, resolve_deploy_server_targets,
    resolve_effective_build_adapter, run_bun_lockfile_preflight, should_run_bun_lockfile_preflight,
};
use format::{
    format_build_plan_target_label, format_parallel_deploy_step, format_partial_failure_error,
    format_preflight_complete_message, format_server_deploy_failure, format_server_deploy_success,
    format_server_not_found_error, format_server_targets_summary, format_servers_summary,
    print_deploy_summary, should_use_per_server_spinners, should_use_unified_js_target_process,
};
use preflight::{check_wildcard_dns_support, run_server_preflight_checks};
use remote::deploy_to_server;
use task_tree::{
    DeployTaskTreeController, build_artifact_target_groups, should_use_deploy_task_tree,
};

pub(crate) use manifest::resolve_deploy_main;

/// Deployment configuration
#[derive(Clone)]
struct DeployConfig {
    app_name: String,
    version: String,
    remote_base: String,
    routes: Vec<String>,
    secrets: HashMap<String, String>,
    /// SHA-256 hash of the decrypted secrets for this deploy.
    secrets_hash: String,
    main: String,
    use_unified_target_process: bool,

    /// Resolved release command (None when no release step). Sent only
    /// to the leader server; followers wait on the result.
    release_command: Option<String>,

    /// Server name that runs the release command. Always
    /// `target_servers.first()` — kept here so per-server code can
    /// compare without re-deriving.
    leader_server: String,
}

#[derive(Clone)]
struct ServerDeployTarget {
    name: String,
    server: ServerEntry,
    target_label: String,
    archive_path: PathBuf,
}

struct ServerCheck {
    name: String,
    mode: tako_core::UpgradeMode,
    dns_provider: Option<String>,
}

struct PreflightPhaseResult {
    checks: Vec<ServerCheck>,
    /// Pre-established SSH connections, keyed by server name.
    /// Kept alive from preflight so deploy can reuse them without reconnecting.
    ssh_clients: HashMap<String, SshClient>,
    elapsed: Duration,
}

struct BuildPhaseResult {
    version: String,
    manifest_main: String,
    deploy_secrets: HashMap<String, String>,
    use_unified_target_process: bool,
    artifacts_by_target: HashMap<String, PathBuf>,
}

struct ValidationResult {
    tako_config: TakoToml,
    servers: ServersToml,
    secrets: SecretsStore,
    env: String,
    warnings: Vec<String>,
}

#[derive(Debug)]
struct ProjectDeployLock {
    _file: File,
    path: PathBuf,
}

impl Drop for ProjectDeployLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Resolve the effective release command for an environment.
///
/// Precedence:
/// 1. `[envs.<env>].release` if set (including empty string, which clears).
/// 2. Top-level `release` if set.
/// 3. None.
///
/// An empty string (top-level or per-env) yields `None`.
pub(super) fn resolve_release_command(config: &TakoToml, env_name: &str) -> Option<String> {
    let candidate = config
        .envs
        .get(env_name)
        .and_then(|e| e.release.clone())
        .or_else(|| config.release.clone());
    candidate.and_then(|s| {
        let trimmed = s.trim();
        if trimmed.is_empty() { None } else { Some(s) }
    })
}

impl DeployConfig {
    fn release_dir(&self) -> String {
        format!("{}/releases/{}", self.remote_base, self.version)
    }

    fn current_link(&self) -> String {
        format!("{}/current", self.remote_base)
    }

    fn shared_dir(&self) -> String {
        format!("{}/shared", self.remote_base)
    }

    fn release_command_payload(&self, release_dir: &str) -> Option<tako_core::Command> {
        let command_line = self.release_command.as_ref()?;
        Some(tako_core::Command::RunRelease {
            app: self.app_name.clone(),
            version: self.version.clone(),
            path: release_dir.to_string(),
            command_line: command_line.clone(),
            vars: HashMap::new(),
            secrets: self.secrets.clone(),
        })
    }
}

pub fn run(
    env: Option<&str>,
    assume_yes: bool,
    config_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Use tokio runtime for async SSH operations
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run_async(env, assume_yes, config_path))
}

async fn run_async(
    requested_env: Option<&str>,
    assume_yes: bool,
    config_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let context = project_context::resolve_existing(config_path)?;
    let project_dir = context.project_dir;
    let validation = output::with_spinner_silent(
        "Validating configuration",
        || -> Result<ValidationResult, String> {
            let _t = output::timed("Configuration validation");
            let tako_config =
                TakoToml::load_from_file(&context.config_path).map_err(|e| e.to_string())?;
            let servers = ServersToml::load().map_err(|e| e.to_string())?;
            let secrets = SecretsStore::load_from_dir(&project_dir).map_err(|e| e.to_string())?;

            let env = resolve_deploy_environment(requested_env, &tako_config)?;

            let config_result = validate_full_config(&tako_config, &servers, Some(&env));
            if config_result.has_errors() {
                return Err(format!(
                    "Configuration errors:\n  {}",
                    config_result.errors.join("\n  ")
                ));
            }
            let mut warnings = config_result.warnings.clone();

            let secrets_result = validate_secrets_for_deployment(&secrets, &env);
            if secrets_result.has_errors() {
                return Err(format!(
                    "Secret errors:\n  {}",
                    secrets_result.errors.join("\n  ")
                ));
            }
            warnings.extend(secrets_result.warnings.clone());

            Ok(ValidationResult {
                tako_config,
                servers,
                secrets,
                env,
                warnings,
            })
        },
    )
    .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    let ValidationResult {
        tako_config,
        mut servers,
        secrets,
        env,
        warnings,
    } = validation;

    let eff_app_dir = project_dir.clone();

    let preflight_preset_ref = resolve_build_preset_ref(&eff_app_dir, &tako_config)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    let preflight_runtime_adapter =
        resolve_effective_build_adapter(&eff_app_dir, &tako_config, &preflight_preset_ref)
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    let source_root = source_bundle_root(&project_dir, preflight_runtime_adapter.id());

    if preflight_runtime_adapter.preset_group() == PresetGroup::Js {
        let _ = js::write_tako_declarations_for_adapter_and_app_root(
            &project_dir,
            preflight_runtime_adapter,
            tako_config.js_app_root(),
        );
    }

    let _bun_lockfile_checked = if should_run_bun_lockfile_preflight(preflight_runtime_adapter) {
        output::with_spinner_silent("Checking Bun lockfile", || {
            let _t = output::timed("Bun lockfile check");
            run_bun_lockfile_preflight(&source_root)
        })
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?
    } else {
        false
    };

    // Skip confirmation if the user explicitly passed --env production (they
    // already know which environment they're targeting).
    let env_was_explicit = requested_env.is_some();
    confirm_production_deploy(&env, assume_yes || env_was_explicit || output::is_dry_run())
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    for warning in &warnings {
        output::warning(&format!("Validation: {}", warning));
    }

    let app_name = require_app_name_from_config_path(&context.config_path).map_err(
        |e| -> Box<dyn std::error::Error> {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()).into()
        },
    )?;
    let routes = required_env_routes(&tako_config, &env)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    let server_names = if output::is_dry_run() {
        resolve_deploy_server_names(&tako_config, &servers, &env)
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?
    } else {
        resolve_deploy_server_names_with_setup(
            &tako_config,
            &mut servers,
            &env,
            &context.config_path,
        )
        .await
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?
    };

    for server_name in &server_names {
        if !servers.contains(server_name) {
            return Err(format_server_not_found_error(server_name).into());
        }
    }

    let server_targets = resolve_deploy_server_targets(&servers, &server_names)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    tracing::debug!("{}", format_servers_summary(&server_names));

    let use_unified_js_target_process =
        should_use_unified_js_target_process(preflight_runtime_adapter.id());
    if let Some(server_targets_summary) =
        format_server_targets_summary(&server_targets, use_unified_js_target_process)
    {
        tracing::debug!("{}", server_targets_summary);
    }
    let build_groups = build_artifact_target_groups(&server_targets, use_unified_js_target_process);

    // Check the secrets key now, before starting the task tree, so missing-key
    // errors are shown against a clean terminal instead of being overpainted by
    // the live viewport.
    if !output::is_dry_run() {
        crate::commands::secret::ensure_secret_key_available(&env, &secrets, Some(&project_dir))?;
    }

    let deploy_task_tree = should_use_deploy_task_tree()
        .then(|| DeployTaskTreeController::new(&server_names, &build_groups));

    if let (Some(task_tree), Some(first_build_group)) = (&deploy_task_tree, build_groups.first()) {
        task_tree.mark_build_target_running(&format_build_plan_target_label(first_build_group));
    }

    if output::is_dry_run() {
        output::dry_run_skip("Server checks");
        output::dry_run_skip("Build");
        for name in &server_names {
            output::dry_run_skip(&format!("Deploy to {}", output::strong(name)));
        }
        if output::is_pretty() {
            eprintln!();
        }
        print_deploy_summary("App", &app_name, &routes);
        return Ok(());
    }

    let _deploy_lock = acquire_project_deploy_lock(&project_dir)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    let use_per_server_spinners = deploy_task_tree.is_none()
        && should_use_per_server_spinners(server_names.len(), output::is_interactive());

    let preflight_deploy_app_name = tako_core::deployment_app_id(&app_name, &env);
    let mut preflight_handle = tokio::spawn(run_server_preflight_checks(
        server_names.clone(),
        servers.clone(),
        preflight_deploy_app_name,
        routes.clone(),
        deploy_task_tree.clone(),
    ));
    let mut build_handle = tokio::spawn(prepare_build_phase(
        project_dir.clone(),
        source_root.clone(),
        eff_app_dir.clone(),
        app_name.clone(),
        env.clone(),
        tako_config.clone(),
        secrets.clone(),
        preflight_preset_ref.clone(),
        preflight_runtime_adapter,
        server_targets.clone(),
        build_groups.clone(),
        deploy_task_tree.clone(),
    ));

    let mut preflight_result: Option<Result<PreflightPhaseResult, String>> = None;
    let mut build_result: Option<BuildPhaseResult> = None;

    while preflight_result.is_none() || build_result.is_none() {
        tokio::select! {
            result = &mut preflight_handle, if preflight_result.is_none() => {
                let result = result
                    .map_err(|e| format!("Server checks task failed: {}", e))
                    .and_then(|result| result);
                match result {
                    Ok(preflight) => {
                        if deploy_task_tree.is_none() {
                            output::success_with_elapsed(
                                &format_preflight_complete_message(&server_names),
                                preflight.elapsed,
                            );
                        }
                        preflight_result = Some(Ok(preflight));
                    }
                    Err(error) => {
                        if let Some(task_tree) = &deploy_task_tree {
                            task_tree.abort_incomplete("Aborted");
                            if build_result.is_none() {
                                build_handle.abort();
                            }
                            return Err(output::silent_exit_error().into());
                        }
                        if build_result.is_none() {
                            build_handle.abort();
                        }
                        return Err(error.into());
                    }
                }
            }
            result = &mut build_handle, if build_result.is_none() => {
                let result = result
                    .map_err(|e| format!("Build task failed: {}", e))
                    .and_then(|result| result);
                match result {
                    Ok(build) => build_result = Some(build),
                    Err(error) => {
                        if let Some(task_tree) = &deploy_task_tree {
                            task_tree.abort_incomplete("Aborted");
                            if preflight_result.is_none() {
                                preflight_handle.abort();
                            }
                            return Err(output::silent_exit_error().into());
                        }
                        if preflight_result.is_none() {
                            preflight_handle.abort();
                        }
                        return Err(error.into());
                    }
                }
            }
        }
    }

    let preflight = preflight_result
        .unwrap()
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    let mut preflight_ssh_clients = preflight.ssh_clients;
    check_wildcard_dns_support(&routes, &preflight.checks)?;

    let BuildPhaseResult {
        version,
        manifest_main,
        deploy_secrets,
        use_unified_target_process: use_unified_js_target_process,
        artifacts_by_target,
    } = build_result.expect("build result should be present");

    // ===== Deploy =====

    let secrets_hash = tako_core::compute_secrets_hash(&deploy_secrets);
    let deployment_app_name = tako_core::deployment_app_id(&app_name, &env);
    let leader_server = server_names
        .first()
        .cloned()
        .ok_or("no servers selected for deploy")?;
    let release_command = resolve_release_command(&tako_config, &env);
    let deploy_config = Arc::new(DeployConfig {
        app_name: deployment_app_name.clone(),
        version: version.clone(),
        remote_base: format!("/opt/tako/apps/{}", deployment_app_name),
        routes: routes.clone(),
        secrets: deploy_secrets,
        secrets_hash,
        main: manifest_main,
        use_unified_target_process: use_unified_js_target_process,
        release_command,
        leader_server,
    });
    let target_by_server: HashMap<String, ServerTarget> = server_targets.into_iter().collect();

    // Build per-server deploy targets (includes per-server scaling settings)
    let mut targets = Vec::new();
    for server_name in &server_names {
        let server = servers.get(server_name).unwrap().clone();
        let target = target_by_server.get(server_name).ok_or_else(|| {
            format!(
                "Missing resolved target metadata for server '{}'",
                server_name
            )
        })?;
        let target_label = target.label();
        let archive_path = artifacts_by_target.get(&target_label).ok_or_else(|| {
            format!(
                "Missing build artifact for server target '{}'; expected artifact for {}",
                target_label, server_name
            )
        })?;
        targets.push(ServerDeployTarget {
            name: server_name.clone(),
            server,
            target_label,
            archive_path: archive_path.clone(),
        });
    }
    if deploy_task_tree.is_none() && targets.len() > 1 {
        output::info(&format_parallel_deploy_step(targets.len()));
    }

    // Watch channel: leader publishes the release-command result; followers
    // observe it before proceeding into Starting. Sender is not Clone, so
    // it moves into the leader's spawned task via `take()`. Receiver is
    // Clone — followers each get their own clone.
    type ReleaseSignal = Option<Result<(), String>>;
    let (mut release_tx, release_rx_template) = if deploy_config.release_command.is_some() {
        let (tx, rx) = tokio::sync::watch::channel::<ReleaseSignal>(None);
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };
    let leader_server_name = deploy_config.leader_server.clone();

    // Spawn parallel deploy tasks
    let mut handles = Vec::new();
    for target in &targets {
        let server = target.server.clone();
        let server_name = target.name.clone();
        let target_label = target.target_label.clone();
        let archive_path = target.archive_path.clone();
        let deploy_config = deploy_config.clone();
        let use_spinner = use_per_server_spinners;
        let task_tree = deploy_task_tree.clone();
        let preconnected_ssh = preflight_ssh_clients.remove(&server_name);

        let is_leader = target.name == leader_server_name;
        let release_tx_for_task = if is_leader { release_tx.take() } else { None };
        let release_rx_for_task = if !is_leader {
            release_rx_template.clone()
        } else {
            None
        };

        let span = output::scope(&server_name);
        let handle = tokio::spawn(
            async move {
                let result = deploy_to_server(
                    &deploy_config,
                    &server_name,
                    &server,
                    &archive_path,
                    &target_label,
                    use_spinner,
                    task_tree,
                    preconnected_ssh,
                    release_tx_for_task,
                    release_rx_for_task,
                )
                .await;
                (server_name, server, result)
            }
            .instrument(span),
        );
        handles.push(handle);
    }

    // Collect results
    let mut errors = Vec::new();

    let deploy_results = if deploy_task_tree.is_none()
        && output::is_interactive()
        && !use_per_server_spinners
        && handles.len() > 1
    {
        output::with_spinner_async_simple(&format_parallel_deploy_step(handles.len()), async {
            let mut results = Vec::new();
            for handle in handles {
                results.push(handle.await);
            }
            results
        })
        .await
    } else {
        let mut results = Vec::new();
        for handle in handles {
            results.push(handle.await);
        }
        results
    };

    for result in deploy_results {
        match result {
            Ok((server_name, server, result)) => match result {
                Ok(()) => {
                    tracing::debug!("{server_name} deploy succeeded");
                    if deploy_task_tree.is_none() {
                        output::bullet(&format_server_deploy_success(&server_name, &server));
                    }
                }
                Err(e) => {
                    // When using per-server spinners (single interactive server), the step
                    // spinner already printed the error detail. Skip the duplicate.
                    if deploy_task_tree.is_none() && !use_per_server_spinners {
                        output::error(&format_server_deploy_failure(
                            &server_name,
                            &server,
                            &e.to_string(),
                        ));
                    }
                    errors.push(format!("{}: {}", server_name, e));
                }
            },
            Err(e) => {
                // Task panicked
                errors.push(format!("Task panic: {}", e));
            }
        }
    }

    // ===== Summary =====
    if errors.is_empty() {
        if let Some(task_tree) = &deploy_task_tree {
            task_tree.set_success_summary(&version, &routes);
            task_tree.finalize();
        } else {
            if output::is_pretty() {
                eprintln!();
            }
            print_deploy_summary("Release", &version, &routes);
        }

        Ok(())
    } else {
        let succeeded = targets.len() - errors.len();
        let total = targets.len();
        if output::is_pretty() {
            if let Some(task_tree) = &deploy_task_tree {
                task_tree.set_error_summary(format!("Deployed to {succeeded}/{total} servers"));
                task_tree.finalize();
            } else {
                eprintln!(
                    "{}",
                    output::theme_error(format!("{succeeded}/{total} servers deployed"))
                );
            }
            return Err(output::silent_exit_error().into());
        }
        Err(format_partial_failure_error(errors.len()).into())
    }
}

fn git_repo_root(project_dir: &Path) -> Option<PathBuf> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(project_dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() {
        return None;
    }
    Some(PathBuf::from(value))
}

fn source_bundle_root(project_dir: &Path, runtime_id: &str) -> PathBuf {
    match git_repo_root(project_dir) {
        Some(root) if project_dir.starts_with(&root) => root,
        _ => tako_runtime::find_runtime_project_root(runtime_id, project_dir),
    }
}

fn deploy_lock_path(project_dir: &Path) -> PathBuf {
    project_dir.join(".tako/deploy.lock")
}

fn acquire_project_deploy_lock(project_dir: &Path) -> Result<ProjectDeployLock, String> {
    let path = deploy_lock_path(project_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create {}: {e}", parent.display()))?;
    }

    let mut file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| format!("Failed to open {}: {e}", path.display()))?;

    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        write_deploy_lock_pid(&mut file)
            .map_err(|e| format!("Failed to write {}: {e}", path.display()))?;
        return Ok(ProjectDeployLock { _file: file, path });
    }

    let err = std::io::Error::last_os_error();
    if err.raw_os_error() != Some(libc::EWOULDBLOCK) {
        return Err(format!("Failed to lock {}: {err}", path.display()));
    }

    let owner_pid = read_deploy_lock_pid(&mut file);
    match owner_pid {
        Some(pid) => Err(format!(
            "Another deploy is already running for this project (PID {pid}). Wait for it to finish and try again."
        )),
        None => Err(
            "Another deploy is already running for this project. Wait for it to finish and try again."
                .to_string(),
        ),
    }
}

fn write_deploy_lock_pid(file: &mut File) -> std::io::Result<()> {
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    write!(file, "{}", std::process::id())?;
    file.sync_all()?;
    Ok(())
}

fn read_deploy_lock_pid(file: &mut File) -> Option<u32> {
    file.seek(SeekFrom::Start(0)).ok()?;
    let mut raw = String::new();
    file.read_to_string(&mut raw).ok()?;
    raw.trim().parse::<u32>().ok()
}

#[cfg(test)]
mod release_resolution_tests;

#[cfg(test)]
mod tests;
