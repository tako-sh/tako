use super::*;
use crate::instances::AppConfig;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Serialize, Deserialize)]
struct App {
    name: String,
    environment: String,
    version: String,
    min_instances: u32,
    max_instances: u32,
    source_ip: tako_core::SourceIpMode,
    routes: Vec<String>,
    compose_replicas: BTreeMap<String, u32>,
    stopped: bool,
    compose_stopped: BTreeSet<String>,
}

pub(crate) fn load_persisted_releases_read_only(
    path: &Path,
) -> Result<Vec<PersistedRelease>, StateStoreError> {
    if !path.try_exists().map_err(storage)? {
        return Ok(Vec::new());
    }
    inspect(path, |rows| {
        scan(rows, "apps", |id, _, value| {
            let app: App = serde_json::from_slice(value)?;
            Ok(PersistedRelease {
                app_id: id.to_owned(),
                version: app.version,
            })
        })
    })
}

impl StateStore {
    pub fn upsert_app(&self, config: &AppConfig, routes: &[String]) -> Result<(), StateStoreError> {
        self.write(|rows| upsert_app_on(rows, config, routes))
    }
    pub fn delete_app(&self, name: &str, environment: &str) -> Result<(), StateStoreError> {
        let id = if environment.is_empty() {
            name.to_owned()
        } else {
            tako_core::deployment_app_id(name, environment)
        };
        self.write(|rows| {
            for bucket in [
                "apps",
                "app_secrets",
                "app_compose_tokens",
                "app_schedule_revisions",
                "app_storages",
                "app_databases",
                "app_ssl",
                "app_compose_pending",
                "app_release_routing",
                "release_databases",
            ] {
                let end = format!("{id}\0");
                rows.retain_in(
                    (bucket, id.as_str(), "")..(bucket, end.as_str(), ""),
                    |_, _| false,
                )
                .map_err(storage)?;
            }
            Ok(())
        })
    }
    pub fn load_apps(&self) -> Result<Vec<PersistedApp>, StateStoreError> {
        self.read(|rows| {
            scan(rows, "apps", |_, _, value| {
                let app: App = serde_json::from_slice(value)?;
                Ok(PersistedApp {
                    routes: app.routes,
                    config: AppConfig {
                        name: app.name,
                        environment: app.environment,
                        version: app.version,
                        source_ip: app.source_ip,
                        min_instances: app.min_instances,
                        max_instances: app.max_instances,
                        compose_replicas: app.compose_replicas,
                        stopped: app.stopped,
                        compose_stopped: app.compose_stopped,
                        ..Default::default()
                    },
                })
            })
        })
    }
}
pub(super) fn upsert_app_on(
    rows: &mut Rows<'_>,
    config: &AppConfig,
    routes: &[String],
) -> Result<(), StateStoreError> {
    let app = config.deployment_id();
    let mut sorted = routes.to_vec();
    sorted.sort();
    if sorted.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(StateStoreError::InvalidData("duplicate route".into()));
    }
    put(
        rows,
        ("apps", &app, ""),
        &App {
            name: config.name.clone(),
            environment: config.environment.clone(),
            version: config.version.clone(),
            min_instances: config.min_instances,
            max_instances: config.max_instances,
            source_ip: config.source_ip,
            routes: sorted,
            compose_replicas: config.compose_replicas.clone(),
            stopped: config.stopped,
            compose_stopped: config.compose_stopped.clone(),
        },
    )?;
    put(
        rows,
        ("app_release_routing", &app, &config.version),
        &ReleaseRouting::new(
            routes.to_vec(),
            config.source_ip,
            config.compose.as_ref(),
            &config.environment,
        ),
    )
}
