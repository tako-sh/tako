use super::{STATE_SCHEMA_VERSION, SqliteStateStore, StateStoreError, block_on};

impl SqliteStateStore {
    pub fn init(&self) -> Result<(), StateStoreError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| StateStoreError::Sqlite(format!("create db parent: {e}")))?;
        }

        let conn = self.lock_conn()?;
        block_on(async {
            let mut rows = conn.query("PRAGMA user_version;", ()).await?;
            let version: i32 = rows
                .next()
                .await?
                .ok_or_else(|| StateStoreError::Sqlite("user_version returned no row".into()))?
                .get::<i64>(0)? as i32;
            drop(rows);

            if version > STATE_SCHEMA_VERSION {
                return Err(StateStoreError::UnsupportedSchemaVersion { found: version });
            }

            if version == 0 {
                initialize_schema(&conn).await
            } else if version < STATE_SCHEMA_VERSION {
                migrate_schema(&conn, version).await
            } else {
                ensure_schema_objects(&conn).await?;
                ensure_default_rows(&conn).await
            }
        })
    }
}

async fn initialize_schema(conn: &turso::Connection) -> Result<(), StateStoreError> {
    let tx = conn.unchecked_transaction().await?;
    let result: Result<(), StateStoreError> = async {
        ensure_schema_objects(&tx).await?;
        ensure_default_rows(&tx).await?;
        tx.execute_batch(&format!("PRAGMA user_version = {STATE_SCHEMA_VERSION};"))
            .await?;
        Ok(())
    }
    .await;
    tako_sqlite::commit_or_rollback(tx, result).await
}

async fn migrate_schema(
    conn: &turso::Connection,
    from_version: i32,
) -> Result<(), StateStoreError> {
    let tx = conn.unchecked_transaction().await?;
    let result = migrate_schema_on(&tx, from_version).await;
    tako_sqlite::commit_or_rollback(tx, result).await
}

async fn migrate_schema_on(
    tx: &turso::Connection,
    from_version: i32,
) -> Result<(), StateStoreError> {
    if from_version < 2 {
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS app_secrets (
                app TEXT NOT NULL PRIMARY KEY,
                encrypted_data BLOB NOT NULL
            );",
        )
        .await?;
    }

    if from_version < 3 {
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS app_storages (
                app TEXT NOT NULL PRIMARY KEY,
                encrypted_data BLOB NOT NULL
            );",
        )
        .await?;
    }

    if from_version < 5 {
        tx.execute_batch("ALTER TABLE apps ADD COLUMN source_ip TEXT NOT NULL DEFAULT 'auto';")
            .await?;
    }

    if from_version < 6 {
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS app_ssl (
                app TEXT NOT NULL PRIMARY KEY,
                encrypted_data BLOB NOT NULL
            );",
        )
        .await?;
    }

    if from_version < 7 {
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS app_backups (
                app TEXT NOT NULL PRIMARY KEY,
                encrypted_data BLOB NOT NULL
            );",
        )
        .await?;
    }

    if from_version < 8 {
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS app_runtime_credentials (
                app TEXT NOT NULL PRIMARY KEY,
                encrypted_data BLOB NOT NULL
            );",
        )
        .await?;
    }

    ensure_default_rows(tx).await?;
    tx.execute_batch(&format!("PRAGMA user_version = {STATE_SCHEMA_VERSION};"))
        .await?;
    Ok(())
}

async fn ensure_schema_objects(conn: &turso::Connection) -> Result<(), StateStoreError> {
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

        CREATE TABLE IF NOT EXISTS app_backups (
            app TEXT NOT NULL PRIMARY KEY,
            encrypted_data BLOB NOT NULL
        );",
    )
    .await?;
    Ok(())
}

async fn ensure_default_rows(conn: &turso::Connection) -> Result<(), StateStoreError> {
    conn.execute(
        "INSERT INTO server_state (id, server_mode)
         VALUES (1, 'normal')
         ON CONFLICT(id) DO NOTHING;",
        (),
    )
    .await?;

    Ok(())
}
