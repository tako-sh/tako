//! An app environment's secret snapshot, as this server applies it to a deploy.
//!
//! Clients never send secret values. A deploy pins the environment's snapshot
//! by digest, and every target server opens the same record versions with its
//! own key grants through the local network agent.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::Duration;
use tokio::time::Instant;

use serde::Deserialize;
use tako_core::{BackupBinding, BackupKeyBinding, PublishedSecret, SslBinding, StorageBinding};
use tako_network::shared::{PublishedVersion, RecordKind, ScopeSecrets};

/// How long a server waits for a pinned snapshot to replicate to it.
const WAIT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(250);

pub(crate) const SSL_CLOUDFLARE: &str = "ssl.cloudflare";
pub(crate) const POSTGRES_URL: &str = "postgres_url";

/// An app environment's values as one deploy applies them.
pub(crate) struct Resolved {
    pub(crate) digest: String,
    pub(crate) app: HashMap<String, String>,
    credentials: HashMap<String, String>,
    /// Storage resource name to its access key ID and secret access key.
    storages: HashMap<String, (String, String)>,
    /// Older keys first; the active key last.
    backup_keys: Vec<BackupKeyBinding>,
    /// Values past their expiry, in snapshot order, with the refusal to
    /// report: the storage resource for storage credentials, `None` for app
    /// secrets and provider credentials. Each operation refuses only the ones
    /// it would apply.
    expired: Vec<(Option<String>, String)>,
    env: String,
}

impl std::fmt::Debug for Resolved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resolved")
            .field("digest", &self.digest)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct StoredCredential {
    value: String,
    #[serde(default)]
    expires_on: Option<String>,
}

#[derive(Deserialize)]
struct StoredStorageCredentials {
    access_key_id: StoredCredential,
    secret_access_key: StoredCredential,
}

impl Resolved {
    fn from_scope(app_id: &str, scope: ScopeSecrets) -> Result<Self, String> {
        let env = tako_core::split_deployment_app_id(app_id)
            .map(|(_, env)| env.to_string())
            .unwrap_or_default();
        let today = today();
        let mut app = HashMap::new();
        let mut credentials = HashMap::new();
        let mut storages = HashMap::new();
        let mut keys = BTreeMap::new();
        let mut active = None;
        let mut expired = Vec::new();
        for value in scope.values {
            let content = value.content;
            if matches!(value.kind, RecordKind::App | RecordKind::Credential)
                && let Some(expires_on) = &content.expires_on
                && expires_on.as_str() <= today.as_str()
            {
                let command = if value.kind == RecordKind::App {
                    "secrets set"
                } else {
                    "credentials set"
                };
                expired.push((None, format!(
                    "Secret '{}' in environment '{env}' expired on {expires_on}. Run `tako {command} {} --env {env}` to update it.",
                    value.name, value.name
                )));
            }
            let text = content.value.to_string();
            match value.kind {
                RecordKind::App => {
                    app.insert(value.name, text);
                }
                RecordKind::Credential => {
                    credentials.insert(value.name, text);
                }
                RecordKind::Storage => {
                    let stored: StoredStorageCredentials =
                        serde_json::from_str(&text).map_err(|_| {
                            format!("Storage credentials for '{}' are malformed.", value.name)
                        })?;
                    // Each key of the pair carries its own expiry.
                    if let Some(expires_on) = [&stored.access_key_id, &stored.secret_access_key]
                        .into_iter()
                        .filter_map(|credential| credential.expires_on.as_deref())
                        .filter(|expires_on| *expires_on <= today.as_str())
                        .min()
                    {
                        expired.push((Some(value.name.clone()), format!(
                            "Storage credentials for '{}' in environment '{env}' expired on {expires_on}. Run `tako storages credentials {} --env {env}` to update them.",
                            value.name, value.name
                        )));
                    }
                    storages.insert(
                        value.name,
                        (stored.access_key_id.value, stored.secret_access_key.value),
                    );
                }
                RecordKind::BackupKey => {
                    keys.insert(value.name, text);
                }
                RecordKind::BackupActive => active = Some(text),
                RecordKind::Database | RecordKind::DatabaseKey => {}
            }
        }
        let mut backup_keys: Vec<_> = keys
            .iter()
            .filter(|(id, _)| Some(*id) != active.as_ref())
            .map(|(id, key)| BackupKeyBinding {
                id: id.clone(),
                key_base64: key.clone(),
            })
            .collect();
        if let Some(active) = &active
            && let Some(key) = keys.get(active)
        {
            backup_keys.push(BackupKeyBinding {
                id: active.clone(),
                key_base64: key.clone(),
            });
        }
        Ok(Self {
            digest: scope.digest,
            app,
            credentials,
            storages,
            backup_keys,
            expired,
            env,
        })
    }

