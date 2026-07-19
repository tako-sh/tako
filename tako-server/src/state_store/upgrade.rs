use tako_core::UpgradeMode;

use super::{SqliteStateStore, StateStoreError, block_on};

impl SqliteStateStore {
    pub fn set_server_mode(&self, mode: UpgradeMode) -> Result<(), StateStoreError> {
        let conn = self.lock_conn()?;
        block_on(conn.execute(
            "UPDATE server_state SET server_mode = ?1 WHERE id = 1;",
            (server_mode_to_str(mode),),
        ))?;
        Ok(())
    }

    pub fn server_mode(&self) -> Result<UpgradeMode, StateStoreError> {
        let conn = self.lock_conn()?;
        let mode_str: Option<String> = block_on(async {
            let mut rows = conn
                .query("SELECT server_mode FROM server_state WHERE id = 1;", ())
                .await?;
            match rows.next().await? {
                Some(row) => Ok::<_, StateStoreError>(Some(row.get(0)?)),
                None => Ok(None),
            }
        })?;

        match mode_str {
            Some(s) => server_mode_from_str(&s),
            None => Ok(UpgradeMode::Normal),
        }
    }

    /// Stale lock threshold: locks older than this are force-acquired.
    pub(crate) const UPGRADE_LOCK_STALE_SECS: i64 = 600; // 10 minutes

    pub fn try_acquire_upgrade_lock(&self, owner: &str) -> Result<bool, StateStoreError> {
        let conn = self.lock_conn()?;
        block_on(async {
            let tx = conn.unchecked_transaction().await?;
            let result: Result<bool, StateStoreError> = async {
                let mut rows = tx
                    .query(
                        "SELECT owner, acquired_at_unix_secs FROM upgrade_lock WHERE id = 1;",
                        (),
                    )
                    .await?;
                let existing: Option<(String, i64)> = match rows.next().await? {
                    Some(row) => Some((row.get(0)?, row.get(1)?)),
                    None => None,
                };
                drop(rows);

                let mut rows = tx
                    .query("SELECT CAST(strftime('%s','now') AS INTEGER);", ())
                    .await?;
                let now: i64 = rows
                    .next()
                    .await?
                    .ok_or_else(|| StateStoreError::Sqlite("strftime returned no row".into()))?
                    .get(0)?;
                drop(rows);

                match existing {
                    Some((ref existing_owner, _)) if existing_owner == owner => Ok(true),
                    Some((_, acquired_at)) if now - acquired_at > Self::UPGRADE_LOCK_STALE_SECS => {
                        // Stale lock: force-acquire by replacing it.
                        tx.execute(
                            "UPDATE upgrade_lock SET owner = ?1, acquired_at_unix_secs = ?2 WHERE id = 1;",
                            (owner, now),
                        )
                        .await?;
                        Ok(true)
                    }
                    Some(_) => Ok(false),
                    None => {
                        tx.execute(
                            "INSERT INTO upgrade_lock (id, owner, acquired_at_unix_secs)
                             VALUES (1, ?1, CAST(strftime('%s','now') AS INTEGER));",
                            (owner,),
                        )
                        .await?;
                        Ok(true)
                    }
                }
            }
            .await;
            tako_sqlite::commit_or_rollback(tx, result).await
        })
    }

    pub fn release_upgrade_lock(&self, owner: &str) -> Result<bool, StateStoreError> {
        let conn = self.lock_conn()?;
        block_on(async {
            let tx = conn.unchecked_transaction().await?;
            let result: Result<bool, StateStoreError> = async {
                let mut rows = tx
                    .query("SELECT owner FROM upgrade_lock WHERE id = 1;", ())
                    .await?;
                let existing: Option<String> = match rows.next().await? {
                    Some(row) => Some(row.get(0)?),
                    None => None,
                };
                drop(rows);

                match existing {
                    Some(existing) if existing == owner => {
                        tx.execute("DELETE FROM upgrade_lock WHERE id = 1;", ())
                            .await?;
                        Ok(true)
                    }
                    _ => Ok(false),
                }
            }
            .await;
            tako_sqlite::commit_or_rollback(tx, result).await
        })
    }

    pub fn upgrade_lock_owner(&self) -> Result<Option<String>, StateStoreError> {
        let conn = self.lock_conn()?;
        block_on(async {
            let mut rows = conn
                .query("SELECT owner FROM upgrade_lock WHERE id = 1;", ())
                .await?;
            match rows.next().await? {
                Some(row) => Ok(Some(row.get(0)?)),
                None => Ok(None),
            }
        })
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
