//! SQL migrations: numbered `.sql` files applied to a database in order, each
//! once, and recorded in the database itself.
//!
//! The record is the `_tako_migrations` table, keyed by the app the
//! migrations belong to and the file name, with each file's SHA-256. Apps
//! that share a database keep separate histories. Each file runs with its
//! record in one `BEGIN IMMEDIATE` transaction, so a failing file leaves no
//! trace and the files before it stay applied.
//!
//! Foreign keys are not enforced while a file runs, so it can rebuild a
//! table that others reference (https://sqlite.org/lang_altertable.html);
//! `PRAGMA foreign_key_check` must pass before the file commits.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::engine::{Args, Budget, Connection, EngineConfig};
use crate::proto::Value;

const TABLE: &str = "CREATE TABLE IF NOT EXISTS _tako_migrations (\
    app TEXT NOT NULL, \
    name TEXT NOT NULL, \
    sha256 TEXT NOT NULL, \
    applied_at INTEGER NOT NULL, \
    PRIMARY KEY (app, name)\
)";

/// The longest migration file name.
const MAX_NAME_BYTES: usize = 255;

/// One migration file.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Migration {
    /// The file name, such as `0001_initial.sql`.
    pub name: String,
    pub sql: String,
}

/// What a migration run did, or would do when it was a dry run.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MigrationReport {
    /// Migrations applied by this run, in order.
    pub applied: Vec<String>,
    /// Migrations a dry run would apply, in order.
    pub pending: Vec<String>,
    pub warnings: Vec<String>,
}

/// The order number of a migration file name: the digits before the first
/// `_` (or before `.sql`). Names look like `0001_initial.sql`.
pub fn migration_number(name: &str) -> Option<u64> {
    if name.len() > MAX_NAME_BYTES {
        return None;
    }
    let stem = name.strip_suffix(".sql")?;
    let (digits, rest) = match stem.split_once('_') {
        Some((digits, rest)) => (digits, Some(rest)),
        None => (stem, None),
    };
    if digits.is_empty() || digits.len() > 18 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if let Some(rest) = rest
        && (rest.is_empty()
            || !rest
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b)))
    {
        return None;
    }
    digits.parse().ok()
}

/// Checks file names and returns the migrations in the order they run.
/// Fails on a name without an order number or two files with one number.
pub fn order(migrations: &[Migration]) -> Result<Vec<(u64, &Migration)>, String> {
    let mut ordered = Vec::with_capacity(migrations.len());
    for migration in migrations {
        let number = migration_number(&migration.name).ok_or_else(|| {
            format!(
                "Migration file `{}` must be named like `0001_initial.sql`: a number, then `_` and letters, digits, `-`, `_` or `.`.",
                migration.name
            )
        })?;
        ordered.push((number, migration));
    }
    ordered.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.name.cmp(&b.1.name)));
    for pair in ordered.windows(2) {
        if pair[0].0 == pair[1].0 {
            return Err(format!(
                "Migrations `{}` and `{}` share the number {}; renumber one.",
                pair[0].1.name, pair[1].1.name, pair[0].0
            ));
        }
    }
    Ok(ordered)
}

fn sha256(sql: &str) -> String {
    hex::encode(Sha256::digest(sql.as_bytes()))
}

fn engine_error(error: crate::proto::Error) -> String {
    match error.extended_code {
        Some(code) => format!("{} ({code})", error.message),
        None => format!("{} ({})", error.message, error.code),
    }
}

