use ::postgres::{Client, NoTls};
use parking_lot::Mutex;

use crate::{ChannelAuthResponse, ChannelError, ChannelMessage, ChannelPublishPayload};

use super::{channel_message_from_row, now_unix_ms, resolve_replay_cursor};

pub(super) struct PostgresChannelStore {
    pub(super) client: Mutex<Client>,
    pub(super) schema: String,
    pub(super) app_id: String,
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
        // Serialize allocation and commit per app across servers. BIGSERIAL
        // alone permits a higher ID to commit before a lower one, which would
        // make a cursor-based subscriber skip the late commit.
        tx.query_one(
            "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))",
            &[&format!("{}:{}", self.schema, self.app_id)],
        )
        .map_err(|e| ChannelError::Storage(e.to_string()))?;
        tx.execute(&format!(
            "INSERT INTO {}.channel_metadata (app_id, channel, replay_window_ms, inactivity_ttl_ms, keepalive_interval_ms, max_connection_lifetime_ms, last_activity_unix_ms)
             VALUES ($1, $2, 600000, 0, 25000, 7200000, $3) ON CONFLICT(app_id, channel) DO NOTHING", self.schema
        ), &[&self.app_id, &channel, &now_unix_ms()]).map_err(|e| ChannelError::Storage(e.to_string()))?;
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
        tx.execute(&format!("UPDATE {}.channel_metadata SET latest_message_id = $3 WHERE app_id = $1 AND channel = $2", self.schema), &[&self.app_id, &channel, &id])
            .map_err(|e| ChannelError::Storage(e.to_string()))?;
        tx.commit()
            .map_err(|e| ChannelError::Storage(e.to_string()))?;

        Ok(ChannelMessage {
            id: id.to_string(),
            event_id: None,
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
        let mut tx = client
            .build_transaction()
            .isolation_level(::postgres::IsolationLevel::RepeatableRead)
            .start()
            .map_err(|e| ChannelError::Storage(e.to_string()))?;
        if after.is_some() {
            let bounds = tx.query_one(&format!(
                "SELECT COALESCE((SELECT highest_deleted_message_id FROM {}.channel_metadata WHERE app_id = $1 AND channel = $2), 0),
                        (SELECT NULLIF(latest_message_id, 0) FROM {}.channel_metadata WHERE app_id = $1 AND channel = $2)", self.schema, self.schema
            ), &[&self.app_id, &channel]).map_err(|e| ChannelError::Storage(e.to_string()))?;
            resolve_replay_cursor(after, bounds.get(0), bounds.get(1))?;
        }
        let rows = tx
            .query(
                &format!(
                    "SELECT id, channel, type, data_json
                     FROM {}.channel_messages
                     WHERE app_id = $1 AND channel = $2 AND id > COALESCE($3::BIGINT, 0)
                     ORDER BY id ASC
                     LIMIT $4",
                    self.schema
                ),
                &[&self.app_id, &channel, &after, &i64::from(limit)],
            )
            .map_err(|e| ChannelError::Storage(e.to_string()))?;

        tx.commit()
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
        let row = client.query_one(&format!(
            "SELECT COALESCE((SELECT highest_deleted_message_id FROM {}.channel_metadata WHERE app_id = $1 AND channel = $2), 0),
                        (SELECT NULLIF(latest_message_id, 0) FROM {}.channel_metadata WHERE app_id = $1 AND channel = $2)", self.schema, self.schema
        ), &[&self.app_id, &channel]).map_err(|e| ChannelError::Storage(e.to_string()))?;
        resolve_replay_cursor(requested, row.get(0), row.get(1))
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
                    "DELETE FROM {}.presence WHERE app_id = $1 AND expires_at <= $2",
                    self.schema
                ),
                &[&self.app_id, &now],
            )
            .map_err(|e| ChannelError::Storage(e.to_string()))?;
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

