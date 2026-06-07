use ::postgres::{Client, NoTls};
use parking_lot::Mutex;

use crate::{ChannelAuthResponse, ChannelError, ChannelMessage, ChannelPublishPayload};

use super::{channel_message_from_row, now_unix_ms};

pub(super) struct PostgresChannelStore {
    client: Mutex<Client>,
    schema: String,
    app_id: String,
}

impl PostgresChannelStore {
    pub(super) fn open(url: &str, schema: &str, app_id: &str) -> Result<Self, ChannelError> {
        validate_pg_identifier(schema)?;
        let mut client =
            Client::connect(url, NoTls).map_err(|e| ChannelError::Storage(e.to_string()))?;
        init_postgres(&mut client, schema)?;
        Ok(Self {
            client: Mutex::new(client),
            schema: schema.to_string(),
            app_id: app_id.to_string(),
        })
    }

    pub(super) fn append(
        &self,
        channel: &str,
        payload: &ChannelPublishPayload,
    ) -> Result<ChannelMessage, ChannelError> {
        let data_json = serde_json::to_string(&payload.data)
            .map_err(|e| ChannelError::BadRequest(format!("serialize payload: {e}")))?;
        let mut client = self.client.lock();
        let mut tx = client
            .transaction()
            .map_err(|e| ChannelError::Storage(e.to_string()))?;
        tx.execute(
            &format!(
                "UPDATE {}.channel_metadata
                 SET last_activity_unix_ms = $3
                 WHERE app_id = $1 AND channel = $2",
                self.schema
            ),
            &[&self.app_id, &channel, &now_unix_ms()],
        )
        .map_err(|e| ChannelError::Storage(e.to_string()))?;
        let row = tx
            .query_one(
                &format!(
                    "INSERT INTO {}.channel_messages (app_id, channel, type, data_json)
                     VALUES ($1, $2, $3, $4)
                     RETURNING id",
                    self.schema
                ),
                &[&self.app_id, &channel, &payload.r#type, &data_json],
            )
            .map_err(|e| ChannelError::Storage(e.to_string()))?;
        let id: i64 = row.get(0);
        tx.commit()
            .map_err(|e| ChannelError::Storage(e.to_string()))?;

        Ok(ChannelMessage {
            id: id.to_string(),
            channel: channel.to_string(),
            r#type: payload.r#type.clone(),
            data: payload.data.clone(),
        })
    }

    pub(super) fn read_after(
        &self,
        channel: &str,
        after: Option<i64>,
        limit: u32,
    ) -> Result<Vec<ChannelMessage>, ChannelError> {
        let mut client = self.client.lock();
        let rows = client
            .query(
                &format!(
                    "SELECT id, channel, type, data_json
                     FROM {}.channel_messages
                     WHERE app_id = $1 AND channel = $2 AND ($3::BIGINT IS NULL OR id > $3)
                     ORDER BY id ASC
                     LIMIT $4",
                    self.schema
                ),
                &[&self.app_id, &channel, &after, &i64::from(limit)],
            )
            .map_err(|e| ChannelError::Storage(e.to_string()))?;

        rows.into_iter()
            .map(|row| {
                channel_message_from_row((
                    row.get::<_, i64>(0),
                    row.get::<_, String>(1),
                    row.get::<_, String>(2),
                    row.get::<_, String>(3),
                ))
            })
            .collect()
    }

    pub(super) fn replay_cursor(
        &self,
        channel: &str,
        requested: Option<i64>,
    ) -> Result<Option<i64>, ChannelError> {
        let mut client = self.client.lock();
        let latest = self.message_id(&mut client, channel, "MAX")?;
        let Some(requested) = requested else {
            return Ok(latest);
        };

        let Some(oldest) = self.message_id(&mut client, channel, "MIN")? else {
            return Ok(Some(requested));
        };

        if requested < oldest.saturating_sub(1) {
            return Err(ChannelError::StaleCursor);
        }

        Ok(Some(requested))
    }

    pub(super) fn sync_channel(
        &self,
        channel: &str,
        auth: &ChannelAuthResponse,
    ) -> Result<(), ChannelError> {
        let mut client = self.client.lock();
        let now = now_unix_ms();
        client
            .execute(
                &format!(
                    "INSERT INTO {}.channel_metadata (
                        app_id,
                        channel,
                        replay_window_ms,
                        inactivity_ttl_ms,
                        keepalive_interval_ms,
                        max_connection_lifetime_ms,
                        last_activity_unix_ms
                    ) VALUES ($1, $2, $3, $4, $5, $6, $7)
                    ON CONFLICT(app_id, channel) DO UPDATE SET
                        replay_window_ms = excluded.replay_window_ms,
                        inactivity_ttl_ms = excluded.inactivity_ttl_ms,
                        keepalive_interval_ms = excluded.keepalive_interval_ms,
                        max_connection_lifetime_ms = excluded.max_connection_lifetime_ms,
                        last_activity_unix_ms = excluded.last_activity_unix_ms",
                    self.schema
                ),
                &[
                    &self.app_id,
                    &channel,
                    &(auth.replay_window_ms as i64),
                    &(auth.inactivity_ttl_ms as i64),
                    &(auth.keepalive_interval_ms as i64),
                    &(auth.max_connection_lifetime_ms as i64),
                    &now,
                ],
            )
            .map_err(|e| ChannelError::Storage(e.to_string()))?;

        if auth.replay_window_ms > 0 {
            let cutoff = now - auth.replay_window_ms as i64;
            client
                .execute(
                    &format!(
                        "DELETE FROM {}.channel_messages
                         WHERE app_id = $1 AND channel = $2 AND created_at_unix_ms < $3",
                        self.schema
                    ),
                    &[&self.app_id, &channel, &cutoff],
                )
                .map_err(|e| ChannelError::Storage(e.to_string()))?;
        }

        client
            .execute(
                &format!(
                    "DELETE FROM {}.channel_messages
                     WHERE app_id = $1
                       AND channel IN (
                        SELECT channel
                        FROM {}.channel_metadata
                        WHERE app_id = $1
                          AND inactivity_ttl_ms > 0
                          AND last_activity_unix_ms < ($2 - inactivity_ttl_ms)
                     )",
                    self.schema, self.schema
                ),
                &[&self.app_id, &now],
            )
            .map_err(|e| ChannelError::Storage(e.to_string()))?;
        client
            .execute(
                &format!(
                    "DELETE FROM {}.channel_metadata
                     WHERE app_id = $1
                       AND inactivity_ttl_ms > 0
                       AND last_activity_unix_ms < ($2 - inactivity_ttl_ms)",
                    self.schema
                ),
                &[&self.app_id, &now],
            )
            .map_err(|e| ChannelError::Storage(e.to_string()))?;

        Ok(())
    }

