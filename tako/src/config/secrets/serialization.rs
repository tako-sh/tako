use serde::Serialize;
use std::collections::{BTreeMap, HashMap};

use super::{EncryptedBackupKey, EncryptedSecretValue, EnvironmentSecrets};

pub(super) fn to_pretty_json(
    environments: &HashMap<String, EnvironmentSecrets>,
) -> serde_json::Result<String> {
    serde_json::to_string_pretty(&sorted_environments(environments))
}

fn sorted_environments(
    environments: &HashMap<String, EnvironmentSecrets>,
) -> BTreeMap<&String, SortedEnvironmentSecrets<'_>> {
    environments
        .iter()
        .map(|(env_name, env_secrets)| {
            let sorted_app = env_secrets.app.iter().collect::<BTreeMap<_, _>>();
            let sorted_storages = env_secrets.storages.iter().collect::<BTreeMap<_, _>>();
            let sorted_credentials = env_secrets.credentials.iter().collect::<BTreeMap<_, _>>();
            (
                env_name,
                SortedEnvironmentSecrets {
                    key_id: &env_secrets.key_id,
                    backup_keys: &env_secrets.backup_keys,
                    app: sorted_app,
                    storages: sorted_storages,
                    credentials: sorted_credentials,
                },
            )
        })
        .collect()
}

#[derive(Serialize)]
struct SortedEnvironmentSecrets<'a> {
    key_id: &'a str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    backup_keys: &'a Vec<EncryptedBackupKey>,
    app: BTreeMap<&'a String, &'a EncryptedSecretValue>,
    storages: BTreeMap<&'a String, &'a crate::config::EncryptedStorageCredentials>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    credentials: BTreeMap<&'a String, &'a EncryptedSecretValue>,
}
