use crate::app_command::env_vars_from_release_dir;
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
    ) -> Response {
        if let Err(msg) = validate_deploy_routes(&routes) {
            return Response::error(msg);
        }
        let release_path =
            match validate_release_path_for_app(&self.runtime.data_dir, app_name, path) {
                Ok(value) => value,
                Err(msg) => return Response::error(msg),
            };

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

    pub(crate) async fn prepare_release(&self, app_name: &str, path: &str) -> Response {
        let release_path =
            match validate_release_path_for_app(&self.runtime.data_dir, app_name, path) {
                Ok(value) => value,
                Err(msg) => return Response::error(msg),
            };

        let env_vars = match env_vars_from_release_dir(&release_path) {
            Ok(vars) => vars,
            Err(error) => return Response::error(format!("Invalid app release: {}", error)),
        };

        let secrets = self.state_store.get_secrets(app_name).unwrap_or_default();
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

        match prepare_release_runtime(
            &release_path,
            &release_env,
            &self.runtime.data_dir,
            #[cfg(unix)]
            Some(isolation),
        )
        .await
        {
            Ok(_) => Response::ok(serde_json::json!({ "status": "prepared" })),
            Err(error) => Response::error(format!("Release preparation failed: {error}")),
        }
    }

    pub(crate) async fn run_release(
        &self,
        app_name: &str,
        version: &str,
        path: &str,
        command_line: &str,
        vars: HashMap<String, String>,
        secrets: HashMap<String, String>,
    ) -> Response {
        use crate::release_command;

        if command_line.trim().is_empty() {
            return Response::error("Release command is empty".to_string());
        }
        let release_path =
            match validate_release_path_for_app(&self.runtime.data_dir, app_name, path) {
                Ok(value) => value,
                Err(msg) => return Response::error(msg),
            };

        // Acquire the per-app deploy lock so the release command runs inside
        // the same logical deploy transaction. A concurrent deploy or release
        // attempt for the same app sees the existing "already in progress"
        // error.
        let lock = self.get_deploy_lock(app_name).await;
        let _guard = match lock.try_lock() {
            Ok(guard) => guard,
            Err(_) => {
                return Response::error(format!(
                    "Deploy already in progress for app '{}'. Please wait and try again.",
                    app_name
                ));
            }
        };

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
        env.extend(secrets);
        if let Ok(path) = std::env::var("PATH") {
            env.entry("PATH".to_string()).or_insert(path);
        }

        let isolation =
            match crate::isolation::app_process_isolation(&self.runtime.data_dir, app_name) {
                Ok(isolation) => isolation,
                Err(error) => return Response::error(format!("Release command failed: {error}")),
            };

        match release_command::run(
            command_line,
            &release_path,
            &env,
            #[cfg(unix)]
            Some(isolation),
        )
        .await
        {
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
        let entries = match std::fs::read_dir(&releases_root) {
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

        for entry in entries.flatten() {
            let release_root = entry.path();
            if !release_root.is_dir() {
                continue;
            }

            let Some(version) = entry.file_name().to_str().map(|value| value.to_string()) else {
                continue;
            };

            let manifest_path = release_root.join("app.json");
            let (commit_message, git_dirty) = read_release_manifest_metadata(&manifest_path);
            releases.push(ReleaseInfo {
                current: current_version.as_deref() == Some(version.as_str()),
                deployed_at_unix_secs: directory_modified_unix_secs(&release_root),
                version,
                commit_message,
                git_dirty,
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
        let _app = match self.app_manager.get_app(app_name) {
            Some(app) => app,
            None => return Response::error(format!("App not found: {}", app_name)),
        };

        let app_root = self.runtime.data_dir.join("apps").join(app_name);
        let target_path = app_root.join("releases").join(version);

        if !target_path.is_dir() {
            return Response::error(format!(
                "Release '{}' not found for app '{}'",
                version, app_name
            ));
        }

        let routes = {
            let route_table = self.routes.read();
            route_table.routes_for_app(app_name)
        };
        if routes.is_empty() {
            return Response::error(format!(
                "Cannot rollback '{}': no routes are configured",
                app_name
            ));
        }
        let ssl = match self.state_store.get_ssl(app_name) {
            Ok(value) => value.unwrap_or_default(),
            Err(error) => {
                return Response::error(format!("Failed to read SSL credentials: {error}"));
            }
        };
        let source_ip = self
            .app_manager
            .get_app(app_name)
            .map(|app| app.config.read().source_ip)
            .unwrap_or_default();

        let target_path = target_path.to_string_lossy();
        self.deploy_app(super::deploy::DeployRequest {
            app_name,
            version,
            path: &target_path,
            routes,
            source_ip,
            secrets: None,
            storages: None,
            ssl,
            backup: None,
        })
        .await
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
