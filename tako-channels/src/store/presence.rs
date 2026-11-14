use super::postgres::DATABASE_NOW_MS;
use super::{ChannelStore, ChannelStoreBackend};
use crate::{ChannelError, PresenceMember};

pub(super) const LEASE_MS: i64 = 45_000;

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
        self.ensure_writable()?;
        let data = serde_json::to_string(&member.data).map_err(storage_error)?;
        if data.len() > 16_384 {
            return Err(ChannelError::BadRequest(
                "Presence data must be at most 16 KiB".into(),
            ));
        }
        let encoded = serde_json::to_string(member).map_err(storage_error)?;
        match self.backend.as_ref() {
            ChannelStoreBackend::Sqlite(store) => {
                store.conn.lock().execute(
                    "INSERT INTO presence (channel, connection_id, member_json, expires_at) VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(channel, connection_id) DO UPDATE SET member_json = excluded.member_json, expires_at = excluded.expires_at",
                    rusqlite::params![channel, member.connection_id, encoded, now.saturating_add(LEASE_MS)],
                ).map_err(storage_error)?;
            }
            ChannelStoreBackend::Postgres(store) => {
                store.client()?.execute(&format!(
                    "INSERT INTO {}.presence (app_id, channel, connection_id, member_json, expires_at) VALUES ($1, $2, $3, $4, {DATABASE_NOW_MS} + {LEASE_MS})
                     ON CONFLICT(app_id, channel, connection_id) DO UPDATE SET member_json = excluded.member_json, expires_at = excluded.expires_at", store.schema),
                    &[&store.app_id, &channel, &member.connection_id, &encoded],
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
                store.client()?.query(&format!(
                    "SELECT member_json FROM {}.presence WHERE app_id = $1 AND channel = $2 AND expires_at > {DATABASE_NOW_MS} ORDER BY connection_id", store.schema),
                    &[&store.app_id, &channel],
                ).map_err(storage_error)?.into_iter().map(|row| row.get(0)).collect()
            }
        };
        rows.into_iter()
            .map(|row| serde_json::from_str(&row).map_err(storage_error))
            .collect()
    }

    pub fn remove_presence(&self, channel: &str, connection_id: &str) -> Result<(), ChannelError> {
        self.ensure_writable()?;
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
                store.client()?.execute(&format!("DELETE FROM {}.presence WHERE app_id = $1 AND channel = $2 AND connection_id = $3", store.schema),
                    &[&store.app_id, &channel, &connection_id]).map_err(storage_error)?;
            }
        }
        Ok(())
    }
}
