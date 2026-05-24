use tako_core::BackupInfo;

use crate::object_storage::{S3Method, presign_s3_url};
use crate::release::requested_deployment_identity;

use super::{S3_URL_EXPIRES_SECONDS, validate_backup_binding};

const BACKUP_INDEX_VERSION: u8 = 1;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct BackupIndex {
    version: u8,
    pub(super) backups: Vec<BackupInfo>,
}

impl Default for BackupIndex {
    fn default() -> Self {
        Self {
            version: BACKUP_INDEX_VERSION,
            backups: Vec::new(),
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
        let key = backup_index_key(app, self);
        let url = presign_s3_url(&backup.storage, &key, S3Method::Get, S3_URL_EXPIRES_SECONDS)?;
        let response = reqwest::Client::new()
            .get(url)
            .send()
            .await
            .map_err(|e| format!("read backup index: {e}"))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(BackupIndex::default());
        }
        if !response.status().is_success() {
            return Err(format!("read backup index returned {}", response.status()));
        }
        response
            .json::<BackupIndex>()
            .await
            .map_err(|e| format!("parse backup index: {e}"))
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
