//! A project's SQL migrations: `migrations/<binding>/*.sql` for each database
//! binding, or `migrations/*.sql` when the project binds a single name.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use tako_database::Migration;

use crate::config::TakoToml;

/// The most SQL one binding's migrations may hold, so they fit in one
/// management request.
const MAX_TOTAL_BYTES: usize = 8 * 1024 * 1024;

/// Every binding name the project uses, in any environment.
pub(crate) fn binding_names(config: &TakoToml) -> BTreeSet<String> {
    config
        .envs
        .values()
        .flat_map(|env| env.databases.keys().cloned())
        .collect()
}

fn sql_files(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("read {}: {error}", dir.display())),
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("read {}: {e}", dir.display()))?;
        let path = entry.path();
        let is_file = entry.file_type().map(|t| t.is_file()).unwrap_or(false);
        if is_file && path.extension().is_some_and(|ext| ext == "sql") {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

/// The directory that holds `binding`'s migrations, if any.
pub(crate) fn directory(
    project_dir: &Path,
    config: &TakoToml,
    binding: &str,
) -> Result<Option<PathBuf>, String> {
    let root = project_dir.join("migrations");
    let own = root.join(binding);
    if own.is_dir() {
        return Ok(Some(own));
    }
    let top = sql_files(&root)?;
    if top.is_empty() {
        return Ok(None);
    }
    let names = binding_names(config);
    if names.len() == 1 && names.contains(binding) {
        return Ok(Some(root));
    }
    Err(format!(
        "migrations/*.sql cannot tell which database it is for: this project binds {}. Move the files to migrations/<binding>/.",
        names
            .iter()
            .map(|name| format!("`{name}`"))
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

/// The migrations of `binding`, in order. Empty when it has none.
pub(crate) fn load(
    project_dir: &Path,
    config: &TakoToml,
    binding: &str,
) -> Result<Vec<Migration>, String> {
    let Some(dir) = directory(project_dir, config, binding)? else {
        return Ok(Vec::new());
    };
    let mut migrations = Vec::new();
    let mut total = 0usize;
    for path in sql_files(&dir)? {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| format!("{} is not a UTF-8 file name", path.display()))?
            .to_owned();
        let sql =
            std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        total += sql.len();
        migrations.push(Migration { name, sql });
    }
    if total > MAX_TOTAL_BYTES {
        return Err(format!(
            "The migrations in {} add up to more than 8 MiB; load large data with `tako sqlite import` instead.",
            dir.display()
        ));
    }
    tako_database::migrate::order(&migrations)
        .map_err(|error| format!("{}: {error}", dir.display()))?;
    Ok(migrations)
}

/// The migrations one database receives, from the folder of one of the
/// bindings that name it.
#[derive(Debug)]
pub(crate) struct DatabaseMigrations {
    pub binding: String,
    pub migrations: Vec<Migration>,
}

/// Each database's migrations, for `bindings` of binding names to database
/// names. Databases without migrations are left out. Migration history is
/// kept per app, not per binding, so two bindings of one database cannot
/// both bring migrations unless they are the same files.
pub(crate) fn by_database<'a>(
    project_dir: &Path,
    config: &TakoToml,
    bindings: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Result<BTreeMap<String, DatabaseMigrations>, String> {
    let mut selected: BTreeMap<String, DatabaseMigrations> = BTreeMap::new();
    let mut bindings: Vec<_> = bindings.into_iter().collect();
    bindings.sort();
    bindings.dedup();
    for (binding, database) in bindings {
        let migrations = load(project_dir, config, binding)?;
        if migrations.is_empty() {
            continue;
        }
        match selected.get(database) {
            None => {
                selected.insert(
                    database.to_owned(),
                    DatabaseMigrations {
                        binding: binding.to_owned(),
                        migrations,
                    },
                );
            }
            Some(existing) if existing.migrations == migrations => {}
            Some(existing) => {
                return Err(format!(
                    "Database `{database}` is bound as `{}` and as `{binding}`, and both have different migrations. A database's migrations share one history, so keep them in one of migrations/{}/ and migrations/{binding}/.",
                    existing.binding, existing.binding
                ));
            }
        }
    }
    Ok(selected)
}

/// The wire form of migrations, for `MigrateDatabase`.
pub(crate) fn to_wire(migrations: Vec<Migration>) -> Vec<tako_core::DatabaseMigration> {
    migrations
        .into_iter()
        .map(|m| tako_core::DatabaseMigration {
            name: m.name,
            sql: m.sql,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(text: &str) -> TakoToml {
        TakoToml::parse(text).unwrap()
    }

    fn write(dir: &Path, path: &str, text: &str) {
        let path = dir.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    const ONE: &str = "name = \"shop\"\n[envs.development]\ndatabases = { main = \"shop\" }\n";
    const TWO: &str = "name = \"shop\"\n[envs.development]\ndatabases = { main = \"shop\", events = \"events\" }\n";

    #[test]
    fn a_single_binding_reads_the_top_level_folder() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "migrations/0002_b.sql", "SELECT 2;");
        write(dir.path(), "migrations/0001_a.sql", "SELECT 1;");
        write(dir.path(), "migrations/README.md", "notes");
        let loaded = load(dir.path(), &config(ONE), "main").unwrap();
        let names: Vec<_> = loaded.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["0001_a.sql", "0002_b.sql"]);
    }

    #[test]
    fn several_bindings_need_their_own_folders() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "migrations/main/0001_a.sql", "SELECT 1;");
        let config = config(TWO);
        assert_eq!(load(dir.path(), &config, "main").unwrap().len(), 1);
        assert!(load(dir.path(), &config, "events").unwrap().is_empty());
        write(dir.path(), "migrations/0001_loose.sql", "SELECT 1;");
        let error = load(dir.path(), &config, "events").unwrap_err();
        assert!(error.contains("migrations/<binding>/"), "{error}");
        // A binding with its own folder is unaffected.
        assert_eq!(load(dir.path(), &config, "main").unwrap().len(), 1);
    }

    #[test]
    fn a_database_takes_migrations_from_one_binding() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(
            "name = \"shop\"\n[envs.development]\ndatabases = { main = \"shop\", reports = \"shop\", events = \"events\" }\n",
        );
        write(dir.path(), "migrations/main/0001_a.sql", "SELECT 1;");
        write(dir.path(), "migrations/events/0001_a.sql", "SELECT 2;");
        let bindings = [("main", "shop"), ("reports", "shop"), ("events", "events")];
        let selected = by_database(dir.path(), &config, bindings).unwrap();
        assert_eq!(selected["shop"].binding, "main");
        assert_eq!(selected["events"].migrations[0].sql, "SELECT 2;");
        // The same files under a second binding are the same history.
        write(dir.path(), "migrations/reports/0001_a.sql", "SELECT 1;");
        assert_eq!(by_database(dir.path(), &config, bindings).unwrap().len(), 2);
        // Different ones would collide in it.
        write(dir.path(), "migrations/reports/0001_a.sql", "SELECT 3;");
        let error = by_database(dir.path(), &config, bindings).unwrap_err();
        assert!(
            error.contains("bound as `main` and as `reports`"),
            "{error}"
        );
    }

    #[test]
    fn bad_names_are_reported_with_their_folder() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "migrations/initial.sql", "SELECT 1;");
        let error = load(dir.path(), &config(ONE), "main").unwrap_err();
        assert!(error.contains("initial.sql"), "{error}");
    }
}
