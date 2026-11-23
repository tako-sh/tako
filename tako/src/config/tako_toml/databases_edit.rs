//! Removes `[db.*]` entries and database and KV store bindings from
//! `tako.toml`, preserving the rest of the file: comments, ordering, and
//! formatting.

use std::fs;
use std::path::Path;

use toml_edit::{DocumentMut, Item};

use super::schema::TakoToml;
use crate::config::error::{ConfigError, Result};

fn load(path: &Path) -> Result<DocumentMut> {
    let text =
        fs::read_to_string(path).map_err(|e| ConfigError::FileRead(path.to_path_buf(), e))?;
    text.parse::<DocumentMut>()
        .map_err(|e| ConfigError::Validation(format!("{}: {e}", path.display())))
}

/// Validates the edited document before replacing the file.
fn save(path: &Path, doc: &DocumentMut) -> Result<()> {
    let text = doc.to_string();
    TakoToml::parse(&text)?;
    let temp = path.with_extension("toml.tmp");
    fs::write(&temp, text.as_bytes()).map_err(|e| ConfigError::FileWrite(temp.clone(), e))?;
    fs::rename(&temp, path).map_err(|e| ConfigError::FileWrite(path.to_path_buf(), e))?;
    Ok(())
}

/// Removes the bindings under `key` (`databases` or `kv`) that name `name`
/// in the environments of `network`, dropping tables left empty. Returns
/// whether some environment is in another network, where they stay.
fn remove_env_bindings(doc: &mut DocumentMut, key: &str, name: &str, network: &str) -> bool {
    let Some(envs) = doc.get_mut("envs").and_then(Item::as_table_like_mut) else {
        return false;
    };
    let mut other_network = false;
    for (_, env) in envs.iter_mut() {
        let Some(env) = env.as_table_like_mut() else {
            continue;
        };
        if env
            .get("network")
            .and_then(Item::as_str)
            .is_some_and(|id| id != network)
        {
            other_network = true;
            continue;
        }
        let empty = match env.get_mut(key).and_then(Item::as_table_like_mut) {
            Some(bindings) => {
                let bound: Vec<String> = bindings
                    .iter()
                    .filter(|(_, target)| target.as_str() == Some(name))
                    .map(|(binding, _)| binding.to_owned())
                    .collect();
                for binding in bound {
                    bindings.remove(&binding);
                }
                bindings.is_empty()
            }
            None => false,
        };
        if empty {
            env.remove(key);
        }
    }
    other_network
}

impl TakoToml {
    /// Remove bindings in the selected network. A backup override is shared by
    /// every network and can still apply to an implicit channel database there.
    pub fn remove_database_from_file(path: &Path, name: &str, network: &str) -> Result<()> {
        let mut doc = load(path)?;
        let other_network = remove_env_bindings(&mut doc, "db", name, network);
        if !other_network
            && let Some(databases) = doc.get_mut("db").and_then(Item::as_table_like_mut)
        {
            databases.remove(name);
            if databases.is_empty() {
                doc.remove("db");
            }
        }
        save(path, &doc)
    }

    /// Removes the bindings to KV store `name` in the environments of
    /// `network`, and its `[kv.<name>]` settings unless environments of
    /// another network still bind it.
    pub fn remove_kv_from_file(path: &Path, name: &str, network: &str) -> Result<()> {
        let mut doc = load(path)?;
        let other_network = remove_env_bindings(&mut doc, "kv", name, network);
        if !other_network && let Some(stores) = doc.get_mut("kv").and_then(Item::as_table_like_mut)
        {
            stores.remove(name);
            if stores.is_empty() {
                doc.remove("kv");
            }
        }
        save(path, &doc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &tempfile::TempDir, text: &str) -> std::path::PathBuf {
        let path = dir.path().join("tako.toml");
        fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn remove_drops_the_resource_and_its_bindings() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir,
            "name = \"shop\"\n\n[envs.production]\nroute = \"shop.example.com\"\ndb = { db = \"main\", other = \"orders\" }\n\n[envs.staging]\nroute = \"staging.example.com\"\ndb = { db = \"main\" }\n\n[db.main]\nbackup = \"private_backups\"\n\n[db.orders]\nbackup = \"private_backups\"\n\n[storages.private_backups]\nprovider = \"s3\"\nbucket = \"b\"\nendpoint = \"https://s3.example.test\"\nregion = \"auto\"\n",
        );
        TakoToml::remove_database_from_file(&path, "main", "0123456789abcdef0123456789abcdef")
            .unwrap();
        let config = TakoToml::load_from_file(&path).unwrap();
        assert!(!config.databases.contains_key("main"));
        assert!(config.databases.contains_key("orders"));
        assert_eq!(config.envs["production"].databases.len(), 1);
        assert!(config.envs["staging"].databases.is_empty());
        assert!(
            !fs::read_to_string(&path)
                .unwrap()
                .contains("[envs.staging]\nroute = \"staging.example.com\"\ndatabases")
        );
    }

    #[test]
    fn removing_a_kv_store_drops_only_its_bindings() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir,
            "name = \"shop\"\n\n[envs.production]\nroute = \"shop.example.com\"\ndb = { main = \"main\" }\n# Sessions and challenges\nkv = { auth = \"sessions\", cache = \"other\" }\n\n[envs.staging]\nroute = \"staging.example.com\"\nkv = { auth = \"sessions\" }\n\n[kv.sessions]\nbackup = \"backups\"\n\n[kv.other]\nbackup = \"backups\"\n\n[storages.backups]\nbucket = \"b\"\nendpoint = \"https://s3.example.test\"\nregion = \"auto\"\n",
        );
        TakoToml::remove_kv_from_file(&path, "sessions", "0123456789abcdef0123456789abcdef")
            .unwrap();
        let config = TakoToml::load_from_file(&path).unwrap();
        assert!(!config.kv.contains_key("sessions"));
        assert!(config.kv.contains_key("other"));
        assert_eq!(config.envs["production"].kv.len(), 1);
        assert_eq!(config.envs["production"].databases.len(), 1);
        assert!(config.envs["staging"].kv.is_empty());
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("# Sessions and challenges"));
        assert!(!text.contains("[envs.staging]\nroute = \"staging.example.com\"\nkv"));
    }

    #[test]
    fn deleting_in_another_network_preserves_channel_replay_backup_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir,
            r#"
name = "shop"
[envs.production]
route = "shop.example.com"
network = "0123456789abcdef0123456789abcdef"
channels.db = "main"
[envs.staging]
route = "staging.example.com"
network = "fedcba9876543210fedcba9876543210"
db = { db = "main" }
[db.main]
backup = "backups"
[storages.backups]
bucket = "b"
endpoint = "https://s3.example.test"
region = "auto"
"#,
        );
        TakoToml::remove_database_from_file(&path, "main", "fedcba9876543210fedcba9876543210")
            .unwrap();
        let config = TakoToml::load_from_file(&path).unwrap();
        assert_eq!(
            config.envs["production"].channels.as_ref().unwrap().db,
            "main"
        );
        assert_eq!(config.databases["main"].backup.as_deref(), Some("backups"));
        assert!(config.envs["staging"].databases.is_empty());
    }
}