    /// Refuse a deploy or release while any app secret, provider credential,
    /// or storage credential of the snapshot has expired.
    pub(crate) fn refuse_expired(&self) -> Result<(), String> {
        match self.expired.first() {
            Some((_, refusal)) => Err(refusal.clone()),
            None => Ok(()),
        }
    }

    /// The credentials the server itself uses at runtime. Never passed to app
    /// code.
    pub(crate) fn runtime_credentials(&self) -> HashMap<String, String> {
        self.credentials
            .get(POSTGRES_URL)
            .map(|url| HashMap::from([(POSTGRES_URL.to_string(), url.clone())]))
            .unwrap_or_default()
    }

    /// Supply the Cloudflare token when the environment has one.
    pub(crate) fn fill_ssl(&self, ssl: &mut SslBinding) {
        if let Some(token) = self.credentials.get(SSL_CLOUDFLARE) {
            ssl.cloudflare_api_token = Some(token.clone());
        }
    }

    /// Supply an S3 binding's access keys from its storage resource.
    pub(crate) fn fill_storage(
        &self,
        name: &str,
        binding: &mut StorageBinding,
    ) -> Result<(), String> {
        if binding.provider != tako_core::StorageProvider::S3 {
            return Ok(());
        }
        let resource = binding
            .resource
            .as_deref()
            .ok_or_else(|| format!("Storage binding '{name}' names no storage resource."))?;
        // Expired keys are never handed out, whatever the operation.
        if let Some((_, refusal)) = self
            .expired
            .iter()
            .find(|(expired, _)| expired.as_deref() == Some(resource))
        {
            return Err(refusal.clone());
        }
        let (access_key_id, secret_access_key) =
            self.storages.get(resource).ok_or_else(|| {
                format!(
                    "Missing storage credentials for resource '{resource}' in environment '{}'. Run `tako storages credentials {resource} --env {}`.",
                    self.env, self.env
                )
            })?;
        binding.access_key_id = Some(access_key_id.clone());
        binding.secret_access_key = Some(secret_access_key.clone());
        Ok(())
    }

    pub(crate) fn fill_storages(
        &self,
        storages: &mut HashMap<String, StorageBinding>,
    ) -> Result<(), String> {
        for (name, binding) in storages {
            self.fill_storage(name, binding)?;
        }
        Ok(())
    }

    /// Supply a backup target's access keys and backup encryption keys.
    pub(crate) fn fill_backup(&self, backup: &mut BackupBinding) -> Result<(), String> {
        self.fill_storage("backup", &mut backup.storage)?;
        if self.backup_keys.is_empty() {
            return Err(format!(
                "Backup encryption key is missing for environment '{}'. Deploy from a full client or run `tako backups now` to create it.",
                self.env
            ));
        }
        backup.backup_keys = self.backup_keys.clone();
        Ok(())
    }
}

/// Converts a published version from the wire into the network's form.
pub(crate) fn published_versions(
    published: &[PublishedSecret],
) -> Result<Vec<PublishedVersion>, String> {
    published
        .iter()
        .map(|version| {
            let kind: RecordKind =
                serde_json::from_value(serde_json::Value::String(version.kind.clone()))
                    .map_err(|_| format!("Unknown secret record kind '{}'.", version.kind))?;
            Ok(PublishedVersion {
                kind,
                name: version.name.clone(),
                revision: version.revision,
            })
        })
        .collect()
}

fn today() -> String {
    let date = time::OffsetDateTime::now_utc().date();
    format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        u8::from(date.month()),
        date.day()
    )
}

