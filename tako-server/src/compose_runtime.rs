use crate::app_command::{ReleaseManifest, safe_subdir};
#[cfg(test)]
use crate::release::TAKO_APP_DATA_DIR_ENV;
#[cfg(test)]
use serde_json::json;
#[cfg(test)]
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::Path;
pub(crate) use tako_compose::ComposeRuntime;
#[cfg(test)]
use tako_compose::{render_override, validate_model};
#[cfg(test)]
use tako_core::compose::ComposeConfig;

pub(crate) fn for_config(config: &crate::instances::AppConfig) -> Result<ComposeRuntime, String> {
    let mut env = config.env_vars.clone();
    env.extend(config.secrets.clone());
    if let Some(data) = config.env_vars.get(crate::release::TAKO_APP_DATA_DIR_ENV) {
        env.insert(crate::release::TAKO_APP_DATA_DIR_ENV.into(), data.clone());
    }
    ComposeRuntime::new(
        config.path.clone(),
        config
            .compose
            .clone()
            .ok_or("App is not a Compose project")?,
        env,
    )
    .map(|runtime| runtime.with_replicas(config.compose_replicas.clone()))
}

pub(crate) async fn prepare(
    release: &Path,
    manifest: &ReleaseManifest,
    env: &HashMap<String, String>,
) -> Result<(), String> {
    let config = manifest
        .compose
        .clone()
        .ok_or("Compose release is missing compose settings")?;
    let directory = safe_subdir(release, &manifest.app_dir)?;
    ComposeRuntime::new(directory, config, env.clone())?
        .prepare(&manifest.version)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_replaces_public_ports_and_pins_each_service() {
        let config = ComposeConfig {
            file: "compose.yaml".into(),
            service: "web".into(),
            port: 8080,
            health_path: "/".into(),
            health_host: None,
        };
        let images = BTreeMap::from([
            ("web".into(), "local:web-v1".into()),
            ("db".into(), "local:db-v1".into()),
        ]);
        let output = render_override(&images, &config, 45001);
        assert!(output.contains("ports: !override [\"127.0.0.1:45001:8080\"]"));
        assert!(output.contains("scale: 1\n    deploy:\n      replicas: 1"));
        assert!(output.contains("image: \"local:db-v1\"\n    ports: !override []"));
    }

    #[test]
    fn reject_release_relative_writable_data_and_host_networking() {
        let dir = tempfile::tempdir().unwrap();
        let mut model = json!({"services":{"web":{"image":"nginx","network_mode":"host"}}});
        assert!(
            validate_model(&model, dir.path(), "web")
                .unwrap_err()
                .contains("network_mode")
        );
        model["services"]["web"] =
            json!({"volumes":[{"type":"bind","source":dir.path(),"read_only":false}]});
        assert!(
            validate_model(&model, dir.path(), "web")
                .unwrap_err()
                .contains("named volumes")
        );
        model["services"]["web"]["volumes"][0]["read_only"] = json!(true);
        validate_model(&model, dir.path(), "web").unwrap();
    }

    #[test]
    fn project_identity_survives_release_changes_and_separates_environments() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("compose.yaml"), "services: {}\n").unwrap();
        let config = ComposeConfig {
            file: "compose.yaml".into(),
            service: "web".into(),
            port: 8080,
            health_path: "/".into(),
            health_host: None,
        };
        let production = HashMap::from([(
            TAKO_APP_DATA_DIR_ENV.into(),
            "/srv/app/production/data/app".into(),
        )]);
        let staging = HashMap::from([(
            TAKO_APP_DATA_DIR_ENV.into(),
            "/srv/app/staging/data/app".into(),
        )]);
        let a = ComposeRuntime::new(dir.path().into(), config.clone(), production.clone()).unwrap();
        let b = ComposeRuntime::new(dir.path().into(), config.clone(), production).unwrap();
        let c = ComposeRuntime::new(dir.path().into(), config, staging).unwrap();
        assert_eq!(a.project, b.project);
        assert_ne!(a.project, c.project);
    }

    #[tokio::test]
    #[ignore = "requires Docker Compose and shared build paths"]
    async fn first_compose_prepare_uses_incoming_secrets_without_committing_them() {
        let dir = crate::isolation::fixture::TestDataDir::new().unwrap();
        let app = "compose/production";
        let data = crate::release::ensure_app_runtime_data_dirs(dir.path(), app).unwrap();
        let release = crate::release::app_release_root(dir.path(), app, "v1");
        std::fs::create_dir_all(&release).unwrap();
        let config = ComposeConfig {
            file: "compose.yaml".into(),
            service: "web".into(),
            port: 8080,
            health_path: "/".into(),
            health_host: None,
        };
        std::fs::write(release.join("app.json"), json!({
            "protocol_version":tako_core::PROTOCOL_VERSION,
            "release_kind":"compose", "app_name":"compose", "environment":"production",
            "version":"v1", "runtime":"compose", "main":"", "idle_timeout":300, "compose":config,
        }).to_string()).unwrap();
        std::fs::write(
            release.join("compose.yaml"),
            include_str!("../../e2e/fixtures/compose/web-worker/compose.yaml"),
        )
        .unwrap();
        std::fs::write(
            release.join("Dockerfile"),
            "FROM docker.io/library/busybox:1.37\nCMD [\"httpd\",\"-f\",\"-p\",\"8080\"]\n",
        )
        .unwrap();
        let state = crate::ServerState::new(
            dir.path().into(),
            std::sync::Arc::new(crate::tls::CertManager::new(
                crate::tls::CertManagerConfig {
                    cert_dir: dir.path().join("certs"),
                    ..Default::default()
                },
            )),
            None,
            std::sync::Arc::new(parking_lot::RwLock::new(HashMap::new())),
        )
        .unwrap();
        let prepared = state
            .handle_command(tako_core::Command::PrepareRelease {
                app: app.into(),
                path: release.display().to_string(),
                secrets: HashMap::from([("DB_PASSWORD".into(), "fixture-only".into())]),
                force: false,
            })
            .await;
        assert!(
            matches!(prepared, tako_core::Response::Ok { .. }),
            "Compose preparation with incoming secrets must succeed"
        );
        assert!(state.state_store.get_secrets(app).unwrap().is_empty());
        let runtime = ComposeRuntime::new(
            release,
            config,
            HashMap::from([
                (TAKO_APP_DATA_DIR_ENV.into(), data.app.display().to_string()),
                ("DB_PASSWORD".into(), "fixture-only".into()),
            ]),
        )
        .unwrap();
        assert!(runtime.begin_rollout().await.is_ok());
        assert!(runtime.containers().await.unwrap().is_empty());
    }

    #[tokio::test]
    #[ignore = "requires Docker Compose and shared host/container loopback and project paths"]
    async fn delete_then_redeploy_recreates_private_mounts_and_preserves_named_data() {
        use std::sync::Arc;
        use tako_core::{Command, Response};
        let dir = crate::isolation::fixture::TestDataDir::new().unwrap();
        let name = "delete-compose/production";
        let state = crate::ServerState::new(
            dir.path().into(),
            Arc::new(crate::tls::CertManager::new(
                crate::tls::CertManagerConfig {
                    cert_dir: dir.path().join("certs"),
                    ..Default::default()
                },
            )),
            None,
            Arc::new(parking_lot::RwLock::new(HashMap::new())),
        )
        .unwrap();
        let config = ComposeConfig {
            file: "compose.yaml".into(),
            service: "web".into(),
            port: 8080,
            health_path: "/".into(),
            health_host: None,
        };
        let release = crate::release::app_release_root(dir.path(), name, "v1");
        let yaml = "services:\n  web:\n    image: docker.io/library/busybox:1.37\n    command: [httpd, -f, -p, '8080', -h, /www]\n    volumes: ['./public:/www:ro', 'state:/state']\nvolumes:\n  state:\n";
        struct Cleanup(String);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                if let Ok(output) = std::process::Command::new("docker")
                    .args([
                        "ps",
                        "-aq",
                        "--filter",
                        &format!("label=com.docker.compose.project={}", self.0),
                    ])
                    .output()
                {
                    let ids = String::from_utf8_lossy(&output.stdout);
                    if !ids.trim().is_empty() {
                        let _ = std::process::Command::new("docker")
                            .args(["rm", "--force", "--volumes"])
                            .args(ids.split_whitespace())
                            .output();
                    }
                }
                let _ = std::process::Command::new("docker")
                    .args(["volume", "rm", &format!("{}_state", self.0)])
                    .output();
            }
        }
        let mut cleanup = None;
        let mut old = None;
        for attempt in 0..2 {
            let data = crate::release::ensure_app_runtime_data_dirs(dir.path(), name).unwrap();
            std::fs::create_dir_all(release.join("public")).unwrap();
            std::fs::write(release.join("public/index.html"), "mounted content").unwrap();
            std::fs::write(release.join("compose.yaml"), yaml).unwrap();
            std::fs::write(
                release.join("app.json"),
                json!({
                    "protocol_version":tako_core::PROTOCOL_VERSION, "release_kind":"compose",
                    "app_name":"delete-compose", "environment":"production", "version":"v1",
                    "runtime":"compose", "main":"", "idle_timeout":300, "compose":config,
                })
                .to_string(),
            )
            .unwrap();
            let runtime = ComposeRuntime::new(
                release.clone(),
                config.clone(),
                HashMap::from([(TAKO_APP_DATA_DIR_ENV.into(), data.app.display().to_string())]),
            )
            .unwrap();
            if cleanup.is_none() {
                cleanup = Some(Cleanup(runtime.project.clone()));
            }
            runtime.prepare("v1").await.unwrap();
            let deploy = serde_json::from_value(json!({"command":"deploy", "app":name,
                "version":"v1", "path":release, "routes":["delete.localhost"]}))
            .unwrap();
            assert!(matches!(
                state.handle_command(deploy).await,
                Response::Ok { .. }
            ));
            let containers = runtime.containers().await.unwrap();
            let id = &containers[0].id;
            if attempt == 0 {
                assert!(
                    tokio::process::Command::new("docker")
                        .args(["exec", id, "sh", "-c", "echo retained > /state/marker"])
                        .status()
                        .await
                        .unwrap()
                        .success()
                );
                old = Some(id.clone());
            } else {
                assert_ne!(Some(id), old.as_ref());
                let output = tokio::process::Command::new("docker")
                    .args(["exec", id, "cat", "/state/marker"])
                    .output()
                    .await
                    .unwrap();
                assert!(output.status.success());
                assert_eq!(output.stdout, b"retained\n");
            }
            assert!(matches!(
                state
                    .handle_command(Command::Delete { app: name.into() })
                    .await,
                Response::Ok { .. }
            ));
            assert!(runtime.containers().await.unwrap().is_empty());
            assert!(!crate::release::app_root(dir.path(), name).exists());
        }
    }

    #[tokio::test]
    #[ignore = "requires Docker Compose, Python 3, and shared host/container loopback and project paths"]
    async fn leaving_compose_stops_background_services_and_failed_native_deploy_restores_them() {
        use std::sync::Arc;
        use tako_core::{Command, Response};

        for missing_http_instance in [false, true] {
            let dir = crate::isolation::fixture::TestDataDir::new().unwrap();
            let app_name = "transition/production";
            let data = crate::release::ensure_app_runtime_data_dirs(dir.path(), app_name).unwrap();
            let release = crate::release::app_release_root(dir.path(), app_name, "compose");
            std::fs::create_dir_all(&release).unwrap();
            let config = ComposeConfig {
                file: "compose.yaml".into(),
                service: "web".into(),
                port: 8080,
                health_path: "/".into(),
                health_host: None,
            };
            std::fs::write(release.join("app.json"), json!({
                "protocol_version":tako_core::PROTOCOL_VERSION,
                "release_kind":"compose", "app_name":"transition", "environment":"production",
                "version":"compose", "runtime":"compose", "main":"", "idle_timeout":300, "compose":config,
            }).to_string()).unwrap();
            std::fs::write(release.join("compose.yaml"), r#"
services:
  web:
    image: docker.io/library/busybox:1.37
    command: [sh, -c, 'mkdir -p /www; echo compose > /www/index.html; exec httpd -f -p 8080 -h /www']
  worker:
    image: docker.io/library/busybox:1.37
    command: [sleep, '3600']
    volumes: [worker-data:/data]
volumes:
  worker-data:
"#).unwrap();
            let runtime = ComposeRuntime::new(
                release.clone(),
                config,
                HashMap::from([(TAKO_APP_DATA_DIR_ENV.into(), data.app.display().to_string())]),
            )
            .unwrap();
            struct Cleanup(ComposeRuntime);
            impl Drop for Cleanup {
                fn drop(&mut self) {
                    let _ = self
                        .0
                        .command(false)
                        .into_std()
                        .args(["down", "--volumes", "--remove-orphans"])
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status();
                }
            }
            let _cleanup = Cleanup(runtime.clone());
            runtime.prepare("compose").await.unwrap();
            let state = crate::ServerState::new(
                dir.path().into(),
                Arc::new(crate::tls::CertManager::new(
                    crate::tls::CertManagerConfig {
                        cert_dir: dir.path().join("certs"),
                        ..Default::default()
                    },
                )),
                None,
                Arc::new(parking_lot::RwLock::new(HashMap::new())),
            )
            .unwrap();
            let deploy = |version: &str| -> Command {
                serde_json::from_value(json!({
                    "command":"deploy", "app":app_name, "version":version,
                    "path":crate::release::app_release_root(dir.path(), app_name, version),
                    "routes":["transition.localhost"],
                }))
                .unwrap()
            };
            assert!(matches!(
                state.handle_command(deploy("compose")).await,
                Response::Ok { .. }
            ));
            let app = state.app_manager.get_app(app_name).unwrap();
            let initial_worker = runtime
                .containers()
                .await
                .unwrap()
                .into_iter()
                .find(|container| container.service == "worker")
                .unwrap();

            // A native process that exits before readiness must restore the old
            // stack, including the worker that was stopped for the transition.
            for version in ["broken", "native"] {
                let native = crate::release::app_release_root(dir.path(), app_name, version);
                std::fs::create_dir_all(&native).unwrap();
                std::fs::write(
                    native.join("app.json"),
                    json!({
                        "protocol_version":tako_core::PROTOCOL_VERSION,
                        "app_name":"transition", "environment":"production", "version":version,
                        "runtime":"go", "main":"server.py", "idle_timeout":300,
                        "start":["/usr/bin/python3", "{main}"],
                    })
                    .to_string(),
                )
                .unwrap();
                std::fs::write(
                    native.join("server.py"),
                    if version == "broken" {
                        "raise SystemExit(1)\n"
                    } else {
                        r#"import http.server, json, os
with os.fdopen(3) as pipe:
    token = json.load(pipe)['token']
class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.send_header('x-tako-internal-token', token)
        self.end_headers()
        self.wfile.write(b'native')
    def log_message(self, *args):
        pass
server = http.server.HTTPServer(('127.0.0.1', 0), Handler)
with os.fdopen(4, 'w') as pipe:
    pipe.write(str(server.server_port) + '\n')
server.serve_forever()
"#
                    },
                )
                .unwrap();
            }
            assert!(matches!(
                state.handle_command(deploy("broken")).await,
                Response::Error { .. }
            ));
            assert!(app.config.read().compose.is_some());
            assert_eq!(app.config.read().version, "compose");
            let restored = runtime.containers().await.unwrap();
            assert_eq!(restored.len(), 2);
            assert!(restored.iter().all(|container| container.running()));
            let restored_worker = restored
                .iter()
                .find(|container| container.service == "worker")
                .unwrap();
            assert_ne!(restored_worker.started_at, initial_worker.started_at);

            if missing_http_instance {
                for instance in app.get_instances() {
                    instance.kill().await.unwrap();
                    app.remove_instance(&instance.id);
                }
                assert!(app.get_instances().is_empty());
                assert!(
                    runtime
                        .containers()
                        .await
                        .unwrap()
                        .iter()
                        .any(|container| container.service == "worker" && container.running())
                );
            }
            assert!(matches!(
                state.handle_command(deploy("native")).await,
                Response::Ok { .. }
            ));
            assert!(app.config.read().compose.is_none());
            assert_eq!(app.config.read().version, "native");
            let containers = runtime.containers().await.unwrap();
            assert_eq!(containers.len(), 2);
            assert!(containers.iter().all(|container| !container.running()));
            assert_eq!(app.get_instances().len(), 1);
            assert!(app.get_instances()[0].is_alive().await);
            let endpoint = app.get_instances()[0].endpoint().unwrap();
            let body = reqwest::Client::builder()
                .no_proxy()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .unwrap()
                .get(format!("http://{endpoint}/"))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap();
            assert_eq!(body, "native");
            state.app_manager.stop_app(app_name).await.unwrap();
        }
    }
}
