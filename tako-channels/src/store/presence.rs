use super::{ChannelStore, ChannelStoreBackend};
use crate::{ChannelError, PresenceMember};

const LEASE_MS: i64 = 45_000;

fn storage_error(error: impl std::fmt::Display) -> ChannelError {
    ChannelError::Storage(error.to_string())
}

impl ChannelStore {
    pub fn update_presence(
        &self,
        channel: &str,
        member: &PresenceMember,
        now: i64,
    ) -> Result<(), ChannelError> {
        let data = serde_json::to_string(&member.data).map_err(storage_error)?;
        if data.len() > 16_384 {
            return Err(ChannelError::BadRequest(
                "Presence data must be at most 16 KiB".into(),
            ));
        }
        let encoded = serde_json::to_string(member).map_err(storage_error)?;
        let expires = now + LEASE_MS;
        match self.backend.as_ref() {
            ChannelStoreBackend::Sqlite(store) => {
                store.conn.lock().execute(
                    "INSERT INTO presence (channel, connection_id, member_json, expires_at) VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(channel, connection_id) DO UPDATE SET member_json = excluded.member_json, expires_at = excluded.expires_at",
                    rusqlite::params![channel, member.connection_id, encoded, expires],
                ).map_err(storage_error)?;
            }
            ChannelStoreBackend::Postgres(store) => {
                store.client.lock().execute(&format!(
                    "INSERT INTO {}.presence (app_id, channel, connection_id, member_json, expires_at) VALUES ($1, $2, $3, $4, $5)
                     ON CONFLICT(app_id, channel, connection_id) DO UPDATE SET member_json = excluded.member_json, expires_at = excluded.expires_at", store.schema),
                    &[&store.app_id, &channel, &member.connection_id, &encoded, &expires],
                ).map_err(storage_error)?;
            }
        }
        Ok(())
    }

    pub fn presence(&self, channel: &str, now: i64) -> Result<Vec<PresenceMember>, ChannelError> {
        let rows: Vec<String> = match self.backend.as_ref() {
            ChannelStoreBackend::Sqlite(store) => {
                let conn = store.conn.lock();
                let mut stmt = conn.prepare_cached(
                    "SELECT member_json FROM presence WHERE channel = ?1 AND expires_at > ?2 ORDER BY connection_id"
                ).map_err(storage_error)?;
                stmt.query_map(rusqlite::params![channel, now], |row| row.get(0))
                    .map_err(storage_error)?.collect::<Result<Vec<_>, _>>().map_err(storage_error)?
            }
            ChannelStoreBackend::Postgres(store) => {
                store.client.lock().query(&format!(
                    "SELECT member_json FROM {}.presence WHERE app_id = $1 AND channel = $2 AND expires_at > $3 ORDER BY connection_id", store.schema),
                    &[&store.app_id, &channel, &now],
                ).map_err(storage_error)?.into_iter().map(|row| row.get(0)).collect()
            }
        };
        rows.into_iter()
            .map(|row| serde_json::from_str(&row).map_err(storage_error))
            .collect()
    }

    pub fn remove_presence(&self, channel: &str, connection_id: &str) -> Result<(), ChannelError> {
        match self.backend.as_ref() {
            ChannelStoreBackend::Sqlite(store) => {
                store
                    .conn
                    .lock()
                    .execute(
                        "DELETE FROM presence WHERE channel = ?1 AND connection_id = ?2",
                        [channel, connection_id],
                    )
                    .map_err(storage_error)?;
            }
            ChannelStoreBackend::Postgres(store) => {
                store.client.lock().execute(&format!("DELETE FROM {}.presence WHERE app_id = $1 AND channel = $2 AND connection_id = $3", store.schema),
                    &[&store.app_id, &channel, &connection_id]).map_err(storage_error)?;
            }
        }
        Ok(())
    }
}
