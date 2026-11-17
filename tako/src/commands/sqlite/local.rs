//! Local copies of databases: the files `tako dev` and `tako run` serve.
//!
//! They live in Tako's data directory under the project's network (see
//! [`crate::local_databases`]), so every project of the network that binds a
//! name shares one local copy of that database.

use std::path::{Path, PathBuf};

use crate::local_databases::{self, LocalDatabase};
use crate::output;

use super::{confirm, dry_run, load_project};

/// The local database `name` of the project at `config_path`: the one a
/// development binding names, or else the project's namespace.
pub(super) fn resolve(
    config: &crate::config::TakoToml,
    network: Option<&str>,
    name: &str,
) -> LocalDatabase {
    if network.is_none()
        && let Some(local) = local_databases::development_bindings(config)
            .into_values()
            .find(|local| local.name == name)
    {
        return local;
    }
    LocalDatabase {
        name: name.to_owned(),
        namespace: local_databases::namespace(config, network),
    }
}

fn project_dir(project: &crate::commands::project_context::ProjectContext) -> PathBuf {
    project
        .project_dir
        .canonicalize()
        .unwrap_or_else(|_| project.project_dir.clone())
}

/// Lists the local databases of the project's network.
pub(super) fn list(
    config_path: Option<&Path>,
    network: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    let (project, config) = load_project(config_path)?;
    let namespace = local_databases::namespace(&config, network);
    let probe = LocalDatabase {
        name: "probe".into(),
        namespace: namespace.clone(),
    };
    let dir = local_databases::database_path(&project_dir(&project), &probe)
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let bound = local_databases::development_bindings(&config);
    let mut databases = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_suffix(".sqlite"))
            else {
                continue;
            };
            let size = entry.metadata().map(|meta| meta.len()).unwrap_or(0);
            let bindings: Vec<&str> = bound
                .iter()
                .filter(|(_, local)| local.name == name && local.namespace == namespace)
                .map(|(binding, _)| binding.as_str())
                .collect();
            databases.push((name.to_owned(), size, bindings, path.clone()));
        }
    }
    databases.sort_by(|a, b| a.0.cmp(&b.0));
    if output::is_json() {
        let items: Vec<serde_json::Value> = databases
            .iter()
            .map(|(name, size, bindings, path)| {
                serde_json::json!({
                    "name": name,
                    "size_bytes": size,
                    "bindings": bindings,
                    "path": path,
                })
            })
            .collect();
        return output::json_result(serde_json::json!({
            "ok": true, "command": "sqlite", "namespace": namespace, "databases": items,
        }));
    }
    if databases.is_empty() {
        output::info(&format!(
            "No local databases for {namespace} yet; `tako dev` creates them on first use."
        ));
        return Ok(());
    }
    output::section(&format!("Local databases ({namespace})"));
    for (name, size, bindings, _) in &databases {
        let used = if bindings.is_empty() {
            String::new()
        } else {
            format!(", bound here as {}", bindings.join(", "))
        };
        output::info(&format!("{name} ({}{used})", output::format_size(*size)));
    }
    Ok(())
}

/// Deletes a local database's file. Refused while `tako dev` or `tako run`
/// serves it.
pub(super) fn delete(
    config_path: Option<&Path>,
    network: Option<&str>,
    name: &str,
    yes: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    tako_core::validate_database_name(name)?;
    let (project, config) = load_project(config_path)?;
    let local = resolve(&config, network, name);
    let path = local_databases::database_path(&project_dir(&project), &local);
    if !path.exists() {
        return Err(format!("There is no local database named {name}.").into());
    }
    confirm(
        &format!("Delete the local database {name}?"),
        &format!(
            "Its file is removed for every project of {} that binds it; `tako dev` creates an empty one again on next use.",
            local.namespace
        ),
        yes,
    )?;
    if output::is_dry_run() {
        return dry_run(
            &format!("Delete the local database {name}"),
            serde_json::json!({"database": name, "path": path}),
        );
    }
    let Some(lock) = tako_database::lock_unused(&path)? else {
        return Err(format!(
            "The local database {name} is in use by `tako dev` or `tako run`; stop them first."
        )
        .into());
    };
    tako_database::remove_database_files(&path, lock)?;
    if output::is_json() {
        return output::json_result(serde_json::json!({
            "ok": true, "command": "sqlite", "deleted": name, "local": true,
        }));
    }
    output::success(&format!("Deleted the local database {name}"));
    Ok(())
}

