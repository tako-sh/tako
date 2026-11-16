//! Edits `[databases.*]` and database bindings in `tako.toml`, preserving
//! the rest of the file: comments, ordering, and formatting.

use std::fs;
use std::path::Path;

use toml_edit::{DocumentMut, InlineTable, Item, Table, Value, value};

use super::schema::{DatabaseResourceConfig, TakoToml};
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

fn table<'a>(parent: &'a mut Table, key: &str, path: &str) -> Result<&'a mut Table> {
    if !parent.contains_key(key) {
        let mut table = Table::new();
        table.set_implicit(true);
        parent.insert(key, Item::Table(table));
    }
    match parent.get_mut(key) {
        Some(item) => {
            if let Some(inline) = item.as_inline_table().cloned() {
                *item = Item::Table(inline.into_table());
            }
            item.as_table_mut()
                .ok_or_else(|| ConfigError::Validation(format!("{path} must be a table")))
        }
        None => unreachable!("inserted above"),
    }
}

impl TakoToml {
    /// Declares `[databases.<name>]` and, optionally, binds it in
    /// `[envs.<env>].databases` as `<binding> = "<name>"`.
    pub fn upsert_database_in_file(
        path: &Path,
        name: &str,
        database: &DatabaseResourceConfig,
        binding: Option<(&str, &str)>,
    ) -> Result<()> {
        let mut doc = load(path)?;
        let root = doc.as_table_mut();
        let databases = table(root, "databases", "[databases]")?;
        let mut entry = Table::new();
        entry.insert("id", value(&database.id));
        entry.insert("server", value(&database.server));
        if let Some(backup) = &database.backup {
            entry.insert("backup", value(backup));
        }
        if database.access == tako_core::DatabaseAccess::Network {
            entry.insert("access", value("network"));
        }
        databases.insert(name, Item::Table(entry));

        if let Some((env, binding)) = binding {
            let envs = table(doc.as_table_mut(), "envs", "[envs]")?;
            let env_table = table(envs, env, &format!("[envs.{env}]"))?;
            match env_table.get_mut("databases") {
                Some(item) => {
                    if let Some(inline) = item.as_inline_table_mut() {
                        inline.insert(binding, Value::from(name));
                    } else if let Some(table) = item.as_table_mut() {
                        table.insert(binding, value(name));
                    } else {
                        return Err(ConfigError::Validation(format!(
                            "[envs.{env}].databases must be a table"
                        )));
                    }
                }
                None => {
                    let mut inline = InlineTable::new();
                    inline.insert(binding, Value::from(name));
                    env_table.insert("databases", Item::Value(Value::InlineTable(inline)));
                }
            }
        }
        save(path, &doc)
    }

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

    fn database() -> DatabaseResourceConfig {
        DatabaseResourceConfig {
            id: "0123456789abcdef0123456789abcdef".into(),
            server: "edge-a".into(),
            backup: None,
            access: tako_core::DatabaseAccess::Local,
        }
    }

    #[test]
    fn upsert_keeps_comments_and_adds_an_inline_binding() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir,
            "# My app\nname = \"shop\"\n\n[envs.production]\n# public route\nroute = \"shop.example.com\"\n",
        );
        TakoToml::upsert_database_in_file(&path, "main", &database(), Some(("production", "db")))
            .unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("# My app"));
        assert!(text.contains("# public route"));
        assert!(text.contains("databases = { db = \"main\" }"), "{text}");
        assert!(text.contains("[databases.main]"), "{text}");
        let config = TakoToml::parse(&text).unwrap();
        assert_eq!(config.databases["main"], database());
        assert_eq!(config.envs["production"].databases["db"], "main");
    }

    #[test]
    fn remove_drops_the_resource_and_its_bindings() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir,
            "name = \"shop\"\n\n[envs.production]\nroute = \"shop.example.com\"\ndatabases = { db = \"main\", other = \"orders\" }\n\n[envs.staging]\nroute = \"staging.example.com\"\ndatabases = { db = \"main\" }\n\n[databases.main]\nid = \"0123456789abcdef0123456789abcdef\"\nserver = \"edge-a\"\n\n[databases.orders]\nid = \"fedcba9876543210fedcba9876543210\"\nserver = \"edge-a\"\n",
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

    #[test]
    fn invalid_results_leave_the_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let original = "name = \"shop\"\n";
        let path = write(&dir, original);
        let mut bad = database();
        bad.id = "not-an-id".into();
        assert!(TakoToml::upsert_database_in_file(&path, "main", &bad, None).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }
}