/// Read `app_id`'s snapshot. With `expected`, wait until this server holds
/// exactly that snapshot; with `published`, wait until it holds those
/// versions. Either way wait for every key grant.
pub(crate) async fn resolve(
    data_dir: &Path,
    app_id: &str,
    expected: Option<&str>,
    published: &[PublishedSecret],
) -> Result<Resolved, String> {
    let scope: tako_network::AppEnv = app_id
        .parse()
        .map_err(|_| format!("'{app_id}' is not an app environment."))?;
    let published = published_versions(published)?;
    let deadline = Instant::now() + WAIT;
    loop {
        let snapshot = read(data_dir, &scope, &published).await?;
        let pinned = expected.is_none_or(|digest| digest == snapshot.digest);
        if snapshot.caught_up && snapshot.missing_keys.is_empty() && pinned {
            return Resolved::from_scope(app_id, snapshot);
        }
        if Instant::now() >= deadline {
            return Err(if !snapshot.caught_up {
                "This server has not received the latest published secrets yet. Run the command again in a moment.".to_string()
            } else if !snapshot.missing_keys.is_empty() {
                format!(
                    "This server has not received the keys for {} yet. Run the command again in a moment.",
                    snapshot.missing_keys.join(", ")
                )
            } else {
                "This app environment's secrets changed while the command ran, or have not reached this server yet. Run the command again.".to_string()
            });
        }
        tokio::time::sleep(POLL).await;
    }
}

async fn read(
    data_dir: &Path,
    scope: &tako_network::AppEnv,
    published: &[PublishedVersion],
) -> Result<ScopeSecrets, String> {
    #[cfg(debug_assertions)]
    if let Some(snapshot) = test_file::read(data_dir, scope)? {
        return Ok(snapshot);
    }
    crate::native_agent::scope_secrets(data_dir, scope.clone(), published.to_vec())
        .await
        .map_err(|error| format!("Could not read this app environment's secrets: {error}"))
}

/// Debug builds only: tests that run without a network agent put an app
/// environment's values in `<data dir>/test-secrets.json`, keyed by app ID.
/// Release builds never read this file.
#[cfg(debug_assertions)]
pub(crate) mod test_file {
    use super::*;
    use serde::Serialize;

    pub(crate) const NAME: &str = "test-secrets.json";

