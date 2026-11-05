use super::{SqliteStateStore, StateStoreError};

impl SqliteStateStore {
    pub fn drain_requested(&self) -> Result<bool, StateStoreError> {
        let conn = self.lock_conn()?;
        Ok(conn.query_row(
            "SELECT requested FROM server_drain WHERE id = 1",
            [],
            |row| row.get(0),
        )?)
    }

    pub fn set_drain_requested(&self, requested: bool) -> Result<(), StateStoreError> {
        let conn = self.lock_conn()?;
        let changed = conn.execute(
            "UPDATE server_drain SET requested = ?1 WHERE id = 1",
            [requested],
        )?;
        if changed != 1 {
            return Err(StateStoreError::InvalidData(
                "missing server drain state".into(),
            ));
        }
        Ok(())
    }
}
