use tako_core::BackupInfo;

use crate::release::requested_deployment_identity;
use object_store::{ObjectStoreExt, path::Path as ObjectPath};

use super::validate_backup_binding;

const BACKUP_INDEX_VERSION: u8 = 1;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct BackupIndex {
    version: u8,
    pub(super) backups: Vec<BackupInfo>,
    /// Deletion intent is persisted before touching either remote object.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) pending_deletions: Vec<BackupInfo>,
}

impl Default for BackupIndex {
    fn default() -> Self {
        Self {
            version: BACKUP_INDEX_VERSION,
            backups: Vec::new(),
            pending_deletions: Vec::new(),
        }
    }
}

impl crate::ServerState {
    pub(super) async fn read_backup_index(
        &self,
        backup: &tako_core::BackupBinding,
        app: &str,
    ) -> Result<BackupIndex, String> {
        validate_backup_binding(backup)?;
        super::objects::with_deadline(super::objects::METADATA_TIMEOUT, async {
            let key = backup_index_key(app, self);
            let store = super::objects::backup_store(&backup.storage)?;
            let key = ObjectPath::parse(key).map_err(|e| e.to_string())?;
            let result = match store.get(&key).await {
                Ok(result) => result,
                Err(object_store::Error::NotFound { .. }) => return Ok(BackupIndex::default()),
                Err(error) => return Err(format!("read backup index: {error}")),
            };
            let bytes = result
                .bytes()
                .await
                .map_err(|e| format!("read backup index body: {e}"))?;
            serde_json::from_slice(&bytes).map_err(|e| format!("parse backup index: {e}"))
        })
        .await
    }

    pub(super) fn backup_server_label(&self) -> String {
        let raw = self
            .runtime
            .server_name
            .as_deref()
            .or(self.runtime.server_identity.as_deref())
            .unwrap_or("server");
        sanitize_key_segment(raw)
    }
}

pub(super) fn backup_index_key(app: &str, state: &crate::ServerState) -> String {
    let (app_name, environment) = requested_deployment_identity(app);
    format!(
        "{}/index.json",
        backup_object_prefix(&app_name, &environment, &state.backup_server_label())
    )
}

pub(super) fn backup_object_prefix(app: &str, environment: &str, server: &str) -> String {
    format!(
        "_tako/backups/{}/{}/{}",
        sanitize_key_segment(app),
        sanitize_key_segment(environment),
        sanitize_key_segment(server)
    )
}

pub(super) fn latest_backup<'a>(
    backups: impl Iterator<Item = &'a BackupInfo>,
) -> Option<&'a BackupInfo> {
    backups.max_by(|a, b| {
        a.created_at_unix_secs
            .cmp(&b.created_at_unix_secs)
            .then_with(|| a.id.cmp(&b.id))
    })
}

fn sanitize_key_segment(value: &str) -> String {
    let mut out = String::new();
    for ch in value.trim().chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch.to_ascii_lowercase());
        } else if ch == '.' {
            out.push('-');
        }
    }
    if out.is_empty() {
        "server".to_string()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_prefix_includes_app_env_and_server() {
        assert_eq!(
            backup_object_prefix("demo", "production", "la.1"),
            "_tako/backups/demo/production/la-1"
        );
    }
}
