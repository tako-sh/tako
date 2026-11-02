use std::collections::HashMap;

use super::{SqliteStateStore, StateStoreError, app_registry::upsert_app_on, encryption};
use crate::instances::AppConfig;

impl SqliteStateStore {
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
    pub(crate) fn commit_compose_deployment(
        &self,
        config: &AppConfig,
        routes: &[String],
        runtime_credentials: &HashMap<String, String>,
        ssl: Option<&tako_core::SslBinding>,
        backup: Option<&tako_core::BackupBinding>,
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
        store
            .commit_compose_deployment(
                &config,
                &["old.example".into()],
                &HashMap::new(),
                None,
                None,
            )
            .unwrap();
        store
            .begin_compose_deployment("app/production", "tako-test")
            .unwrap();
        config.version = "v2".into();
        config.secrets.insert("TOKEN".into(), "next".into());
        // A late SQL failure must roll back the app row, routes, and every secret
        // write, leaving the marker for recovery.
        store.lock_conn().unwrap().execute_batch("CREATE TRIGGER fail_commit BEFORE DELETE ON app_compose_pending BEGIN SELECT RAISE(ABORT, 'injected commit failure'); END;").unwrap();
        assert!(
            store
                .commit_compose_deployment(
                    &config,
                    &["new.example".into()],
                    &HashMap::new(),
                    None,
                    None
                )
                .is_err()
        );
        assert_eq!(store.load_apps().unwrap()[0].config.version, "v1");
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
            .commit_compose_deployment(
                &config,
                &["new.example".into()],
                &HashMap::new(),
                None,
                None,
            )
            .unwrap();
        assert_eq!(store.load_apps().unwrap()[0].config.version, "v2");
        assert_eq!(
            store.get_secrets("app/production").unwrap()["TOKEN"],
            "next"
        );
        assert!(store.pending_compose_deployments().unwrap().is_empty());
    }
}