/// Applies `app`'s pending migrations on `conn`, or reports them when
/// `dry_run` is set.
pub(crate) fn run(
    conn: &mut Connection,
    app: &str,
    migrations: &[Migration],
    dry_run: bool,
) -> Result<MigrationReport, String> {
    if app.is_empty() || app.len() > 128 {
        return Err("migrations must name the app they belong to".into());
    }
    let ordered = order(migrations)?;
    // Reading the history writes nothing, so it does not count as a change.
    let exists = !conn
        .execute(
            "SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = '_tako_migrations'",
            &Args::Positional(&[]),
            true,
            &mut Budget::new(usize::MAX),
        )
        .map_err(engine_error)?
        .rows
        .is_empty();
    let mut applied: BTreeMap<String, String> = BTreeMap::new();
    if exists {
        let recorded = conn
            .execute(
                "SELECT name, sha256 FROM _tako_migrations WHERE app = ?1",
                &Args::Positional(&[Value::Text(app.to_owned())]),
                true,
                &mut Budget::new(usize::MAX),
            )
            .map_err(engine_error)?;
        for row in recorded.rows {
            if let [Value::Text(name), Value::Text(sha)] = row.as_slice() {
                applied.insert(name.clone(), sha.clone());
            }
        }
    }

    let mut report = MigrationReport::default();
    for (name, sha) in &applied {
        match migrations.iter().find(|m| &m.name == name) {
            Some(migration) if &sha256(&migration.sql) != sha => {
                return Err(format!(
                    "Migration `{name}` changed after it was applied to this database. Restore its applied contents and put the change in a new migration."
                ));
            }
            Some(_) => {}
            None => report.warnings.push(format!(
                "Migration `{name}` was applied to this database but no longer exists."
            )),
        }
    }
    let latest = applied
        .keys()
        .filter_map(|name| migration_number(name))
        .max();
    let pending: Vec<&Migration> = ordered
        .iter()
        .filter(|(_, migration)| !applied.contains_key(&migration.name))
        .map(|(number, migration)| {
            if latest.is_some_and(|latest| *number < latest) {
                report.warnings.push(format!(
                    "Migration `{}` is numbered before migrations already applied; it runs now.",
                    migration.name
                ));
            }
            *migration
        })
        .collect();
    if dry_run {
        report.pending = pending.iter().map(|m| m.name.clone()).collect();
        return Ok(report);
    }

    if pending.is_empty() {
        return Ok(report);
    }
    if !exists {
        conn.sequence(TABLE).map_err(engine_error)?;
    }
    // Must be set outside a transaction; the connection is not reused.
    conn.sequence("PRAGMA foreign_keys = OFF")
        .map_err(engine_error)?;
    for migration in pending {
        let failure = |report: &MigrationReport, message: String| {
            if report.applied.is_empty() {
                message
            } else {
                format!(
                    "{message} Applied before it: {}.",
                    report.applied.join(", ")
                )
            }
        };
        conn.sequence("BEGIN IMMEDIATE")
            .map_err(|e| failure(&report, engine_error(e)))?;
        // Another run may have applied it since the history was read.
        let done = conn
            .execute(
                "SELECT 1 FROM _tako_migrations WHERE app = ?1 AND name = ?2",
                &Args::Positional(&[
                    Value::Text(app.to_owned()),
                    Value::Text(migration.name.clone()),
                ]),
                true,
                &mut Budget::new(usize::MAX),
            )
            .map(|result| !result.rows.is_empty());
        match done {
            Ok(true) => {
                conn.rollback();
                continue;
            }
            Ok(false) => {}
            Err(error) => {
                conn.rollback();
                return Err(failure(&report, engine_error(error)));
            }
        }
        match conn.sequence_in_transaction(&migration.sql) {
            Ok(true) => {}
            Ok(false) => {
                conn.rollback();
                return Err(failure(
                    &report,
                    format!(
                        "Migration `{}` controls its own transaction. Remove BEGIN, COMMIT, END, and ROLLBACK from it: each migration already runs in one.",
                        migration.name
                    ),
                ));
            }
            Err(error) => {
                conn.rollback();
                return Err(failure(
                    &report,
                    format!(
                        "Migration `{}` failed: {}",
                        migration.name,
                        engine_error(error)
                    ),
                ));
            }
        }
        // A safety net: nothing else should end the transaction.
        if conn.is_autocommit() {
            return Err(failure(
                &report,
                format!(
                    "Migration `{}` ended the transaction it runs in, so it was not recorded and may be partly applied. Remove BEGIN, COMMIT, and ROLLBACK from it.",
                    migration.name
                ),
            ));
        }
        match foreign_key_violations(conn) {
            Ok(None) => {}
            Ok(Some(violation)) => {
                conn.rollback();
                return Err(failure(
                    &report,
                    format!(
                        "Migration `{}` leaves rows that break foreign keys: {violation}.",
                        migration.name
                    ),
                ));
            }
            Err(error) => {
                conn.rollback();
                return Err(failure(&report, engine_error(error)));
            }
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let recorded = conn
            .execute(
                "INSERT INTO _tako_migrations (app, name, sha256, applied_at) VALUES (?1, ?2, ?3, ?4)",
                &Args::Positional(&[
                    Value::Text(app.to_owned()),
                    Value::Text(migration.name.clone()),
                    Value::Text(sha256(&migration.sql)),
                    Value::Integer(now),
                ]),
                false,
                &mut Budget::new(usize::MAX),
            )
            .and_then(|_| conn.sequence("COMMIT"));
        if let Err(error) = recorded {
            conn.rollback();
            return Err(failure(&report, engine_error(error)));
        }
        report.applied.push(migration.name.clone());
    }
    Ok(report)
}

/// The first row that breaks a foreign key, described.
fn foreign_key_violations(conn: &mut Connection) -> Result<Option<String>, crate::proto::Error> {
    let result = conn.execute(
        "PRAGMA foreign_key_check",
        &Args::Positional(&[]),
        true,
        &mut Budget::new(64 * 1024),
    );
    // A large report is still a failure; describe what fits.
    let rows = match result {
        Ok(result) => result.rows,
        Err(error) if error.code == "RESPONSE_TOO_LARGE" => {
            return Ok(Some("many rows".into()));
        }
        Err(error) => return Err(error),
    };
    Ok(rows.first().map(|row| {
        let text = |index: usize| match row.get(index) {
            Some(Value::Text(text)) => text.clone(),
            Some(Value::Integer(value)) => value.to_string(),
            _ => "?".into(),
        };
        format!(
            "a row of `{}` (rowid {}) references a missing row of `{}`",
            text(0),
            text(1),
            text(2)
        )
    }))
}

/// Applies migrations to a database file directly, creating the file when it
/// does not exist. For local databases (`tako dev`, `tako run`); hosted
/// databases go through [`crate::DatabaseHost::migrate`]. A dry run creates
/// nothing: without a file, every migration is pending.
pub fn migrate_file(
    path: &Path,
    app: &str,
    migrations: &[Migration],
    dry_run: bool,
) -> Result<MigrationReport, String> {
    if dry_run && !path.exists() {
        if app.is_empty() || app.len() > 128 {
            return Err("migrations must name the app they belong to".into());
        }
        return Ok(MigrationReport {
            applied: Vec::new(),
            pending: order(migrations)?
                .into_iter()
                .map(|(_, migration)| migration.name.clone())
                .collect(),
            warnings: Vec::new(),
        });
    }
    let config = config(&EngineConfig::default());
    let buffered = std::sync::Arc::new(tokio::sync::Semaphore::new(config.max_value_bytes));
    let mut conn = Connection::development(path, &config, false, buffered).map_err(engine_error)?;
    run(&mut conn, app, migrations, dry_run)
}

/// The limits migrations run with: a file's statements share one time
/// limit, longer than an app request's, and wait longer for the write lock,
/// which apps' writes hold briefly.
pub(crate) fn config(base: &EngineConfig) -> EngineConfig {
    EngineConfig {
        busy_timeout: MIGRATION_BUSY_TIMEOUT,
        statement_timeout: MIGRATION_TIMEOUT,
        ..base.clone()
    }
}

const MIGRATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5 * 60);
const MIGRATION_BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Build a replacement in isolation. Publish one portable file including all
/// migration writes, even when native MVCC kept them in a journal sidecar.
pub fn prepare_reset(
    path: &Path,
    app: Option<&str>,
    migrations: &[Migration],
) -> Result<Option<MigrationReport>, String> {
    let parent = path.parent().ok_or("database has no parent directory")?;
    let directory = tempfile::tempdir_in(parent).map_err(|e| e.to_string())?;
    let db = directory.path().join("data.db");
    let config = config(&EngineConfig::default());
    let owner = crate::mvcc::Owner::local(&db, config.busy_timeout).map_err(|e| e.to_string())?;
    let report = if let Some(app) = app.filter(|_| !migrations.is_empty()) {
        let buffered = std::sync::Arc::new(tokio::sync::Semaphore::new(config.max_value_bytes));
        let mut conn =
            Connection::hosted(owner.clone(), &config, false, buffered).map_err(engine_error)?;
        Some(run(&mut conn, app, migrations, false)?)
    } else {
        None
    };
    crate::mvcc::export_copy(&owner, path).map_err(|e| e.to_string())?;
    Ok(report)
}

#[cfg(test)]
mod tests;
