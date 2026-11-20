use crate::app_command::{env_vars_from_release_dir, load_release_manifest};
use crate::release::{
    app_release_root, current_release_version, directory_modified_unix_secs,
    ensure_app_runtime_data_dirs, inject_app_data_dir_env, prepare_release_runtime,
    read_release_manifest_metadata, validate_deploy_routes, validate_release_path_for_app,
};
use crate::socket::Response;
use std::collections::HashMap;
use tako_core::{ListReleasesResponse, ReleaseInfo};

impl crate::ServerState {
    pub(crate) async fn prepare_deploy(
        &self,
        app_name: &str,
        version: &str,
        path: &str,
        routes: Vec<String>,
        ssl: tako_core::SslBinding,
        force: bool,
    ) -> Response {
        if !routes.is_empty()
            && let Err(msg) = validate_deploy_routes(&routes)
        {
            return Response::error(msg);
        }
        let release_path =
            match validate_release_path_for_app(&self.runtime.data_dir, app_name, path) {
                Ok(value) => value,
                Err(msg) => return Response::error(msg),
            };
        if let Err(error) =
            crate::protocol_compatibility::validate_release_protocol(&release_path, force)
        {
            return Response::error(error);
        }

        if routes.is_empty() {
            match load_release_manifest(&release_path) {
                Ok(manifest) if manifest.compose.is_some() => {}
                Ok(_) => {
                    return Response::error("Deploy rejected: app must define at least one route");
                }
                Err(error) => return Response::error(error),
            }
        }

        if let Err(error) = self.validate_deploy_ssl_binding(&routes, &ssl).await {
            return Response::error(format!("Cloudflare credential check failed: {error}"));
        }

        if crate::server_state::ssl_binding_needs_cloudflare_token(ssl.provider, &routes) {
            self.stage_prepared_deploy_ssl(app_name, &release_path, routes, ssl)
                .await;
        } else {
            self.clear_prepared_deploy_ssl(app_name, &release_path)
                .await;
        }

        Response::ok(serde_json::json!({
            "status": "prepared",
            "app": app_name,
            "version": version
        }))
    }

    pub(crate) async fn cleanup_prepared_deploy(&self, app_name: &str, version: &str) -> Response {
        let release_path = app_release_root(&self.runtime.data_dir, app_name, version);
        self.clear_prepared_deploy_ssl(app_name, &release_path)
            .await;
        Response::ok(serde_json::json!({ "status": "cleaned" }))
    }

    pub(crate) async fn prepare_release(
        &self,
        app_name: &str,
        path: &str,
        secrets: HashMap<String, String>,
        force: bool,
    ) -> Response {
        let release_path =
            match validate_release_path_for_app(&self.runtime.data_dir, app_name, path) {
                Ok(value) => value,
                Err(msg) => return Response::error(msg),
            };
        if let Err(error) =
            crate::protocol_compatibility::validate_release_protocol(&release_path, force)
        {
            return Response::error(error);
        }

        let env_vars = match env_vars_from_release_dir(&release_path) {
            Ok(vars) => vars,
            Err(error) => return Response::error(format!("Invalid app release: {}", error)),
        };

        let mut release_env = env_vars;
        release_env.extend(secrets);
        let data_paths = match ensure_app_runtime_data_dirs(&self.runtime.data_dir, app_name) {
            Ok(paths) => paths,
            Err(error) => return Response::error(format!("Release preparation failed: {error}")),
        };
        if let Err(error) = crate::isolation::prepare_app_filesystem_isolation(
            &self.runtime.data_dir,
            app_name,
            Some(&release_path),
            &data_paths,
        ) {
            return Response::error(format!("Release preparation failed: {error}"));
        }
        inject_app_data_dir_env(&mut release_env, &data_paths);

        let isolation =
            match crate::isolation::app_process_isolation(&self.runtime.data_dir, app_name) {
                Ok(isolation) => isolation,
                Err(error) => {
                    return Response::error(format!("Release preparation failed: {error}"));
                }
            };

        match crate::process_output::during_preparation(
            self.drain_flag(),
            prepare_release_runtime(
                &release_path,
                &release_env,
                &self.runtime.data_dir,
                #[cfg(unix)]
                Some(isolation),
            ),
        )
        .await
        {
            Ok(_) => Response::ok(serde_json::json!({ "status": "prepared" })),
            Err(error) => Response::error(format!("Release preparation failed: {error}")),
        }
    }