    fn message_id(
        &self,
        client: &mut Client,
        channel: &str,
        aggregate: &str,
    ) -> Result<Option<i64>, ChannelError> {
        let sql = format!(
            "SELECT {aggregate}(id) FROM {}.channel_messages WHERE app_id = $1 AND channel = $2",
            self.schema
        );
        client
            .query_one(&sql, &[&self.app_id, &channel])
            .map(|row| row.get(0))
            .map_err(|e| ChannelError::Storage(e.to_string()))
    }
}

fn init_postgres(client: &mut Client, schema: &str) -> Result<(), ChannelError> {
    client
        .batch_execute(&format!(
            "CREATE SCHEMA IF NOT EXISTS {schema};
             CREATE TABLE IF NOT EXISTS {schema}.channel_messages (
                 id BIGSERIAL PRIMARY KEY,
                 app_id TEXT NOT NULL,
                 channel TEXT NOT NULL,
                 type TEXT NOT NULL,
                 data_json TEXT NOT NULL,
                 created_at_unix_ms BIGINT NOT NULL DEFAULT ((extract(epoch from clock_timestamp()) * 1000)::BIGINT)
             );
             CREATE INDEX IF NOT EXISTS idx_channel_messages_app_channel_id
               ON {schema}.channel_messages(app_id, channel, id);
             CREATE TABLE IF NOT EXISTS {schema}.channel_metadata (
                 app_id TEXT NOT NULL,
                 channel TEXT NOT NULL,
                 replay_window_ms BIGINT NOT NULL,
                 inactivity_ttl_ms BIGINT NOT NULL,
                 keepalive_interval_ms BIGINT NOT NULL,
                 max_connection_lifetime_ms BIGINT NOT NULL,
                 last_activity_unix_ms BIGINT NOT NULL,
                 PRIMARY KEY(app_id, channel)
             );"
        ))
        .map_err(|e| ChannelError::Storage(e.to_string()))?;
    Ok(())
}

fn validate_pg_identifier(identifier: &str) -> Result<(), ChannelError> {
    let mut chars = identifier.chars();
    let Some(first) = chars.next() else {
        return Err(ChannelError::Storage(
            "postgres schema name cannot be empty".to_string(),
        ));
    };
    if !(first == '_' || first.is_ascii_alphabetic())
        || !chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
    {
        return Err(ChannelError::Storage(format!(
            "invalid postgres schema name '{identifier}'"
        )));
    }
    Ok(())
}
