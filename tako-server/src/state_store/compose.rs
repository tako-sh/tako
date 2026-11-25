use super::{StateStore, StateStoreError, app_registry::upsert_app_on, encryption};
use super::{get, put, remove, scan, storage};
use crate::instances::AppConfig;

impl StateStore {
    /// The schedule revision of the app's committed release, replaced when a
    /// failed deployment restores that release's runtime.
    pub(crate) fn set_workflow_schedule_revision(
        &self,
        app: &str,
        revision: i64,
    ) -> Result<(), StateStoreError> {
        self.write(|r| put(r, ("app_schedule_revisions", app, ""), &revision))
    }
    pub(crate) fn workflow_schedule_revision(
        &self,
        app: &str,
    ) -> Result<Option<i64>, StateStoreError> {
        self.read(|r| get(r, ("app_schedule_revisions", app, "")))
    }
    pub(crate) fn begin_compose_deployment(
        &self,
        app: &str,
        project: &str,
    ) -> Result<(), StateStoreError> {
        self.write(|r| put(r, ("app_compose_pending", app, ""), &project))
    }
    pub(crate) fn pending_compose_deployments(
        &self,
    ) -> Result<Vec<(String, String)>, StateStoreError> {
        self.read(|r| {
            scan(r, "app_compose_pending", |app, _, v| {
                Ok((app.to_owned(), serde_json::from_slice(v)?))
            })
        })
    }
    pub(crate) fn clear_compose_deployment(&self, app: &str) -> Result<(), StateStoreError> {
        self.write(|r| remove(r, ("app_compose_pending", app, "")))
    }
    /// The deployment's single durable commit point. Until this commits,
    /// recovery sees the previous release AND its previous credentials.
    pub(crate) fn commit_deployment(
        &self,
        config: &AppConfig,
        routes: &[String],
        ssl: Option<&tako_core::SslBinding>,
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
                "app_databases",
                Some(encode(serde_json::json!(config.databases))?),
            ),
            (
                "app_ssl",
                ssl.map(|value| encode(serde_json::json!(value)))
                    .transpose()?,
            ),
        ];
        // A group keeps its token across releases, including ones without SDK
        // services, so containers it started never stop matching.
        let compose_token = config
            .compose_token
            .as_ref()
            .map(|token| encryption::encrypt_blob(&self.encryption_key, token.as_bytes()))
            .transpose()?;
        let app = config.deployment_id();
        self.write(|r| {
            upsert_app_on(r, config, routes)?;
            match schedule_revision {
                Some(rev) => put(r, ("app_schedule_revisions", &app, ""), &rev)?,
                None => remove(r, ("app_schedule_revisions", &app, ""))?,
            }
            for (bucket, blob) in blobs {
                if let Some(blob) = blob {
                    if bucket == "app_databases" {
                        r.insert(
                            ("release_databases", app.as_str(), config.version.as_str()),
                            blob.as_slice(),
                        )
                        .map_err(storage)?;
                    }
                    r.insert((bucket, app.as_str(), ""), blob.as_slice())
                        .map_err(storage)?;
                } else {
                    remove(r, (bucket, &app, ""))?;
                }
            }
            if let Some(token) = compose_token {
                r.insert(("app_compose_tokens", app.as_str(), ""), token.as_slice())
                    .map_err(storage)?;
            }
            remove(r, ("app_compose_pending", &app, ""))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_token_commits_encrypted_survives_restarts_and_is_deleted_with_the_app() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let store = StateStore::new(path.clone(), [7; 32]);
        store.init().unwrap();
        let mut config = AppConfig {
            name: "app".into(),
            environment: "production".into(),
            version: "v1".into(),
            compose_token: Some("group-token-value".into()),
            ..Default::default()
        };
        let commit = |store: &StateStore, config: &AppConfig| {
            store.commit_deployment(config, &[], None, None).unwrap()
        };
        commit(&store, &config);
        let raw = store
            .read(|r| super::super::bytes(r, ("app_compose_tokens", "app/production", "")))
            .unwrap()
            .unwrap();
        assert!(!raw.windows(17).any(|w| w == b"group-token-value"));
        // A release without SDK services keeps the group's token.
        config.compose_token = None;
        config.version = "v2".into();
        commit(&store, &config);
        let reopened = StateStore::new(path, [7; 32]);
        reopened.init().unwrap();
        assert_eq!(
            reopened
                .get_compose_token("app/production")
                .unwrap()
                .as_deref(),
            Some("group-token-value")
        );
        reopened.delete_app("app", "production").unwrap();
        assert_eq!(reopened.get_compose_token("app/production").unwrap(), None);
    }

    #[test]
    fn release_routes_credentials_and_pending_marker_commit_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().join("state.db"), [7; 32]);
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
        config.compose_stopped.insert("web".into());
        store
            .commit_deployment(&config, &["old.example".into()], None, Some(1))
            .unwrap();
        store
            .begin_compose_deployment("app/production", "tako-test")
            .unwrap();
        config.version = "v2".into();
        config.compose_replicas.remove("web");
        config.compose_replicas.insert("worker".into(), 3);
        config.stopped = true;
        config.compose_stopped.clear();
        config.secrets.insert("TOKEN".into(), "next".into());
        // A late SQL failure must roll back the app row, routes, and every secret
        // write, leaving the marker for recovery.
        store
            .reject_writes
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(
            store
                .commit_deployment(&config, &["new.example".into()], None, Some(2),)
                .is_err()
        );
        let persisted = store.load_apps().unwrap();
        assert!(!persisted[0].config.stopped);
        assert!(persisted[0].config.compose_stopped.contains("web"));
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
            .reject_writes
            .store(false, std::sync::atomic::Ordering::SeqCst);
        store
            .commit_deployment(&config, &["new.example".into()], None, Some(2))
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
}