        Ok(())
    }

    pub(super) fn latest_ids(
        &self,
        channels: &[String],
    ) -> Result<Vec<(String, i64)>, ChannelError> {
        self.client.lock().query(
            &format!("SELECT channel, latest_message_id FROM {}.channel_metadata WHERE app_id = $1 AND channel = ANY($2) AND latest_message_id > 0", self.schema),
            &[&self.app_id, &channels],
        ).map(|rows| rows.into_iter().map(|row| (row.get(0), row.get(1))).collect())
            .map_err(|e| ChannelError::Storage(e.to_string()))
    }

    pub(super) fn prune(&self) -> Result<(), ChannelError> {
        let mut client = self.client.lock();
        let now = now_unix_ms();
        let mut tx = client
            .transaction()
            .map_err(|e| ChannelError::Storage(e.to_string()))?;
        tx.query_one(
            "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))",
            &[&format!("{}:{}", self.schema, self.app_id)],
        )
        .map_err(|e| ChannelError::Storage(e.to_string()))?;
        tx.execute(
            &format!(
                "DELETE FROM {}.presence WHERE app_id = $1 AND expires_at <= $2",
                self.schema
            ),
            &[&self.app_id, &now],
        )
        .map_err(|e| ChannelError::Storage(e.to_string()))?;
        tx.execute(
            &format!(
                "WITH deleted AS (
                    DELETE FROM {schema}.channel_messages AS messages
                    USING {schema}.channel_metadata AS metadata
                    WHERE messages.app_id = $1
                      AND metadata.app_id = messages.app_id AND metadata.channel = messages.channel
                      AND ((metadata.replay_window_ms > 0 AND messages.created_at_unix_ms < ($2 - metadata.replay_window_ms))
                        OR (metadata.inactivity_ttl_ms > 0 AND metadata.last_activity_unix_ms < ($2 - metadata.inactivity_ttl_ms)))
                    RETURNING messages.app_id, messages.channel, messages.id
                 ), floors AS (
                    SELECT app_id, channel, MAX(id) AS id FROM deleted GROUP BY app_id, channel
                 )
                 UPDATE {schema}.channel_metadata AS metadata
                 SET highest_deleted_message_id = GREATEST(metadata.highest_deleted_message_id, floors.id)
                 FROM floors WHERE metadata.app_id = floors.app_id AND metadata.channel = floors.channel",
                schema = self.schema,
            ),
            &[&self.app_id, &now],
        )
        .map_err(|e| ChannelError::Storage(e.to_string()))?;
        tx.commit()
            .map_err(|e| ChannelError::Storage(e.to_string()))?;
        Ok(())
    }
}

