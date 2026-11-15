use std::collections::BTreeMap;

use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use tako_core::{SourceIpMode, compose::ComposeConfig};

use super::{SqliteStateStore, StateStoreError};

/// Server-owned bindings committed with a release. Uploaded manifests cannot
/// grant route ownership or change the source-IP trust policy of an old release.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ReleaseRouting {
    pub routes: Vec<String>,
    pub source_ip: SourceIpMode,
    pub source_ips: BTreeMap<String, SourceIpMode>,
}

impl ReleaseRouting {
    pub(crate) fn new(
        routes: Vec<String>,
        source_ip: SourceIpMode,
        compose: Option<&ComposeConfig>,
        environment: &str,
    ) -> Self {
        let mut source_ips: BTreeMap<_, _> = routes
            .iter()
            .map(|route| (route.clone(), source_ip))
            .collect();
        if let Some(compose) = compose {
            for (_, settings) in compose.routed_services(environment) {
                for route in settings.routes() {
                    source_ips.insert(route, settings.source_ip.unwrap_or(source_ip));
                }
            }
        }
        Self {
            routes,
            source_ip,
            source_ips,
        }
    }
}

impl SqliteStateStore {
    pub(crate) fn release_routing(
        &self,
        app: &str,
        version: &str,
    ) -> Result<Option<ReleaseRouting>, StateStoreError> {
        self.lock_conn()?
            .query_row(
                "SELECT routing FROM app_release_routing WHERE app = ?1 AND version = ?2",
                (app, version),
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|value| {
                serde_json::from_str(&value)
                    .map_err(|error| StateStoreError::InvalidData(error.to_string()))
            })
            .transpose()
    }
}

pub(super) fn save_on(
    conn: &rusqlite::Connection,
    app: &str,
    version: &str,
    routing: &ReleaseRouting,
) -> Result<(), StateStoreError> {
    let json = serde_json::to_string(routing)
        .map_err(|error| StateStoreError::InvalidData(error.to_string()))?;
    conn.execute(
        "INSERT INTO app_release_routing (app, version, routing) VALUES (?1, ?2, ?3)
         ON CONFLICT(app, version) DO UPDATE SET routing = excluded.routing",
        (app, version, json),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instances::AppConfig;

    #[test]
    fn release_bindings_survive_private_deploy_restart_and_failed_commit() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteStateStore::new(dir.path().join("state.sqlite"), [0; 32]);
        store.init().unwrap();
        let public = AppConfig {
            name: "demo".into(),
            environment: "production".into(),
            version: "public".into(),
            source_ip: SourceIpMode::Direct,
            ..Default::default()
        };
        store.upsert_app(&public, &["demo.example".into()]).unwrap();
        // Simulate a native release persisted before this table existed.
        store
            .lock_conn()
            .unwrap()
            .execute("DELETE FROM app_release_routing", [])
            .unwrap();
        let private = AppConfig {
            version: "private".into(),
            ..public.clone()
        };
        store
            .commit_deployment(&private, &[], &Default::default(), None, None, None)
            .unwrap();
        let expected = ReleaseRouting::new(
            vec!["demo.example".into()],
            SourceIpMode::Direct,
            None,
            "production",
        );
        assert_eq!(
            store.release_routing("demo/production", "public").unwrap(),
            Some(expected.clone())
        );
        drop(store);
        let store = SqliteStateStore::new(dir.path().join("state.sqlite"), [0; 32]);
        store.init().unwrap();
        store.lock_conn().unwrap().execute_batch("CREATE TRIGGER fail_commit BEFORE UPDATE ON apps BEGIN SELECT RAISE(ABORT, 'injected'); END;").unwrap();
        assert!(
            store
                .commit_deployment(
                    &public,
                    &["changed.example".into()],
                    &Default::default(),
                    None,
                    None,
                    None
                )
                .is_err()
        );
        assert_eq!(
            store.release_routing("demo/production", "public").unwrap(),
            Some(expected)
        );
        assert!(
            store
                .release_routing("demo/production", "private")
                .unwrap()
                .unwrap()
                .routes
                .is_empty()
        );
        store.delete_app("demo", "production").unwrap();
        assert!(
            store
                .release_routing("demo/production", "public")
                .unwrap()
                .is_none()
        );
    }
}
