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
            "{prepared:?}"
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
}
