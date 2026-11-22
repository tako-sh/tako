mod artifacts;
mod cache;
mod config;
mod databases;
mod format;
mod manifest;
mod preflight;
mod project;
mod remote;
mod task_tree;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::app::require_app_name_from_config_path;
use crate::build::{BuildAdapter, PresetGroup, js};
use crate::commands::project_context;
use crate::config::{SecretsStore, ServerEntry, ServerInventory, ServerTarget, TakoToml};
use crate::output;
use crate::validation::{
    validate_full_config, validate_secrets_for_deployment, validate_ssl_for_deployment,
    validate_storages_for_deployment,
};
use tracing::Instrument;

use artifacts::prepare_build_phase;
use config::{
    confirm_production_deploy, has_multiple_deploy_targets, required_env_routes,
    resolve_build_preset_ref, resolve_deploy_environment, resolve_deploy_server_names,
    resolve_deploy_server_names_with_setup, resolve_deploy_server_targets,
    resolve_effective_build_adapter, run_bun_lockfile_preflight, should_run_bun_lockfile_preflight,
    validate_runtime_state_storage_for_deploy,
};
use format::{
    common_https_port_for_servers, format_build_plan_target_label, format_parallel_deploy_step,
    format_partial_failure_error, format_preflight_complete_message, format_server_deploy_failure,
    format_server_deploy_success, format_server_not_found_error, format_server_targets_summary,
    format_servers_summary, print_deploy_summary_with_https_port, should_use_per_server_spinners,
    should_use_unified_js_target_process,
};
use preflight::run_server_preflight_checks;
#[cfg(test)]
use project::deploy_lock_path;
use project::{acquire_project_deploy_lock, source_bundle_root};
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
    routes: Vec<String>,
    source_ip: tako_core::SourceIpMode,
    /// The app environment's secret snapshot, pinned by the leader server.
    /// Servers resolve secret values from it; the client never sends them.
    secrets_digest: Option<String>,
    /// S3 bindings name their resource and carry no access keys.
    storages: HashMap<String, tako_core::StorageBinding>,
    ssl: tako_core::SslBinding,
    backup: Option<tako_core::BackupBinding>,
    /// Database bindings, sent with the release command and the deploy.
    databases: HashMap<String, tako_core::DatabaseBinding>,
    force: bool,
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
}

struct PreflightPhaseResult {
    elapsed: Duration,
}

struct BuildPhaseResult {
    version: String,
    manifest_main: String,
    deploy_storages: HashMap<String, tako_core::StorageBinding>,
    deploy_ssl: tako_core::SslBinding,
    deploy_backup: Option<tako_core::BackupBinding>,
    use_unified_target_process: bool,
    artifacts_by_target: HashMap<String, PathBuf>,
}

struct ValidationResult {
    tako_config: TakoToml,
    secrets: SecretsStore,
    env: String,
    warnings: Vec<String>,
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
    fn release_command_payload(&self, release_dir: &str) -> Option<tako_core::Command> {
        let command_line = self.release_command.as_ref()?;
        Some(tako_core::Command::RunRelease {
            app: self.app_name.clone(),
            version: self.version.clone(),
            path: release_dir.to_string(),
            command_line: command_line.clone(),
            vars: HashMap::new(),
            secrets_digest: self.secrets_digest.clone(),
            databases: self.databases.clone(),
            force: self.force,
        })
    }
}

pub fn run(
    env: Option<&str>,
    assume_yes: bool,
    force: bool,
    description: Option<&str>,
    config_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run_async(env, assume_yes, force, description, config_path))
}

