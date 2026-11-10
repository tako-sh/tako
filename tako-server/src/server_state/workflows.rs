use super::*;

impl ServerState {
    /// Reconcile workflow + channel runtime support for the active release.
    ///
    /// Deploys, restores, and secret rotations all funnel through here so the
    /// worker runtime follows the current release and its current secrets. If a
    /// new release drops the configured workflows directory, any previously
    /// managed worker is drained and removed.
    pub(crate) async fn sync_app_workflows(
        &self,
        app_name: &str,
        release_path: &std::path::Path,
        runtime_bin_path: Option<&str>,
    ) {
        if self.is_draining() {
            return;
        }
        if let Err(error) = self
            .try_sync_app_workflows(app_name, release_path, runtime_bin_path)
            .await
        {
            tracing::warn!(app = app_name, %error, "Failed to synchronize workflows");
        }
    }

    pub(crate) async fn try_sync_app_workflows(
        &self,
        app_name: &str,
        release_path: &std::path::Path,
        runtime_bin_path: Option<&str>,
    ) -> Result<(), StateStoreError> {
        let manifest = match crate::app_command::load_release_manifest(release_path) {
            Ok(manifest) => manifest,
            Err(e) => {
                tracing::warn!(app = app_name, error = %e, "Skipping workflow engine: could not load release manifest");
                return Err(StateStoreError::InvalidData(e.to_string()));
            }
        };
        let app_path = match crate::app_command::safe_subdir(release_path, &manifest.app_dir) {
            Ok(path) => path,
            Err(e) => {
                tracing::warn!(app = app_name, error = %e, "Skipping workflow engine: invalid app_dir in manifest");
                return Err(StateStoreError::InvalidData(e.to_string()));
            }
        };
        if manifest.release_kind == crate::app_command::ReleaseKind::Compose {
            self.workflows.retire(app_name).await;
            return Ok(());
        }
        let data_paths = match ensure_app_runtime_data_dirs(&self.runtime.data_dir, app_name) {
            Ok(paths) => paths,
            Err(error) => {
                tracing::warn!(app = app_name, error = %error, "Skipping workflow engine: failed to prepare app data dirs");
                return Err(StateStoreError::InvalidData(error.to_string()));
            }
        };
        let internal_socket = self.workflows.socket_path();

        let secrets = self.state_store.get_secrets(app_name)?;
        let storages = self.state_store.get_storages(app_name)?;
        let mut worker_env = manifest.env_vars.clone();
        inject_app_data_dir_env(&mut worker_env, &data_paths);

        let worker_command = match manifest.runtime.as_str() {
            "container" => {
                let Some(run) = manifest.workflow_run.as_deref() else {
                    self.workflows.retire(app_name).await;
                    return Ok(());
                };
                let engine = match detect_container_engine() {
                    Ok(engine) => engine,
                    Err(error) => {
                        tracing::warn!(app = app_name, error = %error, "Skipping workflow engine: container engine not available");
                        self.workflows.retire(app_name).await;
                        return Err(StateStoreError::InvalidData(error.to_string()));
                    }
                };
                let image = match image_tag_for_manifest(&manifest) {
                    Ok(image) => image,
                    Err(error) => {
                        tracing::warn!(app = app_name, error = %error, "Skipping workflow engine: invalid container image tag");
                        self.workflows.retire(app_name).await;
                        return Err(StateStoreError::InvalidData(error.to_string()));
                    }
                };
                worker_env.insert(
                    tako_core::instance_env::TAKO_APP_NAME_ENV.to_string(),
                    app_name.to_string(),
                );
                worker_env.insert(
                    tako_core::instance_env::TAKO_INTERNAL_SOCKET_ENV.to_string(),
                    internal_socket.display().to_string(),
                );
                let token = nanoid::nanoid!(32);
                let container_env = build_container_run_env(
                    &worker_env,
                    &token,
                    &secrets,
                    &storages,
                    crate::container_runtime::DEFAULT_CONTAINER_PORT,
                    &manifest.container_options,
                )
                .into_iter()
                .collect();
                let args = match build_container_workflow_run_args(
                    &image,
                    &worker_env,
                    &token,
                    &secrets,
                    &storages,
                    run,
                    &internal_socket,
                    &manifest.container_options,
                ) {
                    Ok(args) => args,
                    Err(error) => {
                        tracing::warn!(app = app_name, error = %error, "Skipping workflow engine: invalid container workflow run");
                        self.workflows.retire(app_name).await;
                        return Err(StateStoreError::InvalidData(error.to_string()));
                    }
                };
                worker_env = container_env;
                std::iter::once(std::ffi::OsString::from(engine.binary()))
                    .chain(args.into_iter().map(std::ffi::OsString::from))
                    .collect()
            }
            "go" => {
                let Some(worker_main) = manifest.workflow_worker_main.as_deref() else {
                    self.workflows.retire(app_name).await;
                    return Ok(());
                };
                let worker_bin = app_path.join(worker_main);
                if !worker_bin.exists() {
                    tracing::warn!(
                        app = app_name,
                        path = %worker_bin.display(),
                        "Skipping workflow engine: Go worker binary not found in release"
                    );
                    self.workflows.retire(app_name).await;
                    return Err(StateStoreError::InvalidData(
                        "Go worker binary not found in release".into(),
                    ));
                }
                match runtime_bin_path {
                    Some(bin) => vec![std::ffi::OsString::from(bin), worker_bin.into()],
                    None => vec![worker_bin.into()],
                }
            }
            _ => {
                let js_app_root = worker_env
                    .get("TAKO_APP_ROOT")
                    .map(String::as_str)
                    .filter(|root| !root.trim().is_empty())
                    .unwrap_or("src");
                let workflows_dir = if js_app_root == "." {
                    app_path.join("workflows")
                } else {
                    app_path.join(js_app_root).join("workflows")
                };
                if !workflows_dir.is_dir() {
                    self.workflows.retire(app_name).await;
                    return Ok(());
                }

                let worker_entry = app_path
                    .join("node_modules")
                    .join("tako.sh")
                    .join("dist")
                    .join("entrypoints")
                    .join("bun-worker.mjs");
                if !worker_entry.exists() {
                    tracing::warn!(
                        app = app_name,
                        path = %worker_entry.display(),
                        "Skipping workflow engine: worker entrypoint not found in release"
                    );
                    self.workflows.retire(app_name).await;
                    return Err(StateStoreError::InvalidData(
                        "worker entrypoint not found in release".into(),
                    ));
                }

                let runtime_bin = runtime_bin_path
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| std::path::PathBuf::from("bun"));
                vec![runtime_bin.into(), worker_entry.into()]
            }
        };
        let workflow_lanes = match manifest.runtime.as_str() {
            "go" | "container" => Vec::new(),
            _ => {
                let js_app_root = worker_env
                    .get("TAKO_APP_ROOT")
                    .map(String::as_str)
                    .filter(|root| !root.trim().is_empty())
                    .unwrap_or("src");
                let workflows_dir = if js_app_root == "." {
                    app_path.join("workflows")
                } else {
                    app_path.join(js_app_root).join("workflows")
                };
                crate::workflows::workflow_lanes_from_dir(&workflows_dir, 0, 500)
            }
        };
        let isolation = if manifest.runtime == "container" {
            None
        } else {
            match crate::isolation::app_process_isolation(&self.runtime.data_dir, app_name) {
                Ok(isolation) => Some(isolation),
                Err(error) => {
                    tracing::warn!(app = app_name, error = %error, "Skipping workflow engine: failed to prepare app isolation");
                    return Err(StateStoreError::InvalidData(error.to_string()));
                }
            }
        };

        self.ensure_internal_socket_started()?;
        let app = app_name.to_string();
        let app_for_spec = app.clone();
        let worker_cwd = app_path;
        let manager = self.workflows.clone();
        let result = manager
            .ensure(&app, manifest.workflow_retention_ms, move |_db_path| {
                let mut spec = crate::workflows::worker_spec_for_command(
                    &app_for_spec,
                    0,       // workers (scale-to-zero)
                    500,     // concurrency
                    300_000, // idle_timeout_ms (5 min)
                    &internal_socket,
                    worker_command,
                    &worker_cwd,
                    worker_env,
                    secrets,
                    storages,
                    isolation,
                );
                spec.lanes = workflow_lanes;
                spec
            })
            .await;

        result.map_err(|error| StateStoreError::InvalidData(error.to_string()))
    }
}
