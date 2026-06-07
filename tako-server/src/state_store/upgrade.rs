use rusqlite::OptionalExtension;
use tako_core::UpgradeMode;

use super::{SqliteStateStore, StateStoreError};

impl SqliteStateStore {
    pub fn set_server_mode(&self, mode: UpgradeMode) -> Result<(), StateStoreError> {
        let conn = self.open_connection()?;
        conn.execute(
            "UPDATE server_state SET server_mode = ?1 WHERE id = 1;",
            rusqlite::params![server_mode_to_str(mode)],
        )
        .map_err(StateStoreError::from)?;
        Ok(())
    }

    pub fn server_mode(&self) -> Result<UpgradeMode, StateStoreError> {
        let conn = self.open_connection()?;
        let mode_str: Option<String> = conn
            .query_row(
                "SELECT server_mode FROM server_state WHERE id = 1;",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(StateStoreError::from)?;

        match mode_str {
            Some(s) => server_mode_from_str(&s),
            None => Ok(UpgradeMode::Normal),
        }
    }

    /// Stale lock threshold: locks older than this are force-acquired.
    pub(crate) const UPGRADE_LOCK_STALE_SECS: i64 = 600; // 10 minutes

    pub fn try_acquire_upgrade_lock(&self, owner: &str) -> Result<bool, StateStoreError> {
        let conn = self.open_connection()?;
        let tx = conn
            .unchecked_transaction()
            .map_err(StateStoreError::from)?;

        let existing: Option<(String, i64)> = tx
            .query_row(
                "SELECT owner, acquired_at_unix_secs FROM upgrade_lock WHERE id = 1;",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(StateStoreError::from)?;

        let now: i64 = tx
            .query_row("SELECT CAST(strftime('%s','now') AS INTEGER);", [], |row| {
                row.get(0)
            })
            .map_err(StateStoreError::from)?;

        let acquired = match existing {
            Some((ref existing_owner, _)) if existing_owner == owner => true,
            Some((_, acquired_at)) if now - acquired_at > Self::UPGRADE_LOCK_STALE_SECS => {
                // Stale lock: force-acquire by replacing it.
                tx.execute(
                    "UPDATE upgrade_lock SET owner = ?1, acquired_at_unix_secs = ?2 WHERE id = 1;",
                    rusqlite::params![owner, now],
                )
                .map_err(StateStoreError::from)?;
                true
            }
            Some(_) => false,
            None => {
                tx.execute(
                    "INSERT INTO upgrade_lock (id, owner, acquired_at_unix_secs)
                     VALUES (1, ?1, CAST(strftime('%s','now') AS INTEGER));",
                    rusqlite::params![owner],
                )
                .map_err(StateStoreError::from)?;
                true
            }
        };

        tx.commit().map_err(StateStoreError::from)?;
        Ok(acquired)
    }

    pub fn release_upgrade_lock(&self, owner: &str) -> Result<bool, StateStoreError> {
        let conn = self.open_connection()?;
        let tx = conn
            .unchecked_transaction()
            .map_err(StateStoreError::from)?;

        let existing: Option<String> = tx
            .query_row("SELECT owner FROM upgrade_lock WHERE id = 1;", [], |row| {
                row.get(0)
            })
            .optional()
            .map_err(StateStoreError::from)?;

        let released = match existing {
            Some(existing) if existing == owner => {
                tx.execute("DELETE FROM upgrade_lock WHERE id = 1;", [])
                    .map_err(StateStoreError::from)?;
                true
            }
            _ => false,
        };

        tx.commit().map_err(StateStoreError::from)?;
        Ok(released)
    }

    pub fn upgrade_lock_owner(&self) -> Result<Option<String>, StateStoreError> {
        let conn = self.open_connection()?;
        conn.query_row("SELECT owner FROM upgrade_lock WHERE id = 1;", [], |row| {
            row.get(0)
        })
        .optional()
        .map_err(StateStoreError::from)
    }
}

fn server_mode_to_str(mode: UpgradeMode) -> &'static str {
    match mode {
        UpgradeMode::Normal => "normal",
        UpgradeMode::Upgrading => "upgrading",
    }
}

fn server_mode_from_str(value: &str) -> Result<UpgradeMode, StateStoreError> {
    match value {
        "normal" => Ok(UpgradeMode::Normal),
        "upgrading" => Ok(UpgradeMode::Upgrading),
        other => Err(StateStoreError::InvalidData(format!(
            "unknown server_mode value: {}",
            other
        ))),
    }
}
