use super::*;
use crate::config::{
    EncryptedBackupKey, EncryptedSecretValue, EncryptedStorageCredentials, EnvironmentSecrets,
};
use crate::crypto::{EncryptionKey, decrypt, encrypt};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(super) struct RowKey {
    pub kind: SecretKind,
    pub name: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Row {
    pub key: RowKey,
    pub value: String,
    pub expires_on: Option<String>,
}
pub(super) type Rows = BTreeMap<RowKey, Row>;

pub(super) fn rows(values: Option<&EnvironmentSecrets>) -> Result<Rows> {
    let mut rows = BTreeMap::new();
    let Some(values) = values else {
        return Ok(rows);
    };
    let mut add = |kind, name: &str, value: &str, expires_on: Option<String>| {
        let key = RowKey {
            kind,
            name: name.into(),
        };
        rows.insert(
            key.clone(),
            Row {
                key,
                value: value.into(),
                expires_on,
            },
        );
    };
    for (name, value) in &values.app {
        add(
            SecretKind::App,
            name,
            &value.value,
            value.expires_on.clone(),
        );
    }
    for (name, value) in &values.credentials {
        add(
            SecretKind::Credential,
            name,
            &value.value,
            value.expires_on.clone(),
        );
    }
    for (name, value) in &values.storages {
        add(
            SecretKind::Storage,
            name,
            &serde_json::to_string(value)?,
            None,
        );
    }
    for value in &values.backup_keys {
        add(SecretKind::BackupKey, &value.id, &value.value, None);
    }
    if let Some(active) = values.backup_keys.last() {
        add(SecretKind::BackupActive, "active", &active.id, None);
    }
    Ok(rows)
}

pub(super) fn environment(key_id: String, rows: &Rows) -> Result<EnvironmentSecrets> {
    let mut env = EnvironmentSecrets {
        key_id,
        ..Default::default()
    };
    let mut active = None;
    for row in rows.values() {
        let value = EncryptedSecretValue::new(row.value.clone(), row.expires_on.clone());
        match row.key.kind {
            SecretKind::App => {
                env.app.insert(row.key.name.clone(), value);
            }
            SecretKind::Credential => {
                env.credentials.insert(row.key.name.clone(), value);
            }
            SecretKind::Storage => {
                env.storages
                    .insert(row.key.name.clone(), serde_json::from_str(&row.value)?);
            }
            SecretKind::BackupKey => env.backup_keys.push(EncryptedBackupKey::new(
                row.key.name.clone(),
                row.value.clone(),
            )),
            SecretKind::BackupActive => active = Some(row.value.clone()),
        }
    }
    if let Some(active) = active {
        let index = env
            .backup_keys
            .iter()
            .position(|key| key.id == active)
            .ok_or(
                "Active backup key has not arrived; sync through the server that published it",
            )?;
        let key = env.backup_keys.remove(index);
        env.backup_keys.push(key);
    } else if !env.backup_keys.is_empty() {
        return Err(
            "Backup key selection has not arrived; sync through the server that published it"
                .into(),
        );
    }
    Ok(env)
}

pub(super) fn plaintext(row: &Row, key: &EncryptionKey) -> Result<SecretContent> {
    let value = match row.key.kind {
        SecretKind::BackupActive => row.value.clone(),
        SecretKind::Storage => {
            let mut credentials: EncryptedStorageCredentials = serde_json::from_str(&row.value)?;
            credentials.access_key_id.value = decrypt(&credentials.access_key_id.value, key)?;
            credentials.secret_access_key.value =
                decrypt(&credentials.secret_access_key.value, key)?;
            serde_json::to_string(&credentials)?
        }
        _ => decrypt(&row.value, key)?,
    };
    Ok(SecretContent {
        value: Zeroizing::new(value),
        expires_on: row.expires_on.clone(),
    })
}

pub(super) fn encrypted(
    key: RowKey,
    content: SecretContent,
    encryption: &EncryptionKey,
) -> Result<Row> {
    content.validate()?;
    let value = match key.kind {
        SecretKind::BackupActive => {
            crate::config::validate_backup_key_id(&content.value)?;
            content.value.to_string()
        }
        SecretKind::Storage => {
            let mut credentials: EncryptedStorageCredentials =
                serde_json::from_str(&content.value)?;
            credentials.access_key_id.value =
                encrypt(&credentials.access_key_id.value, encryption)?;
            credentials.secret_access_key.value =
                encrypt(&credentials.secret_access_key.value, encryption)?;
            serde_json::to_string(&credentials)?
        }
        _ => encrypt(&content.value, encryption)?,
    };
    Ok(Row {
        key,
        value,
        expires_on: content.expires_on,
    })
}