/// Empties a local database and applies the project's migrations again.
pub(super) fn reset(
    config_path: Option<&Path>,
    network: Option<&str>,
    name: &str,
    yes: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    tako_core::validate_database_name(name)?;
    let (project, config) = load_project(config_path)?;
    let local = resolve(&config, network, name);
    let path = local_databases::database_path(&project_dir(&project), &local);
    confirm(
        &format!("Reset the local database {name}?"),
        "Every table in it is dropped; then the development migrations run again.",
        yes,
    )?;
    if output::is_dry_run() {
        return dry_run(
            &format!("Reset the local database {name}"),
            serde_json::json!({"database": name}),
        );
    }
    if path.exists() {
        tako_database::reset_database_file(&path)?;
    }
    // Only this database: the others keep their state.
    let migrated =
        local_databases::development_migrations(&project.project_dir, &config, false, Some(name))?;
    for warning in &migrated.warnings {
        output::warning(warning);
    }
    if output::is_json() {
        return output::json_result(serde_json::json!({
            "ok": true, "command": "sqlite", "reset": name, "applied": migrated.applied,
        }));
    }
    output::success(&format!("Reset the local database {name}"));
    for line in &migrated.applied {
        output::info(line);
    }
    Ok(())
}

/// Installs `source`, a verified database file, as the local database at
/// `dest`. When no process serves it, the file is replaced; while `tako dev`
/// or `tako run` serves it, the contents are copied into it in place through
/// SQLite's backup, so they see the new contents without a restart.
fn install(source: &Path, dest: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match tako_database::lock_unused(dest)? {
        Some(lock) => {
            // The old file, with its write-ahead log, goes; the new one takes
            // its place in one rename.
            let staged = dest.with_extension("sqlite.installing");
            std::fs::copy(source, &staged)?;
            tako_database::remove_database_files(dest, lock)?;
            std::fs::rename(&staged, dest)?;
        }
        None => {
            let from = rusqlite::Connection::open_with_flags(
                source,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )?;
            let mut to = rusqlite::Connection::open(dest)?;
            to.busy_timeout(std::time::Duration::from_secs(10))?;
            rusqlite::backup::Backup::new(&from, &mut to)?.run_to_completion(
                1024,
                std::time::Duration::ZERO,
                None,
            )?;
        }
    }
    Ok(())
}

/// After installing a copy, applies the project's development migrations to
/// it, so its schema matches the code: the copy may lag behind them.
fn migrate_installed(
    ctx: &super::Context,
    name: &str,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let migrated =
        local_databases::development_migrations(&ctx.project_dir, &ctx.config, false, Some(name))?;
    for warning in &migrated.warnings {
        output::warning(warning);
    }
    Ok(migrated.applied)
}

fn finish(
    ctx: &super::Context,
    name: &str,
    source: serde_json::Value,
) -> Result<(), Box<dyn std::error::Error>> {
    let applied = migrate_installed(ctx, name)?;
    if output::is_json() {
        return output::json_result(serde_json::json!({
            "ok": true, "command": "sqlite", "local": name, "source": source, "applied": applied,
        }));
    }
    for line in &applied {
        output::info(line);
    }
    Ok(())
}

fn local_of(ctx: &super::Context, name: &str) -> (LocalDatabase, PathBuf) {
    let local = LocalDatabase {
        name: name.to_owned(),
        namespace: ctx.network.clone(),
    };
    let path = local_databases::database_path(&ctx.project_dir, &local);
    (local, path)
}

/// A scratch directory next to the local file, so installing it is a rename
/// on the same filesystem.
fn scratch(dest: &Path) -> Result<tempfile::TempDir, Box<dyn std::error::Error>> {
    let parent = dest.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    Ok(tempfile::Builder::new()
        .prefix(".download-")
        .tempdir_in(parent)?)
}

