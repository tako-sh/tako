use super::{STATE_SCHEMA_VERSION, SqliteStateStore, StateStoreError};

impl SqliteStateStore {
    pub fn init(&self) -> Result<(), StateStoreError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| StateStoreError::Sqlite(format!("create db parent: {e}")))?;
        }

        let mut conn = self.lock_conn()?;
        let version: i32 =
            conn.query_row("PRAGMA user_version;", [], |row| row.get::<_, i64>(0))? as i32;

        if version > STATE_SCHEMA_VERSION {
            return Err(StateStoreError::UnsupportedSchemaVersion { found: version });
        }

        if version == 0 {
            initialize_schema(&mut conn)
        } else if version < STATE_SCHEMA_VERSION {
            migrate_schema(&mut conn, version)
        } else {
            ensure_schema_objects(&conn)?;
            ensure_default_rows(&conn)
        }
    }
}

fn initialize_schema(conn: &mut rusqlite::Connection) -> Result<(), StateStoreError> {
    let tx = conn.unchecked_transaction()?;
    ensure_schema_objects(&tx)?;
    ensure_default_rows(&tx)?;
    tx.execute_batch(&format!("PRAGMA user_version = {STATE_SCHEMA_VERSION};"))?;
    tx.commit()?;
    Ok(())
}

fn migrate_schema(
    conn: &mut rusqlite::Connection,
    from_version: i32,
) -> Result<(), StateStoreError> {
    let tx = conn.unchecked_transaction()?;
    migrate_schema_on(&tx, from_version)?;
    tx.commit()?;
    Ok(())
}

fn migrate_schema_on(
    tx: &rusqlite::Transaction<'_>,
    from_version: i32,
) -> Result<(), StateStoreError> {
    if from_version < 2 {
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS app_secrets (
                app TEXT NOT NULL PRIMARY KEY,
                encrypted_data BLOB NOT NULL
            );",
        )?;
    }

    if from_version < 3 {
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS app_storages (
                app TEXT NOT NULL PRIMARY KEY,
                encrypted_data BLOB NOT NULL
            );",
        )?;
    }

    if from_version < 5 {
        tx.execute_batch("ALTER TABLE apps ADD COLUMN source_ip TEXT NOT NULL DEFAULT 'auto';")?;
    }

    if from_version < 6 {
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS app_ssl (
                app TEXT NOT NULL PRIMARY KEY,
                encrypted_data BLOB NOT NULL
            );",
        )?;
    }

    if from_version < 7 {
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS app_backups (
                app TEXT NOT NULL PRIMARY KEY,
                encrypted_data BLOB NOT NULL
            );",
        )?;
    }

    if from_version < 8 {
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS app_runtime_credentials (
                app TEXT NOT NULL PRIMARY KEY,
                encrypted_data BLOB NOT NULL
            );",
        )?;
    }

    ensure_schema_objects(tx)?;
    ensure_default_rows(tx)?;
    tx.execute_batch(&format!("PRAGMA user_version = {STATE_SCHEMA_VERSION};"))?;
    Ok(())
}

fn ensure_schema_objects(conn: &rusqlite::Connection) -> Result<(), StateStoreError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS apps (
            name TEXT NOT NULL,
            environment TEXT NOT NULL,
            version TEXT NOT NULL,
            min_instances INTEGER NOT NULL,
            max_instances INTEGER NOT NULL,
            source_ip TEXT NOT NULL DEFAULT 'auto',
            PRIMARY KEY (name, environment)
        );

        CREATE TABLE IF NOT EXISTS app_routes (
            name TEXT NOT NULL,
            environment TEXT NOT NULL,
            route TEXT NOT NULL,
            PRIMARY KEY (name, environment, route),
            FOREIGN KEY(name, environment) REFERENCES apps(name, environment) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS server_state (
            id INTEGER PRIMARY KEY CHECK(id = 1),
            server_mode TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS upgrade_lock (
            id INTEGER PRIMARY KEY CHECK(id = 1),
            owner TEXT NOT NULL,
            acquired_at_unix_secs INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS app_secrets (
            app TEXT NOT NULL PRIMARY KEY,
            encrypted_data BLOB NOT NULL
        );

        CREATE TABLE IF NOT EXISTS app_runtime_credentials (
            app TEXT NOT NULL PRIMARY KEY,
            encrypted_data BLOB NOT NULL
        );

        CREATE TABLE IF NOT EXISTS app_storages (
            app TEXT NOT NULL PRIMARY KEY,
            encrypted_data BLOB NOT NULL
        );

        CREATE TABLE IF NOT EXISTS app_ssl (
            app TEXT NOT NULL PRIMARY KEY,
            encrypted_data BLOB NOT NULL
        );

        CREATE TABLE IF NOT EXISTS app_compose_pending (
            app TEXT NOT NULL PRIMARY KEY,
            project TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS app_backups (
            app TEXT NOT NULL PRIMARY KEY,
            encrypted_data BLOB NOT NULL
        );",
    )?;
    Ok(())
}

fn ensure_default_rows(conn: &rusqlite::Connection) -> Result<(), StateStoreError> {
    conn.execute(
        "INSERT INTO server_state (id, server_mode)
         VALUES (1, 'normal')
         ON CONFLICT(id) DO NOTHING;",
        [],
    )?;

    Ok(())
}