async fn run_async(
    requested_env: Option<&str>,
    assume_yes: bool,
    force: bool,
    description: Option<&str>,
    config_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let context = project_context::resolve_existing(config_path)?;
    let project_dir = context.project_dir;
    // Choosing the network can ask, so it happens before any spinner.
    let env = resolve_deploy_environment(
        requested_env,
        &TakoToml::load_from_file(&context.config_path)?,
    )?;
    let network = super::env_network::resolve(
        &context.config_path,
        &env,
        super::env_network::Purpose::Deploy,
    )?;
    let validation = output::with_spinner_silent(
        "Validating configuration",
        || -> Result<ValidationResult, String> {
            let _t = output::timed("Configuration validation");
            let tako_config =
                TakoToml::load_from_file(&context.config_path).map_err(|e| e.to_string())?;
            let servers = ServerInventory::load(Some(&network.id)).map_err(|e| e.to_string())?;
            // Deploy clients never read local secrets or keys: servers resolve
            // values from the network at deploy.
            let full = super::helpers::can_configure_bindings(&env).map_err(|e| e.to_string())?;
            let secrets = if full {
                SecretsStore::load_for_environment(&project_dir, &tako_config, &env)
                    .map_err(|e| e.to_string())?
            } else {
                SecretsStore::default()
            };

            let config_result = validate_full_config(&tako_config, &servers, Some(&env));
            if config_result.has_errors() {
                return Err(format!(
                    "Configuration errors:\n  {}",
                    config_result.errors.join("\n  ")
                ));
            }
            let warnings = config_result.warnings.clone();

            Ok(ValidationResult {
                tako_config,
                secrets,
                env: env.clone(),
                warnings,
            })
        },
    )
    .map_err(|_| -> Box<dyn std::error::Error> { output::silent_exit_error().into() })?;

    let ValidationResult {
        tako_config,
        mut secrets,
        env,
        warnings,
    } = validation;
    let mut servers = super::helpers::load_management_servers(Some(vec![network.clone()])).await?;

    let eff_app_dir = project_dir.clone();

    let is_container_release = tako_config.container.is_some()
        || tako_config.image.is_some()
        || tako_config.compose.is_some();
    let (preflight_preset_ref, preflight_runtime_adapter) = if is_container_release {
        ("container".to_string(), BuildAdapter::Unknown)
    } else {
        let preset_ref = resolve_build_preset_ref(&eff_app_dir, &tako_config)
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
        let runtime_adapter =
            resolve_effective_build_adapter(&eff_app_dir, &tako_config, &preset_ref)
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
        (preset_ref, runtime_adapter)
    };

    let source_root = source_bundle_root(&project_dir, preflight_runtime_adapter.id());

    // Code generation and the lockfile check run project code. A client
    // identity's keys stay off disk while they do.
    if !output::is_dry_run() && preflight_runtime_adapter.preset_group() == PresetGroup::Js {
        crate::network::identity::without_keys(|| {
            js::write_generated_files_with_schemas(
                &project_dir,
                preflight_runtime_adapter,
                &tako_config,
                Some(&env),
                false,
            )
        })??;
    }

    if !output::is_dry_run() && should_run_bun_lockfile_preflight(preflight_runtime_adapter) {
        crate::network::identity::without_keys(|| {
            output::with_spinner_silent("Checking Bun lockfile", || {
                let _t = output::timed("Bun lockfile check");
                run_bun_lockfile_preflight(&source_root)
            })
        })?
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    }

    // Skip confirmation when the target is unambiguous: the user passed
    // --env production or --yes, or there is no other environment the
    // deploy could have implicitly missed.
    let skip_confirm = assume_yes
        || requested_env.is_some()
        || output::is_dry_run()
        || !has_multiple_deploy_targets(&tako_config);
    confirm_production_deploy(&env, skip_confirm)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    for warning in &warnings {
        output::warning(&format!("Validation: {}", warning));
    }

    let app_name = require_app_name_from_config_path(&context.config_path).map_err(
        |e| -> Box<dyn std::error::Error> {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()).into()
        },
    )?;
    let _deploy_lock = if output::is_dry_run() {
        None
    } else {
        Some(
            acquire_project_deploy_lock(&project_dir)
                .map_err(|error| -> Box<dyn std::error::Error> { error.into() })?,
        )
    };
    let configure_bindings = super::helpers::can_configure_bindings(&env)?;
    if configure_bindings && !output::is_dry_run() {
        secrets = tokio::task::block_in_place(|| {
            crate::network::secrets::sync_environment(&project_dir, &app_name, &env, false, false)
        })?;
    }
    if configure_bindings {
        let secrets_result = validate_secrets_for_deployment(&secrets, &env);
        if secrets_result.has_errors() {
            return Err(format!("Secret errors: {}", secrets_result.errors.join("; ")).into());
        }
        for warning in secrets_result.warnings {
            output::warning(&warning);
        }
    }
    let routes = required_env_routes(&tako_config, &env)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    let source_ip = tako_config.get_source_ip_mode(&env);
    let ssl_provider = tako_config.get_ssl_provider(&env);

    if configure_bindings {
        let ssl_result = validate_ssl_for_deployment(&routes, ssl_provider, &secrets, &env);
        if ssl_result.has_errors() {
            return Err(format!("SSL errors:\n  {}", ssl_result.errors.join("\n  ")).into());
        }
        for warning in ssl_result.warnings {
            output::warning(&format!("Validation: {}", warning));
        }
    }

    let server_names = if output::is_dry_run() {
        resolve_deploy_server_names(&tako_config, &servers, &env)
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?
    } else {
        resolve_deploy_server_names_with_setup(&tako_config, &mut servers, &env)
            .await
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?
    };

    for server_name in &server_names {
        if !servers.contains(server_name) {
            return Err(format_server_not_found_error(server_name).into());
        }
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
        let https_port = common_https_port_for_servers(&server_names, &servers);
        print_deploy_summary_with_https_port("App", &app_name, &routes, https_port);
        return Ok(());
    }

    config::refresh_native_build_targets(&mut servers, &server_names).await?;
    let server_targets = resolve_deploy_server_targets(&servers, &server_names)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    // Administrators also need the records to revoke stale grants.
    let binds_databases = tako_config
        .envs
        .get(&env)
        .is_some_and(|env| !env.databases.is_empty() || !env.kv.is_empty());
    let database_records = if binds_databases || configure_bindings {
        crate::commands::sqlite::target::env_records(&tako_config, &env)?
    } else {
        Vec::new()
    };
    let mut database_plan = crate::commands::sqlite::target::plan_bindings(
        &tako_config,
        &servers,
        &env,
        &database_records,
    )?;
    if !database_plan.is_empty() && is_container_release {
        return Err(
            "Database bindings are available to native releases only; container and Compose releases cannot use them yet."
                .into(),
        );
    }
    tracing::debug!("{}", format_servers_summary(&server_names));

    if configure_bindings {
        let runtime_state_storage_result = validate_runtime_state_storage_for_deploy(
            &project_dir,
            &tako_config,
            &secrets,
            &env,
            server_names.len(),
        );
        if runtime_state_storage_result.has_errors() {
            return Err(format!(
                "Runtime state storage errors:\n  {}",
                runtime_state_storage_result.errors.join("\n  ")
            )
            .into());
        }
        for warning in runtime_state_storage_result.warnings {
            output::warning(&format!("Validation: {}", warning));
        }

        let storage_result =
            validate_storages_for_deployment(&tako_config, &secrets, &env, server_names.len());
        if storage_result.has_errors() {
            return Err(
                format!("Storage errors:\n  {}", storage_result.errors.join("\n  ")).into(),
            );
        }
        for warning in storage_result.warnings {
            output::warning(&format!("Validation: {}", warning));
        }
    }

    let use_unified_js_target_process = is_container_release
        || should_use_unified_js_target_process(preflight_runtime_adapter.id());
    if let Some(server_targets_summary) =
        format_server_targets_summary(&server_targets, use_unified_js_target_process)
    {
        tracing::debug!("{}", server_targets_summary);
    }
    let build_groups = build_artifact_target_groups(&server_targets, use_unified_js_target_process);

    // Check the secrets key now, before starting the task tree, so missing-key
    // errors are shown against a clean terminal instead of being overpainted by
    // the live viewport.
    if configure_bindings && !output::is_dry_run() {
        crate::commands::secret::ensure_secret_key_available(&env, &secrets, Some(&project_dir))?;
        {
            crate::commands::storage::ensure_backup_keys_for_env(
                &project_dir,
                &env,
                &tako_config,
                &mut secrets,
            )?;
        }
    }

    // Only full clients publish secrets. A deploy client's servers apply what
    // full clients last published.
    if configure_bindings {
        secrets = tokio::task::block_in_place(|| {
            crate::network::secrets::sync_environment(&project_dir, &app_name, &env, true, false)
        })?;
        let final_validation = config::validate_release_secret_snapshot(
            &project_dir,
            &tako_config,
            &secrets,
            &env,
            server_names.len(),
            configure_bindings,
        );
        if final_validation.has_errors() {
            return Err(format!(
                "Secret errors after network synchronization:\n  {}",
                final_validation.errors.join("\n  ")
            )
            .into());
        }
    }

    let deploy_task_tree = should_use_deploy_task_tree()
        .then(|| DeployTaskTreeController::new(&server_names, &build_groups));

    if let (Some(task_tree), Some(first_build_group)) = (&deploy_task_tree, build_groups.first()) {
        task_tree.mark_build_target_running(&format_build_plan_target_label(first_build_group));
    }

    let use_per_server_spinners = deploy_task_tree.is_none()
        && should_use_per_server_spinners(server_names.len(), output::is_interactive());

    let preflight_deploy_app_name = tako_core::deployment_app_id(&app_name, &env);
    let mut preflight_handle = tokio::spawn(run_server_preflight_checks(
        server_names.clone(),
        servers.clone(),
        preflight_deploy_app_name,
        routes.clone(),
        force,
        deploy_task_tree.clone(),
    ));
    let build = prepare_build_phase(
        project_dir.clone(),
        source_root.clone(),
        eff_app_dir.clone(),
        app_name.clone(),
        env.clone(),
        tako_config.clone(),
        preflight_preset_ref.clone(),
        preflight_runtime_adapter,
        server_targets.clone(),
        build_groups.clone(),
        deploy_task_tree.clone(),
        description.map(str::to_owned),
    );

    let mut preflight_result: Option<Result<PreflightPhaseResult, String>> = None;
    let mut build_result: Option<BuildPhaseResult> = None;

    // The build runs project code. A client identity signs the server checks
    // first, then keeps its keys off disk until the build finishes.
    let mut suspended_keys = None;
    if crate::network::identity::is_active() {
        let result = (&mut preflight_handle)
            .await
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
                    task_tree.set_error_summary("Deploy failed".to_string());
                    task_tree.finalize();
                    return Err(output::silent_exit_error().into());
                }
                return Err(error.into());
            }
        }
        suspended_keys = crate::network::identity::suspend_keys()?;
    }
    let mut build_handle = tokio::spawn(build);

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
                            if build_result.is_none() {
                                build_handle.abort();
                            }
                            task_tree.abort_incomplete("Aborted");
                            task_tree.set_error_summary("Deploy failed".to_string());
                            task_tree.finalize();
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
                            task_tree.fail_current_build_target(error.clone());
                            if preflight_result.is_none() {
                                preflight_handle.abort();
                            }
                            task_tree.abort_incomplete("Aborted");
                            task_tree.set_error_summary("Deploy failed".to_string());
                            task_tree.finalize();
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

    if let Some(suspended_keys) = suspended_keys {
        suspended_keys.resume()?;
    }

    preflight_result
        .unwrap()
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    let BuildPhaseResult {
        version,
        manifest_main,
        deploy_storages,
        deploy_ssl,
        deploy_backup,
        use_unified_target_process: use_unified_js_target_process,
        artifacts_by_target,
    } = build_result.expect("build result should be present");

    // ===== Deploy =====

    let deployment_app_name = tako_core::deployment_app_id(&app_name, &env);
    let leader_server = server_names
        .first()
        .cloned()
        .ok_or("no servers selected for deploy")?;
    let release_command = resolve_release_command(&tako_config, &env);
    // An administrator's deploy also revokes the grants of an environment
    // that no longer binds the network's databases.
    let revokes_grants = configure_bindings && !database_records.is_empty();
    let mut granted = None;
    if !database_plan.is_empty() || revokes_grants {
        let prepared = databases::prepare(databases::Preparation {
            plan: &database_plan,
            records: &database_records,
            config: &tako_config,
            servers: &servers,
            secrets: &secrets,
            project_dir: &project_dir,
            env: &env,
            app: &deployment_app_name,
            targets: &server_names,
            administrator: configure_bindings,
            release: release_command.is_some(),
        })
        .await?;
        databases::apply_peer_keys(&mut database_plan, &prepared.peer_keys);
        granted = Some(prepared);
    }
    // Pin the environment's secret snapshot on the leader once it holds what
    // this deploy published. Every server then applies exactly that snapshot.
    let secrets_digest = if output::is_dry_run() {
        None
    } else {
        let leader = servers
            .get(&leader_server)
            .ok_or("Leader server is missing from the server inventory")?;
        let mut client = crate::management_http::ManagementClient::for_server(leader).await?;
        let published = std::mem::take(&mut secrets.published);
        Some(
            remote::query_secrets_digest(&mut client, &deployment_app_name, published)
                .await
                .map_err(|error| format!("Could not pin this deploy's secrets: {error}"))?,
        )
    };
    let deploy_config = Arc::new(DeployConfig {
        app_name: deployment_app_name.clone(),
        version: version.clone(),
        routes: routes.clone(),
        source_ip,
        secrets_digest,
        storages: deploy_storages,
        ssl: deploy_ssl,
        backup: deploy_backup,
        databases: crate::commands::sqlite::target::wire_bindings(&database_plan),
        force,
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
        // Every server runs the new release; grants only the old one used
        // can go.
        if let Some(granted) = granted {
            databases::narrow_grants(&deployment_app_name, granted).await;
        }
        if let Some(task_tree) = &deploy_task_tree {
            let https_port = common_https_port_for_servers(&server_names, &servers);
            task_tree.set_success_summary(&version, &routes, https_port);
            task_tree.finalize();
        } else {
            if output::is_pretty() {
                eprintln!();
            }
            let https_port = common_https_port_for_servers(&server_names, &servers);
            print_deploy_summary_with_https_port("Release", &version, &routes, https_port);
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

#[cfg(test)]
mod release_resolution_tests;

#[cfg(test)]
mod tests;