    pub(crate) async fn run_release(&self, run: ReleaseRun<'_>) -> Response {
        use crate::release_command;
        let ReleaseRun {
            app_name,
            version,
            path,
            command_line,
            vars,
            secrets,
            databases,
            force,
        } = run;

        if command_line.trim().is_empty() {
            return Response::error("Release command is empty".to_string());
        }
        let release_path =
            match validate_release_path_for_app(&self.runtime.data_dir, app_name, path) {
                Ok(value) => value,
                Err(msg) => return Response::error(msg),
            };
        if let Err(error) =
            crate::protocol_compatibility::validate_release_protocol(&release_path, force)
        {
            return Response::error(error);
        }

        // Acquire the per-app deploy lock so the release command runs inside
        // the same logical deploy transaction. A concurrent deploy or release
        // attempt for the same app sees the existing "already in progress"
        // error; automatic recovery gets a moment to step aside.
        let lock = self.get_deploy_lock(app_name).await;
        let _guard = match self.lock_deploy_for_operator(app_name, &lock).await {
            Ok(guard) => guard,
            Err(message) => return Response::error(message),
        };
        if let Some(response) = self.refuse_unresolved_restore(app_name) {
            return response;
        }

        let env_vars = match env_vars_from_release_dir(&release_path) {
            Ok(vars) => vars,
            Err(error) => return Response::error(format!("Invalid app release: {}", error)),
        };
        let data_paths = match ensure_app_runtime_data_dirs(&self.runtime.data_dir, app_name) {
            Ok(paths) => paths,
            Err(error) => {
                return Response::error(format!("Failed to create app data dirs: {error}"));
            }
        };
        if let Err(error) = crate::isolation::prepare_app_filesystem_isolation(
            &self.runtime.data_dir,
            app_name,
            Some(&release_path),
            &data_paths,
        ) {
            return Response::error(format!("Failed to prepare app isolation: {error}"));
        }

        let mut env = env_vars;
        env.extend(vars);
        inject_app_data_dir_env(&mut env, &data_paths);
        env.insert("TAKO_BUILD".to_string(), version.to_string());
        // Migrations need the release's databases before the release is
        // live, so the command gets temporary bindings in an envelope.
        let release_databases = ReleaseDatabases {
            state: self,
            app: app_name,
        };
        let mut bootstrap = None;
        if !databases.is_empty() {
            // The provisioned bindings supply peer keys that deployers
            // without administration do not send.
            let mut release_config = crate::instances::AppConfig::default();
            if let Some(app) = self.app_manager.get_app(app_name) {
                release_config.databases = app.config.read().databases.clone();
            }
            let connections =
                match self
                    .databases
                    .begin_release(app_name, &release_config, databases)
                {
                    Ok(connections) => connections,
                    Err(error) => {
                        return Response::error(format!("Release command failed: {error}"));
                    }
                };
            bootstrap = Some(tako_core::bootstrap::envelope_string(
                "",
                tako_core::bootstrap::Bindings {
                    secrets: &secrets,
                    storages: &HashMap::new(),
                    databases: &connections,
                },
            ));
        }
        env.extend(secrets);
        // After the secrets, so a secret of the same name cannot replace it.
        if let Some(bootstrap) = bootstrap {
            env.insert(
                tako_core::bootstrap::TAKO_BOOTSTRAP_DATA_ENV.to_string(),
                bootstrap,
            );
        }
        if let Ok(path) = std::env::var("PATH") {
            env.entry("PATH".to_string()).or_insert(path);
        }

        let isolation =
            match crate::isolation::app_process_isolation(&self.runtime.data_dir, app_name) {
                Ok(isolation) => isolation,
                Err(error) => return Response::error(format!("Release command failed: {error}")),
            };

        let outcome = crate::process_output::during_preparation(
            self.drain_flag(),
            release_command::run(
                command_line,
                &release_path,
                &env,
                #[cfg(unix)]
                Some(isolation),
            ),
        )
        .await;
        drop(release_databases);
        match outcome {
            Err(spawn_err) => Response::error(format!("Release command failed: {spawn_err}")),
            Ok(outcome) if outcome.succeeded() => Response::ok(serde_json::json!({
                "status": "released",
                "exit_code": 0,
                "stdout_tail": tail_string(&outcome.stdout, 4_000),
                "stderr_tail": tail_string(&outcome.stderr, 4_000),
            })),
            Ok(outcome) if outcome.timed_out => Response::error(format!(
                "Release command timed out after {}s\n--- stderr ---\n{}",
                release_command::RELEASE_COMMAND_TIMEOUT.as_secs(),
                tail_string(&outcome.stderr, 4_000),
            )),
            Ok(outcome) => Response::error(format!(
                "Release command exit {}\n--- stderr ---\n{}",
                outcome
                    .exit_code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "?".into()),
                tail_string(&outcome.stderr, 4_000),
            )),
        }
    }

    pub(crate) async fn list_releases(&self, app_name: &str) -> Response {
        let _app = match self.app_manager.get_app(app_name) {
            Some(app) => app,
            None => return Response::error(format!("App not found: {}", app_name)),
        };

        let app_root = self.runtime.data_dir.join("apps").join(app_name);
        let releases_root = app_root.join("releases");
        let current_version = current_release_version(&app_root);

        let mut releases = Vec::new();
        let entries = match super::release_artifacts::release_entries(&releases_root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Response::ok(ListReleasesResponse {
                    app: app_name.to_string(),
                    releases,
                });
            }
            Err(error) => {
                return Response::error(format!(
                    "Failed to read releases directory '{}': {}",
                    releases_root.display(),
                    error
                ));
            }
        };

        for (version, release_root) in entries {
            let manifest_path = release_root.join("app.json");
            let metadata = read_release_manifest_metadata(&manifest_path);
            releases.push(ReleaseInfo {
                current: current_version.as_deref() == Some(version.as_str()),
                deployed_at_unix_secs: directory_modified_unix_secs(
                    &release_root.join("release.tar.zst"),
                ),
                version,
                commit_message: metadata.commit_message,
                description: metadata.description,
                git_dirty: metadata.git_dirty,
                sdk: metadata.sdk,
            });
        }

        releases.sort_by(|a, b| {
            b.deployed_at_unix_secs
                .cmp(&a.deployed_at_unix_secs)
                .then_with(|| b.version.cmp(&a.version))
        });

        Response::ok(ListReleasesResponse {
            app: app_name.to_string(),
            releases,
        })
    }

    pub(crate) async fn rollback_app(&self, app_name: &str, version: &str) -> Response {
        self.rollback_app_as(app_name, version, super::Caller::Full)
            .await
    }

    pub(crate) async fn rollback_app_as(
        &self,
        app_name: &str,
        version: &str,
        caller: super::Caller,
    ) -> Response {
        let lock = self.get_deploy_lock(app_name).await;
        let _guard = match self.lock_deploy_for_operator(app_name, &lock).await {
            Ok(guard) => guard,
            Err(error) => return Response::error(error),
        };
        if let Some(response) = self.refuse_unresolved_restore(app_name) {
            return response;
        }
        let release_preexisted =
            app_release_root(&self.runtime.data_dir, app_name, version).is_dir();
        let pending_preexisted = self
            .runtime
            .data_dir
            .join("apps")
            .join(app_name)
            .join("archives")
            .join(version)
            .join("pending")
            .exists();
        let response = self.rollback_app_locked(app_name, version, caller).await;
        if !response.is_ok()
            && !self
                .app_manager
                .get_app(app_name)
                .is_some_and(|app| app.version() == version)
        {
            // Keep the immutable archive when preparation or readiness fails.
            if let Err(error) = self
                .discard_restored_release(app_name, version, release_preexisted, pending_preexisted)
                .await
            {
                tracing::warn!(app = app_name, version, %error, "Could not clean up failed rollback");
            }
        }
        response
    }

    async fn rollback_app_locked(
        &self,
        app_name: &str,
        version: &str,
        caller: super::Caller,
    ) -> Response {
        let environment = match self.app_manager.get_app(app_name) {
            Some(app) => app.config.read().environment.clone(),
            None => return Response::error(format!("App not found: {}", app_name)),
        };

        let (target_path, needs_preparation) =
            match self.restore_release_artifact(app_name, version).await {
                Ok(restored) => restored,
                Err(error) => return Response::error(error),
            };

        let target = match load_release_manifest(&target_path) {
            Ok(manifest) => manifest,
            Err(error) => return Response::error(error),
        };
        let historical = match self.state_store.release_routing(app_name, version) {
            Ok(value) => value,
            Err(error) => {
                return Response::error(format!("Failed to read release routes: {error}"));
            }
        };
        // Compose routes are part of the immutable service configuration. A
        // rollback restores those bindings, including a Docker-only group.
        let routes = match &target.compose {
            Some(compose) => compose.routes(&environment),
            None => historical
                .as_ref()
                .map(|routing| routing.routes.clone())
                .unwrap_or_else(|| self.routes.read().routes_for_app(app_name)),
        };
        if routes.is_empty() && target.compose.is_none() {
            return Response::error(format!(
                "Cannot rollback '{}': no routes are configured",
                app_name
            ));
        }
        // An older release may name a route another app environment owns now.
        if let Err(error) = self.refuse_claimed_routes(app_name, &routes, caller) {
            return Response::error(error);
        }
        let ssl = match self.state_store.get_ssl(app_name) {
            Ok(value) => value.unwrap_or_default(),
            Err(error) => {
                return Response::error(format!("Failed to read SSL credentials: {error}"));
            }
        };
        let source_ip = historical
            .map(|routing| routing.source_ip)
            .unwrap_or_else(|| {
                self.app_manager
                    .get_app(app_name)
                    .map(|app| app.config.read().source_ip)
                    .unwrap_or_default()
            });

        let target_path = target_path.to_string_lossy();
        if needs_preparation {
            let secrets = match self.state_store.get_secrets(app_name) {
                Ok(secrets) => secrets,
                Err(error) => return Response::error(format!("Failed to read secrets: {error}")),
            };
            let prepared = self
                .prepare_release(app_name, &target_path, secrets, false)
                .await;
            if !prepared.is_ok() {
                return prepared;
            }
        }
        let response = self
            .deploy_app_locked(super::deploy::DeployRequest {
                app_name,
                version,
                path: &target_path,
                routes,
                source_ip,
                secrets: None,
                runtime_credentials: None,
                storages: None,
                databases: None,
                ssl,
                backup: super::deploy::BackupUpdate::Preserve,
                force: false,
            })
            .await;
        if matches!(response, Response::Ok { .. }) {
            // Rollback runs directly through this RPC rather than the CLI's
            // deploy pipeline, which normally finalizes the release marker.
            if let Err(error) = self.finalize_release_locked(app_name, version).await {
                return Response::error(format!(
                    "Rollback activated '{version}', but release finalization failed: {error}"
                ));
            }
        }
        response
    }
}

fn tail_string(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut start = s.len() - max_bytes;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &s[start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_string_handles_multibyte_boundary() {
        let input = "あ".repeat(2_000);

        let tail = tail_string(&input, 4_000);

        assert!(tail.starts_with('…'));
        assert!(tail.len() <= 4_000 + "…".len());
    }
}

/// A release command to run once before a deploy's rollout.
pub(crate) struct ReleaseRun<'a> {
    pub(crate) app_name: &'a str,
    pub(crate) version: &'a str,
    pub(crate) path: &'a str,
    pub(crate) command_line: &'a str,
    pub(crate) vars: HashMap<String, String>,
    pub(crate) secrets: HashMap<String, String>,
    pub(crate) databases: HashMap<String, tako_core::DatabaseBinding>,
    pub(crate) force: bool,
}

/// Removes a release command's temporary database bindings.
struct ReleaseDatabases<'a> {
    state: &'a crate::ServerState,
    app: &'a str,
}

impl Drop for ReleaseDatabases<'_> {
    fn drop(&mut self) {
        self.state.databases.end_release(self.app);
    }
}