    #[derive(Default, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(crate) struct TestScope {
        #[serde(default)]
        pub(crate) app: BTreeMap<String, String>,
        #[serde(default)]
        pub(crate) credentials: BTreeMap<String, String>,
        #[serde(default)]
        pub(crate) storages: BTreeMap<String, (String, String)>,
        #[serde(default)]
        pub(crate) backup_keys: Vec<(String, String)>,
    }

    /// The digest servers report for `scope` read from the test file.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn digest(scope: &TestScope) -> String {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(
            serde_json::to_vec(scope).expect("serialize test secrets"),
        ))
    }

    /// Write one app environment's values and return its digest.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn pin(data_dir: &Path, app_id: &str, scope: TestScope) -> String {
        let path = data_dir.join(NAME);
        let mut scopes: BTreeMap<String, TestScope> = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let digest = digest(&scope);
        scopes.insert(app_id.to_string(), scope);
        write(data_dir, &scopes);
        digest
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn write(data_dir: &Path, scopes: &BTreeMap<String, TestScope>) {
        std::fs::write(
            data_dir.join(NAME),
            serde_json::to_vec(scopes).expect("serialize test secrets"),
        )
        .expect("write test secrets");
    }

    pub(super) fn read(
        data_dir: &Path,
        scope: &tako_network::AppEnv,
    ) -> Result<Option<ScopeSecrets>, String> {
        let path = data_dir.join(NAME);
        let Ok(bytes) = std::fs::read(&path) else {
            return Ok(None);
        };
        let mut scopes: BTreeMap<String, TestScope> =
            serde_json::from_slice(&bytes).map_err(|error| format!("{NAME}: {error}"))?;
        let entry = scopes.remove(scope.as_str()).unwrap_or_default();
        let digest = digest(&entry);
        let mut values = Vec::new();
        let mut push = |kind, name: &str, value: String| {
            values.push(tako_network::shared::ScopeValue {
                kind,
                name: name.to_string(),
                content: tako_network::shared::RecordContent {
                    value: value.into(),
                    expires_on: None,
                },
            });
        };
        for (name, value) in &entry.app {
            push(RecordKind::App, name, value.clone());
        }
        for (name, value) in &entry.credentials {
            push(RecordKind::Credential, name, value.clone());
        }
        for (name, (access, secret)) in &entry.storages {
            let stored = serde_json::json!({
                "access_key_id": {"value": access},
                "secret_access_key": {"value": secret},
            });
            push(RecordKind::Storage, name, stored.to_string());
        }
        for (id, key) in &entry.backup_keys {
            push(RecordKind::BackupKey, id, key.clone());
        }
        if let Some((active, _)) = entry.backup_keys.last() {
            push(RecordKind::BackupActive, "active", active.clone());
        }
        Ok(Some(ScopeSecrets {
            digest,
            caught_up: true,
            missing_keys: Vec::new(),
            values,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::test_file::{TestScope, pin};
    use super::*;

    const APP: &str = "shop/production";

    fn s3(resource: &str) -> StorageBinding {
        StorageBinding {
            provider: tako_core::StorageProvider::S3,
            resource: Some(resource.into()),
            bucket: Some("bucket".into()),
            endpoint: Some("https://s3.example.test".into()),
            region: None,
            access_key_id: None,
            secret_access_key: None,
            force_path_style: false,
            public_base_url: None,
            path: None,
            signing_key: None,
        }
    }

    #[tokio::test]
    async fn resolves_values_and_fills_bindings_from_the_pinned_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let digest = pin(
            dir.path(),
            APP,
            TestScope {
                app: [("API_KEY".into(), "app-secret".into())].into(),
                credentials: [
                    (SSL_CLOUDFLARE.into(), "cf-token".into()),
                    (POSTGRES_URL.into(), "postgres://runtime".into()),
                ]
                .into(),
                storages: [("r2".into(), ("access".into(), "secret".into()))].into(),
                backup_keys: vec![
                    ("backup-key-new".into(), "new".into()),
                    ("backup-key-old".into(), "old".into()),
                ],
            },
        );
        let resolved = resolve(dir.path(), APP, Some(&digest), &[]).await.unwrap();
        assert_eq!(resolved.digest, digest);
        assert_eq!(resolved.app["API_KEY"], "app-secret");
        // App code never receives provider credentials, and the server keeps
        // only the one it uses at runtime.
        assert!(!resolved.app.contains_key(SSL_CLOUDFLARE));
        assert_eq!(
            resolved.runtime_credentials(),
            HashMap::from([(POSTGRES_URL.to_string(), "postgres://runtime".to_string())])
        );

        let mut ssl = SslBinding::default();
        resolved.fill_ssl(&mut ssl);
        assert_eq!(ssl.cloudflare_api_token.as_deref(), Some("cf-token"));

        let mut storages = HashMap::from([("uploads".to_string(), s3("r2"))]);
        resolved.fill_storages(&mut storages).unwrap();
        assert_eq!(storages["uploads"].access_key_id.as_deref(), Some("access"));
        assert_eq!(
            storages["uploads"].secret_access_key.as_deref(),
            Some("secret")
        );
        let mut missing = HashMap::from([("files".to_string(), s3("other"))]);
        assert!(
            resolved
                .fill_storages(&mut missing)
                .unwrap_err()
                .contains("tako storages credentials other --env production")
        );

        // The active key, the last one written, goes last.
        let mut backup = BackupBinding {
            storage: s3("r2"),
            backup_keys: Vec::new(),
            retention_days: 7,
        };
        resolved.fill_backup(&mut backup).unwrap();
        let ids: Vec<_> = backup
            .backup_keys
            .iter()
            .map(|key| key.id.as_str())
            .collect();
        assert_eq!(ids, ["backup-key-new", "backup-key-old"]);
        assert_eq!(backup.storage.access_key_id.as_deref(), Some("access"));
    }

    #[tokio::test]
    async fn a_snapshot_that_never_arrives_fails_and_expired_values_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        pin(dir.path(), APP, TestScope::default());
        tokio::time::pause();
        let error = resolve(dir.path(), APP, Some("another-digest"), &[])
            .await
            .unwrap_err();
        assert!(error.contains("Run the command again"), "{error}");

        let scope = ScopeSecrets {
            digest: "d".into(),
            caught_up: true,
            missing_keys: Vec::new(),
            values: vec![tako_network::shared::ScopeValue {
                kind: RecordKind::App,
                name: "OLD_KEY".into(),
                content: tako_network::shared::RecordContent {
                    value: "x".to_string().into(),
                    expires_on: Some("2000-01-01".into()),
                },
            }],
        };
        let error = Resolved::from_scope(APP, scope)
            .unwrap()
            .refuse_expired()
            .unwrap_err();
        assert!(
            error.contains("expired on 2000-01-01")
                && error.contains("tako secrets set OLD_KEY --env production"),
            "{error}"
        );

        // Storage credentials keep their expiry on each key of the pair.
        let storage = |secret_expires_on: Option<&str>| ScopeSecrets {
            digest: "d".into(),
            caught_up: true,
            missing_keys: Vec::new(),
            values: vec![tako_network::shared::ScopeValue {
                kind: RecordKind::Storage,
                name: "r2".into(),
                content: tako_network::shared::RecordContent {
                    value: serde_json::json!({
                        "access_key_id": {"value": "access", "expires_on": "2999-01-01"},
                        "secret_access_key": {"value": "secret", "expires_on": secret_expires_on},
                    })
                    .to_string()
                    .into(),
                    expires_on: None,
                },
            }],
        };
        let resolved = Resolved::from_scope(APP, storage(Some("2000-01-01"))).unwrap();
        let mut storages = HashMap::from([("uploads".to_string(), s3("r2"))]);
        for error in [
            resolved.refuse_expired().unwrap_err(),
            resolved.fill_storages(&mut storages).unwrap_err(),
        ] {
            assert!(
                error.contains("Storage credentials for 'r2'")
                    && error.contains("expired on 2000-01-01")
                    && error.contains("tako storages credentials r2 --env production"),
                "{error}"
            );
        }
        assert!(storages["uploads"].access_key_id.is_none());
        let resolved = Resolved::from_scope(APP, storage(None)).unwrap();
        assert!(resolved.refuse_expired().is_ok());
    }

    #[test]
    fn a_backup_needs_only_its_target_and_keys_to_be_current() {
        let value = |kind, name: &str, value: String, expires_on: Option<&str>| {
            tako_network::shared::ScopeValue {
                kind,
                name: name.into(),
                content: tako_network::shared::RecordContent {
                    value: value.into(),
                    expires_on: expires_on.map(Into::into),
                },
            }
        };
        let storage = |expires_on: &str| {
            serde_json::json!({
                "access_key_id": {"value": "access"},
                "secret_access_key": {"value": "secret", "expires_on": expires_on},
            })
            .to_string()
        };
        let scope = ScopeSecrets {
            digest: "d".into(),
            caught_up: true,
            missing_keys: Vec::new(),
            values: vec![
                value(RecordKind::App, "OLD_KEY", "x".into(), Some("2000-01-01")),
                value(
                    RecordKind::Credential,
                    POSTGRES_URL,
                    "postgres://old".into(),
                    Some("2000-01-01"),
                ),
                value(RecordKind::Storage, "uploads", storage("2000-01-01"), None),
                value(RecordKind::Storage, "backups", storage("2999-01-01"), None),
                value(RecordKind::BackupKey, "backup-key-1", "key".into(), None),
                value(
                    RecordKind::BackupActive,
                    "active",
                    "backup-key-1".into(),
                    None,
                ),
            ],
        };
        let resolved = Resolved::from_scope(APP, scope).unwrap();

        // A deploy still refuses the expired app secret.
        let error = resolved.refuse_expired().unwrap_err();
        assert!(error.contains("Secret 'OLD_KEY'"), "{error}");

        // A backup to a current target succeeds with an expired app secret,
        // credential, and unrelated storage resource.
        let mut backup = BackupBinding {
            storage: s3("backups"),
            backup_keys: Vec::new(),
            retention_days: 7,
        };
        resolved.fill_backup(&mut backup).unwrap();
        assert_eq!(backup.storage.secret_access_key.as_deref(), Some("secret"));
        assert_eq!(backup.backup_keys.len(), 1);

        // A backup to the expired target is refused.
        let mut backup = BackupBinding {
            storage: s3("uploads"),
            backup_keys: Vec::new(),
            retention_days: 7,
        };
        let error = resolved.fill_backup(&mut backup).unwrap_err();
        assert!(
            error.contains("Storage credentials for 'uploads'"),
            "{error}"
        );
    }

    #[test]
    fn published_versions_read_record_kinds() {
        let versions = published_versions(&[PublishedSecret {
            kind: "backup_key".into(),
            name: "backup-key-1".into(),
            revision: 3,
        }])
        .unwrap();
        assert_eq!(versions[0].kind, RecordKind::BackupKey);
        assert!(
            published_versions(&[PublishedSecret {
                kind: "unknown".into(),
                name: "x".into(),
                revision: 1,
            }])
            .is_err()
        );
    }
}