/// Reads a snapshot of a network database from backup storage on this
/// machine (the database's server need not be reachable), into `work`, and
/// verifies it. Returns its manifest and the restored file.
async fn fetch_snapshot(
    ctx: &super::Context,
    name: &str,
    snapshot: Option<&str>,
    work: &Path,
) -> Result<(tako_storage::database_snapshots::Manifest, PathBuf), Box<dyn std::error::Error>> {
    use tako_storage::database_snapshots as snapshots;
    let (record, home) = ctx.database(name)?;
    // Where its server sends snapshots, when it can still be asked.
    let source = super::send::<serde_json::Value>(
        &home,
        tako_core::Command::DatabaseStatus {
            name: Some(name.to_owned()),
        },
    )
    .await
    .ok()
    .and_then(|status| {
        serde_json::from_value::<tako_core::BackupDestination>(
            status["databases"][0]["backup"]["destination"].clone(),
        )
        .ok()
    });
    let storage = ctx.snapshot_storage(name, source)?;
    let key = snapshots::key_binding(&record.generation, &ctx.backup_key(&record)?);
    let fetched = output::with_spinner_async(
        &format!("Downloading a snapshot of {name}"),
        &format!("Downloaded a snapshot of {name}"),
        async {
            let store = tako_storage::s3(&storage)?;
            let manifests = snapshots::list_manifests(
                &store,
                name,
                &record.generation,
                std::time::Duration::from_secs(30),
            )
            .await?;
            let manifest = snapshots::choose(manifests, name, snapshot)?;
            let encrypted = work.join("snapshot.sqlite.zst.enc");
            snapshots::download(
                &store,
                &snapshots::data_key(name, &record.generation, &manifest.snapshot.id),
                &encrypted,
                std::time::Duration::from_secs(30 * 60),
            )
            .await?;
            let (dir, opened) = (work.to_path_buf(), manifest.clone());
            let restored = tokio::task::spawn_blocking(move || -> Result<PathBuf, String> {
                let restored = snapshots::open(&encrypted, &dir, &opened, &key)?;
                tako_database::verify_database(&restored)?;
                Ok(restored)
            })
            .await
            .map_err(|e| e.to_string())??;
            Ok::<_, String>((manifest, restored))
        },
    )
    .await?;
    Ok(fetched)
}

/// Replaces the local copy of a network database with one of its snapshots,
/// read from backup storage on this machine.
pub(super) async fn restore(
    ctx: &super::Context,
    name: &str,
    snapshot: Option<String>,
    yes: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    ctx.require_admin()?;
    ctx.database(name)?;
    let (_, dest) = local_of(ctx, name);
    let which = snapshot.as_deref().unwrap_or("the newest snapshot");
    confirm(
        &format!("Replace the local copy of {name} with {which}?"),
        "The local copy's current contents are lost; the network's database does not change.",
        yes,
    )?;
    if output::is_dry_run() {
        return dry_run(
            &format!("Restore {name} locally"),
            serde_json::json!({"database": name, "snapshot": snapshot, "path": dest}),
        );
    }
    let work = scratch(&dest)?;
    let _t = output::timed(&format!("Restore {name} locally"));
    let (manifest, restored) = fetch_snapshot(ctx, name, snapshot.as_deref(), work.path()).await?;
    install(&restored, &dest)?;
    output::success(&format!(
        "Restored snapshot {} of {name} locally",
        manifest.snapshot.id
    ));
    finish(
        ctx,
        name,
        serde_json::json!({"snapshot": manifest.snapshot}),
    )
}

