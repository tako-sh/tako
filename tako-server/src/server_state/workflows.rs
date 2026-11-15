use super::*;
use tako_workflows::manager::{PreparedWorkflow, ScheduleActivation};

pub(crate) struct WorkflowBindings<'a> {
    pub(crate) secrets: &'a HashMap<String, String>,
    pub(crate) storages: &'a HashMap<String, tako_core::StorageBinding>,
    pub(crate) postgres_url: Option<&'a String>,
}

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
        let secrets = self.state_store.get_secrets(app_name)?;
        let storages = self.state_store.get_storages(app_name)?;
        let runtime_credentials = self.state_store.get_runtime_credentials(app_name)?;
        let prepared = self
            .prepare_app_workflows(
                app_name,
                release_path,
                runtime_bin_path,
                WorkflowBindings {
                    secrets: &secrets,
                    storages: &storages,
                    postgres_url: runtime_credentials.get("postgres_url"),
                },
                ScheduleActivation::Recover(self.state_store.workflow_schedule_revision(app_name)?),
            )
            .await?;
        self.install_app_workflows(app_name, prepared);
        Ok(())
    }

    pub(crate) fn install_app_workflows(&self, app_name: &str, prepared: Option<PreparedWorkflow>) {
        match prepared {
            Some(runtime) => self.workflows.install(app_name, runtime),
            None => self.workflows.retire(app_name),
        }
    }

    pub(crate) async fn prepare_app_workflows(
        &self,
        app_name: &str,
        release_path: &std::path::Path,
        runtime_bin_path: Option<&str>,
        bindings: WorkflowBindings<'_>,
        activation: ScheduleActivation,
    ) -> Result<Option<PreparedWorkflow>, StateStoreError> {
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
            return Ok(None);
        }
        let data_paths = match ensure_app_runtime_data_dirs(&self.runtime.data_dir, app_name) {
            Ok(paths) => paths,
            Err(error) => {
                tracing::warn!(app = app_name, error = %error, "Skipping workflow engine: failed to prepare app data dirs");
                return Err(StateStoreError::InvalidData(error.to_string()));
            }
        };
        let internal_socket = self.workflows.socket_path();

        let WorkflowBindings {
            secrets,
            storages,
            postgres_url,
        } = bindings;
        let mut worker_env = manifest.env_vars.clone();
        // Older manifests can omit the version. Their release directory still
        // identifies the build used by the server during deploy and recovery.
        let build = if manifest.version.is_empty() {
            release_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
        } else {
            &manifest.version
        };
        worker_env.insert("TAKO_BUILD".into(), build.to_string());
        inject_app_data_dir_env(&mut worker_env, &data_paths);

        let worker_command = match manifest.runtime.as_str() {
            "container" => {
                let Some(run) = manifest.workflow_run.as_deref() else {
                    return Ok(None);
                };
                let engine = match detect_container_engine() {
                    Ok(engine) => engine,
                    Err(error) => {
                        tracing::warn!(app = app_name, error = %error, "Skipping workflow engine: container engine not available");
                        return Err(StateStoreError::InvalidData(error.to_string()));
                    }
                };
                let image = match image_tag_for_manifest(&manifest) {
                    Ok(image) => image,
                    Err(error) => {
                        tracing::warn!(app = app_name, error = %error, "Skipping workflow engine: invalid container image tag");
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
                    secrets,
                    storages,
                    crate::container_runtime::DEFAULT_CONTAINER_PORT,
                    &manifest.container_options,
                )
                .into_iter()
                .collect();
                let args = match build_container_workflow_run_args(
                    &image,
                    &worker_env,
                    &token,
                    secrets,
                    storages,
                    run,
                    &internal_socket,
                    &manifest.container_options,
                ) {
                    Ok(args) => args,
                    Err(error) => {
                        tracing::warn!(app = app_name, error = %error, "Skipping workflow engine: invalid container workflow run");
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
                    return Ok(None);
                };
                let worker_bin = app_path.join(worker_main);
                if !worker_bin.exists() {
                    tracing::warn!(
                        app = app_name,
                        path = %worker_bin.display(),
                        "Skipping workflow engine: Go worker binary not found in release"
                    );
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
                    return Ok(None);
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
        let store = match postgres_url {
            Some(url) => tako_workflows::WorkflowStoreConfig::postgres(url.clone(), app_name),
            None => tako_workflows::WorkflowStoreConfig::sqlite(
                self.workflows.workflows_db_path(app_name),
            ),
        };
        let mut spec = crate::workflows::worker_spec_for_command(
            app_name,
            0,
            500,
            300_000,
            &internal_socket,
            worker_command,
            &app_path,
            worker_env,
            secrets.clone(),
            storages.clone(),
            isolation,
        );
        spec.lanes = workflow_lanes;
        self.workflows
            .prepare(store, manifest.workflow_retention_ms, spec, activation)
            .await
            .map(Some)
            .map_err(|error| StateStoreError::InvalidData(error.to_string()))
    }
}
