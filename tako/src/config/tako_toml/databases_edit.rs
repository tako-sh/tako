//! Removes `[databases.*]` entries and database bindings from `tako.toml`,
//! preserving the rest of the file: comments, ordering, and formatting.

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

impl TakoToml {
    /// Removes `[databases.<name>]` and every environment binding to it.
    pub fn remove_database_from_file(path: &Path, name: &str) -> Result<()> {
        let mut doc = load(path)?;
        if let Some(databases) = doc.get_mut("databases").and_then(Item::as_table_like_mut) {
            databases.remove(name);
            if databases.is_empty() {
                doc.remove("databases");
            }
        }
        if let Some(envs) = doc.get_mut("envs").and_then(Item::as_table_like_mut) {
            for (_, env) in envs.iter_mut() {
                let Some(env) = env.as_table_like_mut() else {
                    continue;
                };
                let empty = match env.get_mut("databases").and_then(Item::as_table_like_mut) {
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
                    env.remove("databases");
                }
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
            "name = \"shop\"\n\n[envs.production]\nroute = \"shop.example.com\"\ndatabases = { db = \"main\", other = \"orders\" }\n\n[envs.staging]\nroute = \"staging.example.com\"\ndatabases = { db = \"main\" }\n\n[databases.main]\nbackup = \"private_backups\"\n\n[databases.orders]\nbackup = \"private_backups\"\n\n[storages.private_backups]\nprovider = \"s3\"\nbucket = \"b\"\nendpoint = \"https://s3.example.test\"\nregion = \"auto\"\n",
        );
        TakoToml::remove_database_from_file(&path, "main").unwrap();
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
}