/// Writes a snapshot of a network database to `out`, read from backup
/// storage on this machine. Nothing is replaced.
pub(super) async fn download_snapshot(
    ctx: &super::Context,
    name: &str,
    snapshot: Option<String>,
    out: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    ctx.require_admin()?;
    ctx.database(name)?;
    if out.exists() {
        return Err(format!("{} already exists.", out.display()).into());
    }
    if output::is_dry_run() {
        return dry_run(
            &format!("Write a snapshot of {name} to {}", out.display()),
            serde_json::json!({"database": name, "snapshot": snapshot, "path": out}),
        );
    }
    let directory = match out.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    std::fs::create_dir_all(directory)?;
    // Next to the file, so moving it into place is a rename.
    let work = tempfile::Builder::new()
        .prefix(".snapshot-")
        .tempdir_in(directory)?;
    let _t = output::timed(&format!("Download a snapshot of {name}"));
    let (manifest, restored) = fetch_snapshot(ctx, name, snapshot.as_deref(), work.path()).await?;
    if out.exists() {
        return Err(format!("{} already exists.", out.display()).into());
    }
    std::fs::rename(&restored, out)?;
    if output::is_json() {
        return output::json_result(serde_json::json!({
            "ok": true, "command": "sqlite",
            "downloaded": {"database": name, "snapshot": manifest.snapshot, "path": out},
        }));
    }
    output::success(&format!(
        "Wrote snapshot {} of {name} to {}",
        manifest.snapshot.id,
        out.display()
    ));
    Ok(())
}

/// Replaces the local copy of a network database with a consistent copy of
/// the database as it is now, downloaded from its server.
pub(super) async fn pull(
    ctx: &super::Context,
    name: &str,
    yes: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    ctx.require_admin()?;
    let (_, home) = ctx.database(name)?;
    let (_, dest) = local_of(ctx, name);
    confirm(
        &format!(
            "Replace the local copy of {name} with its current contents on {}?",
            home.name
        ),
        "The local copy's current contents are lost; the network's database does not change.",
        yes,
    )?;
    if output::is_dry_run() {
        return dry_run(
            &format!("Pull {name}"),
            serde_json::json!({"database": name, "server": home.name, "path": dest}),
        );
    }
    let work = scratch(&dest)?;
    let copy = work.path().join("copy.sqlite");
    let _t = output::timed(&format!("Pull {name}"));
    let export = super::transfer::download(
        &home,
        name,
        &copy,
        &format!("Downloading {name} from {}", home.name),
        &format!("Downloaded {name} from {}", home.name),
    )
    .await?;
    let verify = copy.clone();
    tokio::task::spawn_blocking(move || tako_database::verify_database(&verify))
        .await
        .map_err(|e| e.to_string())??;
    install(&copy, &dest)?;
    output::success(&format!("Pulled {name} into its local copy"));
    finish(
        ctx,
        name,
        serde_json::json!({"server": home.name, "size_bytes": export.size_bytes}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn database(path: &Path, value: i64) {
        if !path.exists() {
            tako_database::DatabaseHost::create_file(path).unwrap();
        }
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS t (x); DELETE FROM t; INSERT INTO t VALUES ({value});"
        ))
        .unwrap();
    }

    fn value(path: &Path) -> i64 {
        rusqlite::Connection::open(path)
            .unwrap()
            .query_row("SELECT x FROM t", [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn installing_replaces_an_unused_local_copy() {
        let dir = tempfile::tempdir().unwrap();
        let (source, dest) = (dir.path().join("copy.sqlite"), dir.path().join("db.sqlite"));
        database(&source, 2);
        // Missing, then present but unused.
        install(&source, &dest).unwrap();
        assert_eq!(value(&dest), 2);
        database(&source, 3);
        install(&source, &dest).unwrap();
        assert_eq!(value(&dest), 3);
        assert!(!dest.with_extension("sqlite.installing").exists());
    }

    #[test]
    fn installing_into_a_served_copy_writes_it_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let (source, dest) = (dir.path().join("copy.sqlite"), dir.path().join("db.sqlite"));
        database(&dest, 1);
        database(&source, 7);
        // As `tako dev` holds it: a host serves the file, with its lock.
        let host = tako_database::DatabaseHost::new(tako_database::HostConfig::default());
        let id = tako_database::DatabaseId::parse("local-test").unwrap();
        host.register(id, dest.clone()).unwrap();
        assert!(tako_database::lock_unused(&dest).unwrap().is_none());
        install(&source, &dest).unwrap();
        assert_eq!(value(&dest), 7);
    }
}
