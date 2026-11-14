use std::collections::HashMap;

use super::{SqliteStateStore, StateStoreError, app_registry::upsert_app_on, encryption};
use crate::instances::AppConfig;
use rusqlite::OptionalExtension;

impl SqliteStateStore {
    pub(crate) fn workflow_schedule_revision(
        &self,
        app: &str,
    ) -> Result<Option<i64>, StateStoreError> {
        Ok(self
            .lock_conn()?
            .query_row(
                "SELECT revision FROM app_schedule_revisions WHERE app = ?1",
                [app],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub(crate) fn begin_compose_deployment(
        &self,
        app: &str,
        project: &str,
    ) -> Result<(), StateStoreError> {
        self.lock_conn()?.execute(
            "INSERT INTO app_compose_pending (app, project) VALUES (?1, ?2)
             ON CONFLICT(app) DO UPDATE SET project = excluded.project;",
            (app, project),
        )?;
        Ok(())
    }

    pub(crate) fn pending_compose_deployments(
        &self,
    ) -> Result<Vec<(String, String)>, StateStoreError> {
        let conn = self.lock_conn()?;
        let mut statement =
            conn.prepare("SELECT app, project FROM app_compose_pending ORDER BY app;")?;
        Ok(statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?)
    }

    pub(crate) fn clear_compose_deployment(&self, app: &str) -> Result<(), StateStoreError> {
        self.lock_conn()?
            .execute("DELETE FROM app_compose_pending WHERE app = ?1;", [app])?;
        Ok(())
    }

    /// The deployment's single durable commit point. Until this commits,
    /// recovery sees the previous release AND its previous credentials.
    pub(crate) fn commit_deployment(
        &self,
        config: &AppConfig,
        routes: &[String],
        runtime_credentials: &HashMap<String, String>,
        ssl: Option<&tako_core::SslBinding>,
        backup: Option<&tako_core::BackupBinding>,
        schedule_revision: Option<i64>,
    ) -> Result<(), StateStoreError> {
        let encode = |value: serde_json::Value| -> Result<Vec<u8>, StateStoreError> {
            let bytes = serde_json::to_vec(&value)
                .map_err(|e| StateStoreError::InvalidData(e.to_string()))?;
            encryption::encrypt_blob(&self.encryption_key, &bytes)
        };
        let blobs = [
            (
                "app_secrets",
                Some(encode(serde_json::json!(config.secrets))?),
            ),
            (
                "app_storages",
                Some(encode(serde_json::json!(config.storages))?),
            ),
            (
                "app_runtime_credentials",
                Some(encode(serde_json::json!(runtime_credentials))?),
            ),
            (
                "app_ssl",
                ssl.map(|value| encode(serde_json::json!(value)))
                    .transpose()?,
            ),
            (
                "app_backups",
                backup
                    .map(|value| encode(serde_json::json!(value)))
                    .transpose()?,
            ),
        ];
        let app = config.deployment_id();
        let conn = self.lock_conn()?;
        let tx = conn.unchecked_transaction()?;
        upsert_app_on(&tx, config, routes)?;
        tx.execute("DELETE FROM app_schedule_revisions WHERE app = ?1", [&app])?;
        if let Some(revision) = schedule_revision {
            tx.execute(
                "INSERT INTO app_schedule_revisions (app, revision) VALUES (?1, ?2)",
                (&app, revision),
            )?;
        }
        for (table, blob) in blobs {
            match blob {
                Some(bytes) => {
                    tx.execute(&format!("INSERT INTO {table} (app, encrypted_data) VALUES (?1, ?2) ON CONFLICT(app) DO UPDATE SET encrypted_data = excluded.encrypted_data;"), (&app, bytes))?;
                }
                None => {
                    tx.execute(&format!("DELETE FROM {table} WHERE app = ?1;"), [&app])?;
                }
            }
        }
        tx.execute("DELETE FROM app_compose_pending WHERE app = ?1;", [&app])?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_routes_credentials_and_pending_marker_commit_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteStateStore::new(dir.path().join("state.db"), [7; 32]);
        store.init().unwrap();
        let mut config = AppConfig {
            name: "app".into(),
            environment: "production".into(),
            version: "v1".into(),
            ..Default::default()
        };
        config.secrets.insert("TOKEN".into(), "previous".into());
        config.compose_replicas.insert("web".into(), 5);
        config.compose_replicas.insert("worker".into(), 0);
        store
            .commit_deployment(
                &config,
                &["old.example".into()],
                &HashMap::new(),
                None,
                None,
                Some(1),
            )
            .unwrap();
        store
            .begin_compose_deployment("app/production", "tako-test")
            .unwrap();
        config.version = "v2".into();
        config.compose_replicas.remove("web");
        config.compose_replicas.insert("worker".into(), 3);
        config.secrets.insert("TOKEN".into(), "next".into());
        // A late SQL failure must roll back the app row, routes, and every secret
        // write, leaving the marker for recovery.
        store.lock_conn().unwrap().execute_batch("CREATE TRIGGER fail_commit BEFORE DELETE ON app_compose_pending BEGIN SELECT RAISE(ABORT, 'injected commit failure'); END;").unwrap();
        assert!(
            store
                .commit_deployment(
                    &config,
                    &["new.example".into()],
                    &HashMap::new(),
                    None,
                    None,
                    Some(2),
                )
                .is_err()
        );
        let persisted = store.load_apps().unwrap();
        assert_eq!(
            store.workflow_schedule_revision("app/production").unwrap(),
            Some(1)
        );
        assert_eq!(persisted[0].config.compose_replicas["web"], 5);
        assert_eq!(persisted[0].config.compose_replicas["worker"], 0);
        assert_eq!(persisted[0].config.version, "v1");
        assert_eq!(store.load_apps().unwrap()[0].routes, ["old.example"]);
        assert_eq!(
            store.get_secrets("app/production").unwrap()["TOKEN"],
            "previous"
        );
        assert_eq!(store.pending_compose_deployments().unwrap().len(), 1);
        store
            .lock_conn()
            .unwrap()
            .execute_batch("DROP TRIGGER fail_commit;")
            .unwrap();
        store
            .commit_deployment(
                &config,
                &["new.example".into()],
                &HashMap::new(),
                None,
                None,
                Some(2),
            )
            .unwrap();
        assert_eq!(store.load_apps().unwrap()[0].config.version, "v2");
        assert_eq!(
            store.workflow_schedule_revision("app/production").unwrap(),
            Some(2)
        );
        assert_eq!(
            store.load_apps().unwrap()[0].config.compose_replicas,
            config.compose_replicas
        );
        assert_eq!(
            store.get_secrets("app/production").unwrap()["TOKEN"],
            "next"
        );
        assert!(store.pending_compose_deployments().unwrap().is_empty());
        store.delete_app("app", "production").unwrap();
        assert_eq!(
            store.workflow_schedule_revision("app/production").unwrap(),
            None
        );
        config.compose_replicas.clear();
        store.upsert_app(&config, &[]).unwrap();
        assert!(
            store.load_apps().unwrap()[0]
                .config
                .compose_replicas
                .is_empty()
        );
    }

    #[test]
    fn upgrade_from_schema_nine_preserves_apps_without_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let store = SqliteStateStore::new(path.clone(), [7; 32]);
        store.init().unwrap();
        let config = AppConfig {
            name: "app".into(),
            environment: "production".into(),
            version: "v1".into(),
            ..Default::default()
        };
        store.upsert_app(&config, &["old.example".into()]).unwrap();
        store
            .lock_conn()
            .unwrap()
            .execute_batch("DROP TABLE app_compose_scales; PRAGMA user_version = 9;")
            .unwrap();
        drop(store);
        let store = SqliteStateStore::new(path, [7; 32]);
        store.init().unwrap();
        let apps = store.load_apps().unwrap();
        assert_eq!(apps[0].config.version, "v1");
        assert_eq!(apps[0].routes, ["old.example"]);
        assert!(apps[0].config.compose_replicas.is_empty());
    }
}