fn init_postgres(client: &mut Client, schema: &str) -> Result<(), ChannelError> {
    let mut tx = client
        .transaction()
        .map_err(|e| ChannelError::Storage(e.to_string()))?;
    // IF NOT EXISTS does not serialize concurrent PostgreSQL catalog writes.
    tx.query_one(
        "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))",
        &[&format!("tako-channel-schema:{schema}")],
    )
    .map_err(|e| ChannelError::Storage(e.to_string()))?;
    // Old databases did not record deletions. Seed their floor conservatively once;
    // new streams and later pruning use only IDs actually deleted from that stream.
    tx
        .batch_execute(&format!(
            "CREATE SCHEMA IF NOT EXISTS {schema};
             CREATE TABLE IF NOT EXISTS {schema}.presence (
                 app_id TEXT NOT NULL, channel TEXT NOT NULL, connection_id TEXT NOT NULL,
                 member_json TEXT NOT NULL, expires_at BIGINT NOT NULL,
                 PRIMARY KEY(app_id, channel, connection_id)
             );
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
             CREATE INDEX IF NOT EXISTS idx_channel_messages_app_id
               ON {schema}.channel_messages(app_id, id);
             CREATE TABLE IF NOT EXISTS {schema}.channel_metadata (
                 app_id TEXT NOT NULL,
                 channel TEXT NOT NULL,
                 replay_window_ms BIGINT NOT NULL,
                 inactivity_ttl_ms BIGINT NOT NULL,
                 keepalive_interval_ms BIGINT NOT NULL,
                 max_connection_lifetime_ms BIGINT NOT NULL,
                 last_activity_unix_ms BIGINT NOT NULL,
                 latest_message_id BIGINT NOT NULL DEFAULT 0,
                 highest_deleted_message_id BIGINT NOT NULL DEFAULT 0,
                 PRIMARY KEY(app_id, channel)
             );
             ALTER TABLE {schema}.channel_metadata ADD COLUMN IF NOT EXISTS latest_message_id BIGINT NOT NULL DEFAULT 0;
             ALTER TABLE {schema}.channel_metadata ADD COLUMN IF NOT EXISTS highest_deleted_message_id BIGINT;
             UPDATE {schema}.channel_metadata AS metadata
             SET latest_message_id = GREATEST(latest_message_id, COALESCE(
                 (SELECT MAX(id) FROM {schema}.channel_messages AS messages
                  WHERE messages.app_id = metadata.app_id AND messages.channel = metadata.channel), 0));
             UPDATE {schema}.channel_metadata AS metadata
             SET highest_deleted_message_id = COALESCE(
                 (SELECT MIN(id) - 1 FROM {schema}.channel_messages AS messages
                  WHERE messages.app_id = metadata.app_id AND messages.channel = metadata.channel), latest_message_id)
             WHERE highest_deleted_message_id IS NULL;
             ALTER TABLE {schema}.channel_metadata ALTER COLUMN highest_deleted_message_id SET DEFAULT 0;
             ALTER TABLE {schema}.channel_metadata ALTER COLUMN highest_deleted_message_id SET NOT NULL;"
        ))
        .map_err(|e| ChannelError::Storage(e.to_string()))?;
    tx.commit()
        .map_err(|e| ChannelError::Storage(e.to_string()))
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

#[cfg(test)]
mod integration_tests {
    use super::*;

    struct Database {
        client: Client,
        url: String,
        schema: String,
    }

    impl Database {
        fn new() -> Self {
            let url = std::env::var("TAKO_TEST_POSTGRES_URL")
                .expect("real PostgreSQL regression requires TAKO_TEST_POSTGRES_URL");
            let client = Client::connect(&url, NoTls).unwrap();
            let schema = format!(
                "replay_floor_{}_{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            );
            Self {
                client,
                url,
                schema,
            }
        }

        fn open(&self, app: &str) -> PostgresChannelStore {
            PostgresChannelStore::open(&self.url, &self.schema, app).unwrap()
        }

        fn expire(&mut self, id: i64) {
            self.client
                .execute(
                    &format!(
                        "UPDATE {}.channel_messages SET created_at_unix_ms = 0 WHERE id = $1",
                        self.schema
                    ),
                    &[&id],
                )
                .unwrap();
        }
    }

    impl Drop for Database {
        fn drop(&mut self) {
            self.client
                .batch_execute(&format!("DROP SCHEMA IF EXISTS {} CASCADE", self.schema))
                .unwrap();
        }
    }

    fn append(store: &PostgresChannelStore, channel: &str) -> ChannelMessage {
        store
            .append(
                channel,
                &ChannelPublishPayload {
                    r#type: "event".into(),
                    data: serde_json::json!(channel),
                },
            )
            .unwrap()
    }

    fn id(message: &ChannelMessage) -> i64 {
        message.id.parse().unwrap()
    }

    #[test]
    fn postgres_replay_floor_tracks_deleted_stream_rows_across_apps_and_pages() {
        let mut db = Database::new();
        let store = db.open("one");
        let other = db.open("two");
        let consumed = append(&store, "a");
        let b = append(&store, "b");
        let other_app = append(&other, "a");
        append(&store, "b");
        let next = append(&store, "a");
        assert_eq!(id(&next), id(&consumed) + 4);
        for message in [&consumed, &b, &other_app] {
            db.expire(id(message));
        }
        store.prune().unwrap();
        drop(store);
        let store = db.open("one");
        assert_eq!(
            store.replay_cursor("a", Some(id(&consumed))).unwrap(),
            Some(id(&consumed))
        );
        assert_eq!(
            store.read_after("a", Some(id(&consumed)), 1).unwrap(),
            vec![next.clone()]
        );
        assert_eq!(
            other.read_after("a", None, 10).unwrap(),
            vec![other_app.clone()]
        );
        other.prune().unwrap();
        assert_eq!(
            store.read_after("a", Some(id(&consumed)), 1).unwrap(),
            vec![next.clone()]
        );

        let unseen = append(&store, "a");
        append(&store, "b");
        let head = append(&store, "a");
        db.expire(id(&unseen));
        store.prune().unwrap();
        for cursor in [id(&next), id(&head) + 1] {
            assert!(matches!(
                store.read_after("a", Some(cursor), 1),
                Err(ChannelError::StaleCursor)
            ));
            assert!(matches!(
                store.replay_cursor("a", Some(cursor)),
                Err(ChannelError::StaleCursor)
            ));
        }
        assert_eq!(
            store.read_after("a", Some(id(&unseen)), 1).unwrap(),
            vec![head.clone()]
        );
        store.client.lock().execute(
            &format!("UPDATE {}.channel_metadata SET inactivity_ttl_ms = 1, last_activity_unix_ms = 0 WHERE app_id = 'one' AND channel = 'a'", db.schema),
            &[],
        ).unwrap();
        store.prune().unwrap();
        assert!(
            store
                .read_after("a", Some(id(&head)), 1)
                .unwrap()
                .is_empty()
        );
        assert_eq!(store.replay_cursor("a", None).unwrap(), Some(id(&head)));
        assert!(matches!(
            store.read_after("a", Some(id(&unseen)), 1),
            Err(ChannelError::StaleCursor)
        ));
        store.prune().unwrap();
        assert!(
            store
                .read_after("a", Some(id(&head)), 1)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn postgres_replay_floor_migrates_legacy_metadata_once_and_backfills_heads() {
        for legacy_has_head in [true, false] {
            let mut db = Database::new();
            let store = db.open("one");
            let first = append(&store, "a");
            for _ in 0..3 {
                append(&store, "b");
            }
            let retained = append(&store, "a");
            let expired = append(&store, "expired");
            drop(store);
            db.client
                .batch_execute(&format!(
                    "DELETE FROM {schema}.channel_messages WHERE id IN ({first}, {expired});
                 ALTER TABLE {schema}.channel_metadata DROP COLUMN highest_deleted_message_id;",
                    schema = db.schema,
                    first = id(&first),
                    expired = id(&expired),
                ))
                .unwrap();
            if !legacy_has_head {
                db.client
                    .batch_execute(&format!(
                        "ALTER TABLE {}.channel_metadata DROP COLUMN latest_message_id",
                        db.schema
                    ))
                    .unwrap();
            }
            let store = db.open("one");
            assert_eq!(store.replay_cursor("a", None).unwrap(), Some(id(&retained)));
            let floor = id(&retained) - 1;
            assert_eq!(
                store.read_after("a", Some(floor), 10).unwrap(),
                vec![retained.clone()]
            );
            assert!(matches!(
                store.read_after("a", Some(floor - 1), 10),
                Err(ChannelError::StaleCursor)
            ));
            if legacy_has_head {
                assert_eq!(
                    store.replay_cursor("expired", None).unwrap(),
                    Some(id(&expired))
                );
                assert!(
                    store
                        .read_after("expired", Some(id(&expired)), 10)
                        .unwrap()
                        .is_empty()
                );
                assert!(matches!(
                    store.read_after("expired", Some(id(&expired) - 1), 10),
                    Err(ChannelError::StaleCursor)
                ));
            }
            let fresh = append(&store, "fresh");
            assert_eq!(
                store.read_after("fresh", Some(0), 10).unwrap(),
                vec![fresh.clone()]
            );
            drop(store);
            let reopened = db.open("one");
            assert_eq!(
                reopened.read_after("fresh", Some(0), 10).unwrap(),
                vec![fresh]
            );
            assert_eq!(
                reopened.read_after("a", Some(floor), 10).unwrap(),
                vec![retained]
            );
        }
    }
}
