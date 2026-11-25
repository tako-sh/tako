use super::*;
use crate::config::{EncryptedSecretValue, EncryptedStorageCredentials, EnvironmentSecrets};
use crate::crypto::{EncryptionKey, decrypt, encrypt};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(super) struct RowKey {
    pub kind: RecordKind,
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
            RecordKind::App,
            name,
            &value.value,
            value.expires_on.clone(),
        );
    }
    for (name, value) in &values.credentials {
        add(
            RecordKind::Credential,
            name,
            &value.value,
            value.expires_on.clone(),
        );
    }
    for (name, value) in &values.storages {
        add(
            RecordKind::Storage,
            name,
            &serde_json::to_string(value)?,
            None,
        );
    }
    Ok(rows)
}

pub(super) fn environment(key_id: String, rows: &Rows) -> Result<EnvironmentSecrets> {
    let mut env = EnvironmentSecrets {
        key_id,
        ..Default::default()
    };
    for row in rows.values() {
        let value = EncryptedSecretValue::new(row.value.clone(), row.expires_on.clone());
        match row.key.kind {
            RecordKind::App => {
                env.app.insert(row.key.name.clone(), value);
            }
            RecordKind::Credential => {
                env.credentials.insert(row.key.name.clone(), value);
            }
            RecordKind::Storage => {
                env.storages
                    .insert(row.key.name.clone(), serde_json::from_str(&row.value)?);
            }
            // The catalog owns database records.
            RecordKind::Database | RecordKind::DatabaseKey => {}
        }
    }
    Ok(env)
}

pub(super) fn plaintext(row: &Row, key: &EncryptionKey) -> Result<RecordContent> {
    let value = match row.key.kind {
        RecordKind::Storage => {
            let mut credentials: EncryptedStorageCredentials = serde_json::from_str(&row.value)?;
            credentials.access_key_id.value = decrypt(&credentials.access_key_id.value, key)?;
            credentials.secret_access_key.value =
                decrypt(&credentials.secret_access_key.value, key)?;
            serde_json::to_string(&credentials)?
        }
        _ => decrypt(&row.value, key)?,
    };
    Ok(RecordContent {
        value: Zeroizing::new(value),
        expires_on: row.expires_on.clone(),
    })
}

pub(super) fn encrypted(
    key: RowKey,
    content: RecordContent,
    encryption: &EncryptionKey,
) -> Result<Row> {
    content.validate()?;
    let value = match key.kind {
        RecordKind::Storage => {
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
