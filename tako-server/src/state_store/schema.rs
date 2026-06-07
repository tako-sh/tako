use super::{STATE_SCHEMA_VERSION, SqliteStateStore, StateStoreError};

impl SqliteStateStore {
    pub fn init(&self) -> Result<(), StateStoreError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| StateStoreError::Sqlite(format!("create db parent: {e}")))?;
        }

        let conn = self.open_connection()?;
        let version: i32 = conn
            .query_row("PRAGMA user_version;", [], |row| row.get(0))
            .map_err(StateStoreError::from)?;

        if version > STATE_SCHEMA_VERSION {
            return Err(StateStoreError::UnsupportedSchemaVersion { found: version });
        }

        if version == 0 {
            self.initialize_schema(&conn)?;
        } else if version < STATE_SCHEMA_VERSION {
            self.migrate_schema(&conn, version)?;
        } else {
            self.ensure_schema_objects(&conn)?;
            self.ensure_default_rows(&conn)?;
        }

        Ok(())
    }

    fn ensure_schema_objects(&self, conn: &rusqlite::Connection) -> Result<(), StateStoreError> {
        self.ensure_schema_objects_on(conn)
    }

    fn initialize_schema(&self, conn: &rusqlite::Connection) -> Result<(), StateStoreError> {
        let tx = conn
            .unchecked_transaction()
            .map_err(StateStoreError::from)?;
        self.ensure_schema_objects_on(&tx)?;
        self.ensure_default_rows_on(&tx)?;
        tx.execute_batch(&format!("PRAGMA user_version = {STATE_SCHEMA_VERSION};"))
            .map_err(StateStoreError::from)?;
        tx.commit().map_err(StateStoreError::from)?;
        Ok(())
    }

    fn migrate_schema(
        &self,
        conn: &rusqlite::Connection,
        from_version: i32,
    ) -> Result<(), StateStoreError> {
        let tx = conn
            .unchecked_transaction()
            .map_err(StateStoreError::from)?;

        if from_version < 2 {
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS app_secrets (
                    app TEXT NOT NULL PRIMARY KEY,
                    encrypted_data BLOB NOT NULL
                );",
            )
            .map_err(StateStoreError::from)?;
        }

        if from_version < 3 {
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS app_storages (
                    app TEXT NOT NULL PRIMARY KEY,
                    encrypted_data BLOB NOT NULL
                );",
            )
            .map_err(StateStoreError::from)?;
        }

        if from_version < 5 {
            tx.execute_batch("ALTER TABLE apps ADD COLUMN source_ip TEXT NOT NULL DEFAULT 'auto';")
                .map_err(StateStoreError::from)?;
        }

        if from_version < 6 {
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS app_ssl (
                    app TEXT NOT NULL PRIMARY KEY,
                    encrypted_data BLOB NOT NULL
                );",
            )
            .map_err(StateStoreError::from)?;
        }

        if from_version < 7 {
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS app_backups (
                    app TEXT NOT NULL PRIMARY KEY,
                    encrypted_data BLOB NOT NULL
                );",
            )
            .map_err(StateStoreError::from)?;
        }

        if from_version < 8 {
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS app_runtime_credentials (
                    app TEXT NOT NULL PRIMARY KEY,
                    encrypted_data BLOB NOT NULL
                );",
            )
            .map_err(StateStoreError::from)?;
        }

        self.ensure_default_rows_on(&tx)?;
        tx.execute_batch(&format!("PRAGMA user_version = {STATE_SCHEMA_VERSION};"))
            .map_err(StateStoreError::from)?;
        tx.commit().map_err(StateStoreError::from)?;
        Ok(())
    }

    fn ensure_schema_objects_on(&self, conn: &rusqlite::Connection) -> Result<(), StateStoreError> {
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
        .map_err(StateStoreError::from)?;
        Ok(())
    }

    fn ensure_default_rows(&self, conn: &rusqlite::Connection) -> Result<(), StateStoreError> {
        self.ensure_default_rows_on(conn)
    }

    fn ensure_default_rows_on(&self, conn: &rusqlite::Connection) -> Result<(), StateStoreError> {
        conn.execute(
            "INSERT INTO server_state (id, server_mode)
             VALUES (1, 'normal')
             ON CONFLICT(id) DO NOTHING;",
            [],
        )
        .map_err(StateStoreError::from)?;

        Ok(())
    }
}
