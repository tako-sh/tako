use crate::{ChannelEvent, ChannelEventId};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use parking_lot::Mutex;
use std::path::Path;

use crate::{
    CLOSE_MESSAGE_TYPE, ChannelAuthResponse, ChannelError, ChannelLifecycle, ChannelMessage,
    ChannelPublishPayload, ReplayCursor,
};

use super::{IDLE_STREAM_GRACE_MS, channel_message_from_row, now_unix_ms};
use rusqlite::OptionalExtension;

// Autoincrement message IDs start at one, so zero is the initial cursor.
const READ_AFTER_SQL: &str = "SELECT id, channel, type, data_json,
    (SELECT event_json FROM channel_message_events WHERE message_id = channel_messages.id)
    FROM channel_messages WHERE channel = ?1 AND id > ?2 ORDER BY id ASC LIMIT ?3";

/// The first retained terminal marker of a stream after a floor. The partial
/// index on markers keeps this a single seek however long the stream is.
fn close_marker_sql() -> String {
    format!(
        "SELECT id, channel, type, data_json,
            (SELECT event_json FROM channel_message_events WHERE message_id = channel_messages.id)
         FROM channel_messages WHERE channel = ?1 AND type = '{CLOSE_MESSAGE_TYPE}' AND id > ?2
         ORDER BY id ASC LIMIT 1"
    )
}

type StoredRow = (i64, String, String, String, Option<String>);

/// What a remote event did to this store.
pub(super) enum Ingested {
    /// Already applied, or this store's own event.
    Duplicate,
    /// Acknowledged without a message: its stream is closed.
    Dropped,
    /// Acknowledged without a message: it would start a stream beyond the limit.
    StreamLimit,
    Applied(ChannelMessage),
}

fn stored_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
    ))
}

/// A stored row as clients see it: its id is a cursor of this epoch and stream.
fn stored_message(
    epoch: [u8; 16],
    channel: &str,
    row: StoredRow,
) -> Result<ChannelMessage, ChannelError> {
    let cursor = ReplayCursor::sqlite(epoch, channel, row.0);
    let mut message = channel_message_from_row((row.0, row.1, row.2, row.3))?;
    message.event_id = row
        .4
        .map(|json| serde_json::from_str(&json))
        .transpose()
        .map_err(storage_err)?;
    message.id = cursor.to_string();
    Ok(message)
}

fn close_marker(
    conn: &rusqlite::Connection,
    epoch: [u8; 16],
    channel: &str,
    floor: i64,
) -> Result<Option<ChannelMessage>, ChannelError> {
    conn.prepare_cached(&close_marker_sql())?
        .query_row(rusqlite::params![channel, floor], stored_row)
        .optional()?
        .map(|row| stored_message(epoch, channel, row))
        .transpose()
}

const INCREMENTAL_VACUUM_PAGES: i64 = 128;
/// Streams one maintenance tick visits for age pruning, and releases at most.
const MAINTENANCE_BATCH: usize = 1_000;
const WAL_TRUNCATE_DELETED_ROWS_THRESHOLD: usize = 1024;

fn storage_err(e: impl std::fmt::Display) -> ChannelError {
    ChannelError::Storage(e.to_string())
}

/// Hard replay budgets. Byte limits count UTF-8 channel, type and serialized data bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SqliteReplayLimits {
    pub max_messages_per_stream: u64,
    pub max_bytes_per_stream: u64,
    pub max_messages: u64,
    pub max_bytes: u64,
    /// Zero disables this age cap, not the count or byte budgets.
    pub max_age_ms: u64,
}

impl Default for SqliteReplayLimits {
    fn default() -> Self {
        Self {
            max_messages_per_stream: 10_000,
            max_bytes_per_stream: 16 * 1024 * 1024,
            max_messages: 100_000,
            max_bytes: 64 * 1024 * 1024,
            max_age_ms: 24 * 60 * 60 * 1000,
        }
    }
}

impl SqliteReplayLimits {
    fn validate(self) -> Result<Self, ChannelError> {
        let bounds = [
            self.max_messages_per_stream,
            self.max_bytes_per_stream,
            self.max_messages,
            self.max_bytes,
        ];
        if bounds
            .iter()
            .any(|value| *value == 0 || *value > i64::MAX as u64)
            || self.max_age_ms > i64::MAX as u64
        {
            return Err(ChannelError::BadRequest(
                "replay count/byte limits must be positive SQLite integers".into(),
            ));
        }
        Ok(self)
    }
}

pub(super) struct SqliteChannelStore {
    pub(crate) conn: Mutex<rusqlite::Connection>,
    epoch: [u8; 16],
    /// The last stream age maintenance visited; the next tick continues after it.
    prune_cursor: Mutex<String>,
}

impl SqliteChannelStore {
    pub(super) fn open(
        path: &Path,
        limits: Option<SqliteReplayLimits>,
    ) -> Result<Self, ChannelError> {
        let limits = limits.map(SqliteReplayLimits::validate).transpose()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ChannelError::Storage(format!("create channel dir: {e}")))?;
        }
        let mut conn = tako_sqlite::open_local(path).map_err(storage_err)?;
        let epoch = init_connection(&mut conn, limits)?;
        Ok(Self {
            epoch,
            conn: Mutex::new(conn),
            prune_cursor: Mutex::default(),
        })
    }

    pub(super) fn open_in_memory() -> Result<Self, ChannelError> {
        let mut conn = tako_sqlite::open_in_memory().map_err(storage_err)?;
        let epoch = init_connection(&mut conn, None)?;
        Ok(Self {
            epoch,
            conn: Mutex::new(conn),
            prune_cursor: Mutex::default(),
        })
    }

    pub(super) fn open_reader(path: &Path) -> Result<Self, ChannelError> {
        let conn = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(std::time::Duration::from_secs(3))?;
        let epoch: Vec<u8> = conn.query_row(
            "SELECT epoch FROM channel_store_state WHERE singleton = 1",
            [],
            |row| row.get(0),
        )?;
        load_limits(&conn)?;
        replay_epoch(&conn)?;
        Ok(Self {
            epoch: epoch
                .try_into()
                .map_err(|_| storage_err("invalid channel epoch"))?,
            conn: Mutex::new(conn),
            prune_cursor: Mutex::default(),
        })
    }

    /// Publish one message, or close the stream when `payload` is the close
    /// marker. Closing a closed stream returns its marker and no event.
    pub(super) fn publish(
        &self,
        channel: &str,
        payload: &ChannelPublishPayload,
        ephemeral: bool,
        lifecycle: Option<&ChannelLifecycle>,
    ) -> Result<(ChannelMessage, Option<ChannelEvent>), ChannelError> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // A marker closes the stream until retention deletes it. One that has
        // expired but awaits maintenance still closes: the stream reopens only
        // once its history is gone, never under a stale marker.
        if !ephemeral && let Some(marker) = close_marker(&tx, replay_epoch(&tx)?, channel, 0)? {
            if payload.r#type == CLOSE_MESSAGE_TYPE {
                return Ok((marker, None));
            }
            return Err(ChannelError::StreamClosed);
        }
        let sequence: i64 = tx.query_row(
            "UPDATE channel_event_sequence SET sequence = sequence + 1
             WHERE singleton = 1 AND sequence < 9223372036854775807 RETURNING sequence",
            [],
            |row| row.get(0),
        )?;
        let event = ChannelEvent {
            id: ChannelEventId {
                origin: URL_SAFE_NO_PAD.encode(self.epoch),
                sequence: sequence as u64,
            },
            channel: channel.into(),
            payload: payload.clone(),
            ephemeral,
            lifecycle: None,
            created_at_unix_ms: (!ephemeral).then(now_unix_ms),
        };
        let message = append_event(&tx, replay_epoch(&tx)?, &event, lifecycle)?;
        let event = ChannelEvent {
            lifecycle: stored_lifecycle(&tx, channel)?,
            ..event
        };
        tx.commit()?;
        Ok((message, Some(event)))
    }

    /// The stream's first terminal marker that readers can still see.
    pub(super) fn close_marker(
        &self,
        channel: &str,
    ) -> Result<Option<ChannelMessage>, ChannelError> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Deferred)?;
        let expired_floor = expired_floor(&tx, load_limits(&tx)?, channel, now_unix_ms())?;
        let marker = close_marker(&tx, replay_epoch(&tx)?, channel, expired_floor)?;
        tx.commit()?;
        Ok(marker)
    }

    pub(super) fn ingest_remote(
        &self,
        event: &ChannelEvent,
        signal_gap: impl FnOnce(),
    ) -> Result<Ingested, ChannelError> {
        if event.id.sequence == 0
            || event.id.sequence > i64::MAX as u64
            || event.id.origin.is_empty()
            || event.id.origin.len() > 128
            || !event
                .id
                .origin
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        {
            return Err(ChannelError::BadRequest(
                "invalid channel event identity".into(),
            ));
        }
        if event.id.origin == URL_SAFE_NO_PAD.encode(self.epoch) {
            return Ok(Ingested::Duplicate);
        }
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let previous: i64 = tx
            .query_row(
                "SELECT sequence FROM channel_remote_origins WHERE origin = ?1",
                [&event.id.origin],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0);
        let sequence = event.id.sequence as i64;
        if sequence <= previous {
            return Ok(Ingested::Duplicate);
        }
        let gap = sequence > previous + 1;
        if gap {
            rotate_replay_epoch(&tx)?;
        }
        // Close wins on every node. A close is ordered after its origin's own
        // publishes, which arrive first; a publish from another server that
        // raced it is dropped instead of landing after the marker, and a
        // second marker changes nothing. An event made once the marker had
        // expired is no race: its server had reopened the stream, so this
        // copy reopens too, whether or not it has pruned the marker yet. The
        // check reads the origin's own, unclamped clock: that is the clock its
        // server judged its marker by when it reopened.
        let closed = if event.ephemeral {
            false
        } else {
            match retained_marker(&tx, &event.channel)? {
                None => false,
                Some((marker, closed_at)) => {
                    let reopened = match event.created_at_unix_ms {
                        Some(created) => {
                            marker_expired_at(&tx, &event.channel, closed_at, created)?
                        }
                        // A peer that sends no creation time keeps close-wins.
                        None => false,
                    };
                    if reopened {
                        tx.execute(
                            "DELETE FROM channel_messages WHERE channel = ?1 AND id <= ?2",
                            rusqlite::params![event.channel, marker],
                        )?;
                    }
                    !reopened
                }
            }
        };
        // A dropped event still advances its origin's sequence: it was
        // delivered, so the next one is no gap that would reset every reader.
        let ingested = if closed {
            Ingested::Dropped
        } else if !event.ephemeral && stream_limit_reached(&tx, &event.channel)? {
            Ingested::StreamLimit
        } else {
            Ingested::Applied(append_event(
                &tx,
                replay_epoch(&tx)?,
                event,
                event.lifecycle.as_ref(),
            )?)
        };
        tx.execute(
            "INSERT INTO channel_remote_origins(origin, sequence) VALUES (?1, ?2)
            ON CONFLICT(origin) DO UPDATE SET sequence = excluded.sequence",
            rusqlite::params![event.id.origin, sequence],
        )?;
        if gap {
            // Other connections can read the row as soon as this transaction commits.
            signal_gap();
        }
        tx.commit()?;
        Ok(ingested)
    }

    pub(super) fn read_after(
        &self,
        channel: &str,
        after: Option<ReplayCursor>,
        limit: u32,
    ) -> Result<Vec<ChannelMessage>, ChannelError> {
        let mut conn = self.conn.lock();
        // Reads never prune. Background maintenance deletes expired rows; the
        // computed floor hides rows that still wait for it, so replay stays
        // read-only and never contends with publishers for the write lock.
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Deferred)?;
        let expired_floor = expired_floor(&tx, load_limits(&tx)?, channel, now_unix_ms())?;
        let epoch = replay_epoch(&tx)?;
        if let Some(cursor) = after {
            validate_cursor(&tx, epoch, channel, cursor)?;
            if cursor.position < expired_floor {
                return Err(ChannelError::StaleCursor);
            }
        }
        let rows = {
            let mut stmt = tx.prepare_cached(READ_AFTER_SQL)?;
            stmt.query_map(
                rusqlite::params![
                    channel,
                    after.map_or(0, |cursor| cursor.position).max(expired_floor),
                    i64::from(limit)
                ],
                stored_row,
            )?
            .collect::<Result<Vec<_>, _>>()?
        };
        tx.commit()?;
        rows.into_iter()
            .map(|row| stored_message(epoch, channel, row))
            .collect()
    }

    pub(super) fn replay_cursor(
        &self,
        channel: &str,
        requested: Option<ReplayCursor>,
    ) -> Result<Option<ReplayCursor>, ChannelError> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Deferred)?;
        let expired_floor = expired_floor(&tx, load_limits(&tx)?, channel, now_unix_ms())?;
        let epoch = replay_epoch(&tx)?;
        let cursor = if let Some(cursor) = requested {
            validate_cursor(&tx, epoch, channel, cursor)?;
            if cursor.position < expired_floor {
                return Err(ChannelError::StaleCursor);
            }
            cursor
        } else {
            ReplayCursor::sqlite(epoch, channel, stream_head(&tx, channel)?)
        };
        tx.commit()?;
        Ok(Some(cursor))
    }

    pub(super) fn invalidate_replay(&self, signal_gap: impl FnOnce()) -> Result<(), ChannelError> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        rotate_replay_epoch(&tx)?;
        signal_gap();
        tx.commit()?;
        Ok(())
    }

    pub(super) fn sync_channel(
        &self,
        channel: &str,
        auth: &ChannelAuthResponse,
    ) -> Result<(), ChannelError> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let lifecycle = [
            auth.replay_window_ms as i64,
            auth.inactivity_ttl_ms as i64,
            auth.keepalive_interval_ms as i64,
            auth.max_connection_lifetime_ms as i64,
        ];
        touch_stream(&tx, channel, Some(lifecycle), now_unix_ms())?;
        tx.commit()?;
        Ok(())
    }

    pub(super) fn latest_ids(
        &self,
        channels: &[String],
    ) -> Result<Vec<(String, i64)>, ChannelError> {
        let channels = serde_json::to_string(channels).map_err(storage_err)?;
        let conn = self.conn.lock();
        let mut statement = conn.prepare_cached(
            "SELECT channel, latest_message_id FROM channel_metadata
             WHERE channel IN (SELECT value FROM json_each(?1)) AND latest_message_id > 0",
        )?;
        let heads = statement
            .query_map([channels], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(heads)
    }

    /// One maintenance tick. Its work is bounded: age pruning visits at most
    /// [`MAINTENANCE_BATCH`] streams, continuing after the last one next tick,
    /// and at most that many idle streams are released. Readers compute the
    /// expired floor themselves, so a stream pruned a few ticks late never
    /// serves expired rows; publishes enforce the count and byte budgets.
    pub(super) fn prune(&self, now: i64) -> Result<(), ChannelError> {
        let mut conn = self.conn.lock();
        let mut cursor = self.prune_cursor.lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM presence WHERE expires_at <= ?1", [now])?;
        let limits = load_limits(&tx)?;
        let (mut deleted, next) = prune_age_batch(&tx, limits, now, &cursor, MAINTENANCE_BATCH)?;
        deleted += enforce_store_budget(&tx, limits)?;
        release_idle_streams(&tx, now, MAINTENANCE_BATCH)?;
        tx.commit()?;
        *cursor = next;
        if deleted > 0 {
            run_cleanup_maintenance(&conn, deleted);
        }
        Ok(())
    }
}

/// Which rows of a stream the age rules retire at `now`.
enum AgeCutoff {
    /// The stream is inactive: every retained row has expired.
    Everything,
    /// Rows created before this timestamp have expired.
    Before(i64),
}

fn age_cutoff(
    limits: SqliteReplayLimits,
    lifecycle: Option<(i64, i64, i64)>,
    now: i64,
) -> Option<AgeCutoff> {
    let mut cutoff = (limits.max_age_ms > 0).then(|| now - limits.max_age_ms as i64);
    if let Some((replay_window_ms, inactivity_ttl_ms, last_activity_unix_ms)) = lifecycle {
        if inactivity_ttl_ms > 0 && last_activity_unix_ms < now - inactivity_ttl_ms {
            return Some(AgeCutoff::Everything);
        }
        if replay_window_ms > 0 {
            let window = now - replay_window_ms;
            cutoff = Some(cutoff.map_or(window, |cutoff| cutoff.max(window)));
        }
    }
    cutoff.map(AgeCutoff::Before)
}

fn stream_lifecycle(
    conn: &rusqlite::Connection,
    channel: &str,
) -> Result<Option<(i64, i64, i64)>, ChannelError> {
    Ok(conn
        .query_row(
            "SELECT replay_window_ms, inactivity_ttl_ms, last_activity_unix_ms
             FROM channel_metadata WHERE channel = ?1",
            [channel],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?)
}

/// Highest expired position of a stream, including rows maintenance has not
/// deleted yet. Uses the (channel, created_at) index, so it only visits rows
/// that are already past the cutoff.
fn expired_floor(
    conn: &rusqlite::Connection,
    limits: SqliteReplayLimits,
    channel: &str,
    now: i64,
) -> Result<i64, ChannelError> {
    Ok(
        match age_cutoff(limits, stream_lifecycle(conn, channel)?, now) {
            None => 0,
            Some(AgeCutoff::Everything) => conn.query_row(
                "SELECT COALESCE(MAX(id), 0) FROM channel_messages WHERE channel = ?1",
                [channel],
                |row| row.get(0),
            )?,
            Some(AgeCutoff::Before(created)) => conn.query_row(
                "SELECT COALESCE(MAX(id), 0) FROM channel_messages
                 WHERE channel = ?1 AND created_at_unix_ms < ?2",
                rusqlite::params![channel, created],
                |row| row.get(0),
            )?,
        },
    )
}

/// Clock difference between servers that the mesh tolerates when it compares
/// one server's event time with another's close marker.
const MESH_CLOCK_SKEW_MS: i64 = 2_000;

/// Id and origin creation time of a stream's first retained close marker,
/// through the partial marker index.
fn retained_marker(
    conn: &rusqlite::Connection,
    channel: &str,
) -> Result<Option<(i64, i64)>, ChannelError> {
    Ok(conn
        .query_row(
            &format!(
                "SELECT id, created_at_unix_ms FROM channel_messages
                 WHERE channel = ?1 AND type = '{CLOSE_MESSAGE_TYPE}' ORDER BY id LIMIT 1"
            ),
            [channel],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?)
}

/// Whether a close marker made at `closed_at` had expired by `at`, by the
/// rules readers apply: the replay window (and store age cap) by message age,
/// or inactivity counted from the close, the last activity every server saw.
/// The margin keeps one server's slightly slow clock from turning a publish
/// it made after reopening into a race.
fn marker_expired_at(
    conn: &rusqlite::Connection,
    channel: &str,
    closed_at: i64,
    at: i64,
) -> Result<bool, ChannelError> {
    let lifecycle =
        stream_lifecycle(conn, channel)?.map(|(replay_window_ms, inactivity_ttl_ms, _)| {
            (replay_window_ms, inactivity_ttl_ms, closed_at)
        });
    Ok(
        match age_cutoff(
            load_limits(conn)?,
            lifecycle,
            at.saturating_add(MESH_CLOCK_SKEW_MS),
        ) {
            None => false,
            Some(AgeCutoff::Everything) => true,
            Some(AgeCutoff::Before(created)) => closed_at < created,
        },
    )
}

fn replay_epoch(conn: &rusqlite::Connection) -> Result<[u8; 16], ChannelError> {
    let bytes: Vec<u8> = conn.query_row(
        "SELECT epoch FROM channel_replay_state WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    bytes
        .try_into()
        .map_err(|_| storage_err("invalid replay epoch"))
}

fn rotate_replay_epoch(conn: &rusqlite::Connection) -> Result<(), ChannelError> {
    let mut epoch = [0u8; 16];
    getrandom::fill(&mut epoch).map_err(storage_err)?;
    conn.execute(
        "UPDATE channel_replay_state SET epoch = ?1 WHERE singleton = 1",
        [epoch.as_slice()],
    )?;
    Ok(())
}

fn append_event(
    tx: &rusqlite::Connection,
    epoch: [u8; 16],
    event: &ChannelEvent,
    lifecycle: Option<&ChannelLifecycle>,
) -> Result<ChannelMessage, ChannelError> {
    let mut message = ChannelMessage {
        id: String::new(),
        event_id: Some(event.id.clone()),
        channel: event.channel.clone(),
        r#type: event.payload.r#type.clone(),
        data: event.payload.data.clone(),
    };
    if event.ephemeral {
        return Ok(message);
    }
    let data_json = serde_json::to_string(&event.payload.data).map_err(storage_err)?;
    let limits = load_limits(tx)?;
    let bytes = event
        .channel
        .len()
        .saturating_add(event.payload.r#type.len())
        .saturating_add(data_json.len()) as u64;
    if bytes > limits.max_bytes || bytes > limits.max_bytes_per_stream {
        return Err(ChannelError::BadRequest(
            "message exceeds SQLite replay byte budget".into(),
        ));
    }
    let now = now_unix_ms();
    touch_stream(tx, &event.channel, lifecycle.map(lifecycle_columns), now)?;
    // A replicated row keeps its origin's creation time, so it ages out on
    // every node at once, within bounds: never in this node's future, never
    // more than the tolerated skew in its past, and never before the stream's
    // newest row. Creation times then grow with ids in every stream, which
    // the expired floor and age pruning rely on.
    let created = match event.created_at_unix_ms {
        Some(origin) => origin
            .clamp(now.saturating_sub(MESH_CLOCK_SKEW_MS), now)
            .max(newest_created_at(tx, &event.channel)?),
        None => now,
    };
    tx.execute("INSERT INTO channel_messages (channel, type, data_json, created_at_unix_ms) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![event.channel, event.payload.r#type, data_json, created])?;
    let id = tx.last_insert_rowid();
    tx.execute(
        "INSERT INTO channel_message_events(message_id, event_json) VALUES (?1, ?2)",
        rusqlite::params![id, serde_json::to_string(&event.id).map_err(storage_err)?],
    )?;
    tx.execute(
        "UPDATE channel_metadata SET latest_message_id = ?2 WHERE channel = ?1",
        rusqlite::params![event.channel, id],
    )?;
    // Age pruning belongs to background maintenance. Budgets are hard bounds,
    // so this publish retires the oldest rows when the counters say it must.
    enforce_stream_budget(tx, limits, &event.channel)?;
    enforce_store_budget(tx, limits)?;
    message.id = ReplayCursor::sqlite(epoch, &event.channel, id).to_string();
    Ok(message)
}

/// Creation time of a stream's newest row, through the (channel, created_at)
/// index; the earliest time representable when it has none.
fn newest_created_at(conn: &rusqlite::Connection, channel: &str) -> Result<i64, ChannelError> {
    Ok(conn.query_row(
        "SELECT COALESCE(MAX(created_at_unix_ms), ?2) FROM channel_messages WHERE channel = ?1",
        rusqlite::params![channel, i64::MIN],
        |row| row.get(0),
    )?)
}

/// Replay window, inactivity TTL, keepalive interval and connection lifetime of
/// a stream first seen through a publish, in milliseconds.
const DEFAULT_STREAM_LIFECYCLE: [i64; 4] = [600_000, 0, 25_000, 7_200_000];

/// The durable lifecycle a stream's bookkeeping holds.
fn stored_lifecycle(
    conn: &rusqlite::Connection,
    channel: &str,
) -> Result<Option<ChannelLifecycle>, ChannelError> {
    Ok(conn
        .query_row(
            "SELECT replay_window_ms, inactivity_ttl_ms, keepalive_interval_ms,
                max_connection_lifetime_ms
             FROM channel_metadata WHERE channel = ?1",
            [channel],
            |row| {
                Ok(ChannelLifecycle {
                    ephemeral: false,
                    replay_window_ms: row.get::<_, i64>(0)? as u64,
                    inactivity_ttl_ms: row.get::<_, i64>(1)? as u64,
                    keepalive_interval_ms: row.get::<_, i64>(2)? as u64,
                    max_connection_lifetime_ms: row.get::<_, i64>(3)? as u64,
                })
            },
        )
        .optional()?)
}

fn lifecycle_columns(lifecycle: &ChannelLifecycle) -> [i64; 4] {
    [
        lifecycle.replay_window_ms as i64,
        lifecycle.inactivity_ttl_ms as i64,
        lifecycle.keepalive_interval_ms as i64,
        lifecycle.max_connection_lifetime_ms as i64,
    ]
}

/// Record activity on a stream, starting its bookkeeping when it has none,
/// and apply `lifecycle` when one is given (otherwise a new stream gets the
/// default). The stored connection lifetime only grows: idle cleanup must
/// outwait the longest connection any grant allowed on this stream.
fn touch_stream(
    conn: &rusqlite::Connection,
    channel: &str,
    lifecycle: Option<[i64; 4]>,
    now: i64,
) -> Result<(), ChannelError> {
    if ensure_stream(
        conn,
        channel,
        lifecycle.unwrap_or(DEFAULT_STREAM_LIFECYCLE),
        now,
    )? {
        return Ok(());
    }
    match lifecycle {
        Some(lifecycle) => conn.execute(
            "UPDATE channel_metadata SET
                replay_window_ms = ?2,
                inactivity_ttl_ms = ?3,
                keepalive_interval_ms = ?4,
                max_connection_lifetime_ms = MAX(max_connection_lifetime_ms, ?5),
                last_activity_unix_ms = ?6
             WHERE channel = ?1",
            rusqlite::params![
                channel,
                lifecycle[0],
                lifecycle[1],
                lifecycle[2],
                lifecycle[3],
                now
            ],
        )?,
        None => conn.execute(
            "UPDATE channel_metadata SET last_activity_unix_ms = ?2 WHERE channel = ?1",
            rusqlite::params![channel, now],
        )?,
    };
    Ok(())
}

/// Start a stream's bookkeeping when it has none. A stream is born caught up
/// with the store: its head and replay floor begin at the newest allocated
/// message id. A cursor issued before earlier bookkeeping was released is then
/// stale instead of resuming past messages it never saw.
fn ensure_stream(
    conn: &rusqlite::Connection,
    channel: &str,
    lifecycle: [i64; 4],
    now: i64,
) -> Result<bool, ChannelError> {
    if stream_limit_reached(conn, channel)? {
        return Err(super::stream_limit_error(channel));
    }
    let born = conn.execute(
        "INSERT INTO channel_metadata (
            channel,
            replay_window_ms,
            inactivity_ttl_ms,
            keepalive_interval_ms,
            max_connection_lifetime_ms,
            last_activity_unix_ms,
            latest_message_id
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, COALESCE(
            (SELECT seq FROM sqlite_sequence WHERE name = 'channel_messages'), 0))
        ON CONFLICT(channel) DO NOTHING",
        rusqlite::params![
            channel,
            lifecycle[0],
            lifecycle[1],
            lifecycle[2],
            lifecycle[3],
            now
        ],
    )? == 1;
    if born {
        conn.execute(
            "INSERT INTO channel_replay_floor(channel, floor)
             SELECT channel, latest_message_id FROM channel_metadata
             WHERE channel = ?1 AND latest_message_id > 0
             ON CONFLICT(channel) DO UPDATE SET floor = MAX(floor, excluded.floor)",
            [channel],
        )?;
    }
    Ok(born)
}

/// Release the bookkeeping of streams that retain no messages and can have no
/// subscriber. Every connection syncs its stream when it starts and ends within
/// the stream's connection lifetime, so activity older than that lifetime
/// proves nobody is connected, in this process or any other. Releases at most
/// `limit` streams, the longest idle first, through the partial idle index.
fn release_idle_streams(
    conn: &rusqlite::Connection,
    now: i64,
    limit: usize,
) -> Result<(), ChannelError> {
    let idle = {
        let mut statement = conn.prepare_cached(RELEASE_IDLE_SQL)?;
        statement
            .query_map(
                rusqlite::params![now - IDLE_STREAM_GRACE_MS, limit as i64],
                |row| row.get::<_, String>(0),
            )?
            .collect::<Result<Vec<_>, _>>()?
    };
    for channel in idle {
        conn.execute(
            "DELETE FROM channel_replay_floor WHERE channel = ?1",
            [&channel],
        )?;
        conn.execute(
            "DELETE FROM channel_metadata WHERE channel = ?1",
            [&channel],
        )?;
    }
    Ok(())
}

/// Streams that retain nothing, longest idle first. The expression matches
/// `idx_channel_metadata_idle`, so only released candidates are visited.
const RELEASE_IDLE_SQL: &str = "SELECT channel FROM channel_metadata
    WHERE message_count = 0 AND last_activity_unix_ms + max_connection_lifetime_ms < ?1
    ORDER BY last_activity_unix_ms + max_connection_lifetime_ms LIMIT ?2";

/// Streams with retained messages after a maintenance cursor, in key order,
/// through the partial `idx_channel_metadata_retained` index.
const RETAINED_STREAMS_SQL: &str =
    "SELECT channel, replay_window_ms, inactivity_ttl_ms, last_activity_unix_ms
     FROM channel_metadata WHERE message_count > 0 AND channel > ?1 ORDER BY channel LIMIT ?2";

/// Whether starting `channel` would exceed the app's stream limit. Existing
/// streams and unparameterized channel names are never refused, so the limit
/// cannot break a stream that already works.
fn stream_limit_reached(conn: &rusqlite::Connection, channel: &str) -> Result<bool, ChannelError> {
    if !crate::stream::counts_toward_stream_limit(channel) {
        return Ok(false);
    }
    let (exists, streams): (bool, i64) = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM channel_metadata WHERE channel = ?1),
            (SELECT stream_count FROM channel_store_state WHERE singleton = 1)",
        [channel],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok(!exists && streams >= super::MAX_STREAMS)
}

fn load_limits(conn: &rusqlite::Connection) -> Result<SqliteReplayLimits, ChannelError> {
    let json: String = conn.query_row(
        "SELECT limits_json FROM channel_store_state WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    serde_json::from_str::<SqliteReplayLimits>(&json)
        .map_err(storage_err)?
        .validate()
}

fn stream_head(conn: &rusqlite::Connection, channel: &str) -> Result<i64, ChannelError> {
    Ok(conn
        .query_row(
            "SELECT latest_message_id FROM channel_metadata WHERE channel = ?1",
            [channel],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(0))
}

fn validate_cursor(
    conn: &rusqlite::Connection,
    epoch: [u8; 16],
    channel: &str,
    cursor: ReplayCursor,
) -> Result<(), ChannelError> {
    if cursor.scope != ReplayCursor::sqlite(epoch, channel, 0).scope {
        return Err(ChannelError::StaleCursor);
    }
    let floor: i64 = conn
        .query_row(
            "SELECT floor FROM channel_replay_floor WHERE channel = ?1",
            [channel],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(0);
    if cursor.position < floor || cursor.position > stream_head(conn, channel)? {
        return Err(ChannelError::StaleCursor);
    }
    Ok(())
}

/// Delete expired rows of up to `limit` streams after `cursor`, stream by
/// stream through the (channel, created_at) index. Returns the deleted row
/// count and the cursor of the next batch: empty once the last stream was
/// visited, so the next batch starts over. Every message has a metadata row:
/// publishing upserts it first.
fn prune_age_batch(
    conn: &rusqlite::Connection,
    limits: SqliteReplayLimits,
    now: i64,
    cursor: &str,
    limit: usize,
) -> Result<(usize, String), ChannelError> {
    let lifecycles = {
        let mut statement = conn.prepare_cached(RETAINED_STREAMS_SQL)?;
        statement
            .query_map(rusqlite::params![cursor, limit as i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
    };
    let next = if lifecycles.len() < limit {
        String::new()
    } else {
        lifecycles
            .last()
            .map(|(channel, ..)| channel.clone())
            .unwrap_or_default()
    };
    let mut deleted = 0;
    for (channel, replay_window_ms, inactivity_ttl_ms, last_activity_unix_ms) in lifecycles {
        let lifecycle = Some((replay_window_ms, inactivity_ttl_ms, last_activity_unix_ms));
        deleted += match age_cutoff(limits, lifecycle, now) {
            None => 0,
            Some(AgeCutoff::Everything) => conn.execute(
                "DELETE FROM channel_messages WHERE channel = ?1",
                [&channel],
            )?,
            Some(AgeCutoff::Before(created)) => conn.execute(
                "DELETE FROM channel_messages WHERE channel = ?1 AND created_at_unix_ms < ?2",
                rusqlite::params![channel, created],
            )?,
        };
    }
    Ok((deleted, next))
}

/// Retire the oldest rows of one stream until its counters fit the budget. The
/// counters are maintained by triggers, so a stream within budget costs one
/// indexed row read, and a stream at capacity retires about one row per publish.
fn enforce_stream_budget(
    conn: &rusqlite::Connection,
    limits: SqliteReplayLimits,
    channel: &str,
) -> Result<usize, ChannelError> {
    let mut deleted = 0;
    loop {
        let Some((count, bytes)) = conn
            .query_row(
                "SELECT message_count, message_bytes FROM channel_metadata WHERE channel = ?1",
                [channel],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?
        else {
            return Ok(deleted);
        };
        let excess = count - limits.max_messages_per_stream as i64;
        if excess <= 0 && bytes <= limits.max_bytes_per_stream as i64 {
            return Ok(deleted);
        }
        let removed = conn.execute(
            "DELETE FROM channel_messages WHERE id IN (
                SELECT id FROM channel_messages WHERE channel = ?1 ORDER BY id LIMIT ?2
            )",
            rusqlite::params![channel, excess.max(1)],
        )?;
        if removed == 0 {
            return Ok(deleted);
        }
        deleted += removed;
    }
}

/// Retire the globally oldest rows until the store counters fit the budget.
fn enforce_store_budget(
    conn: &rusqlite::Connection,
    limits: SqliteReplayLimits,
) -> Result<usize, ChannelError> {
    let mut deleted = 0;
    loop {
        let (count, bytes) = conn.query_row(
            "SELECT message_count, message_bytes FROM channel_store_state WHERE singleton = 1",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )?;
        let excess = count - limits.max_messages as i64;
        if excess <= 0 && bytes <= limits.max_bytes as i64 {
            return Ok(deleted);
        }
        let removed = conn.execute(
            "DELETE FROM channel_messages WHERE id IN (
                SELECT id FROM channel_messages ORDER BY id LIMIT ?1
            )",
            [excess.max(1)],
        )?;
        if removed == 0 {
            return Ok(deleted);
        }
        deleted += removed;
    }
}

/// Full maintenance when a store opens, after which limits may have changed:
/// every stream is pruned and every idle stream released.
fn prune_replay(
    conn: &rusqlite::Connection,
    limits: SqliteReplayLimits,
    now: i64,
) -> Result<usize, ChannelError> {
    let mut deleted = 0;
    let mut cursor = String::new();
    loop {
        let (removed, next) = prune_age_batch(conn, limits, now, &cursor, MAINTENANCE_BATCH)?;
        deleted += removed;
        if next.is_empty() {
            break;
        }
        cursor = next;
    }
    let over_budget = {
        let mut statement = conn.prepare_cached(
            "SELECT channel FROM channel_metadata WHERE message_count > ?1 OR message_bytes > ?2",
        )?;
        statement
            .query_map(
                rusqlite::params![
                    limits.max_messages_per_stream as i64,
                    limits.max_bytes_per_stream as i64
                ],
                |row| row.get::<_, String>(0),
            )?
            .collect::<Result<Vec<_>, _>>()?
    };
    for channel in over_budget {
        deleted += enforce_stream_budget(conn, limits, &channel)?;
    }
    deleted += enforce_store_budget(conn, limits)?;
    release_idle_streams(conn, now, usize::MAX >> 1)?;
    Ok(deleted)
}

fn init_connection(
    conn: &mut rusqlite::Connection,
    limits: Option<SqliteReplayLimits>,
) -> Result<[u8; 16], ChannelError> {
    conn.execute_batch("PRAGMA auto_vacuum = INCREMENTAL;")?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let conn = &tx;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS presence (
             channel TEXT NOT NULL, connection_id TEXT NOT NULL, member_json TEXT NOT NULL,
             expires_at INTEGER NOT NULL, PRIMARY KEY(channel, connection_id)
         );
         CREATE TABLE IF NOT EXISTS channel_messages (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             channel TEXT NOT NULL,
             type TEXT NOT NULL,
             data_json TEXT NOT NULL,
             created_at_unix_ms INTEGER NOT NULL DEFAULT (unixepoch() * 1000)
         );",
    )
    .map_err(storage_err)?;

    ensure_channel_metadata_schema(conn)?;

    conn.execute_batch(&format!(
        "CREATE INDEX IF NOT EXISTS idx_channel_messages_channel_id
         ON channel_messages(channel, id);
         CREATE INDEX IF NOT EXISTS idx_channel_messages_close
         ON channel_messages(channel, id) WHERE type = '{CLOSE_MESSAGE_TYPE}';"
    ))
    .map_err(storage_err)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS channel_store_state (
            singleton INTEGER PRIMARY KEY CHECK(singleton = 1), epoch BLOB NOT NULL, limits_json TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS channel_event_sequence (singleton INTEGER PRIMARY KEY CHECK(singleton = 1), sequence INTEGER NOT NULL);
         INSERT INTO channel_event_sequence VALUES (1, 0) ON CONFLICT DO NOTHING;
         CREATE TABLE IF NOT EXISTS channel_message_events (message_id INTEGER PRIMARY KEY, event_json TEXT NOT NULL);
         CREATE TRIGGER IF NOT EXISTS channel_event_deleted AFTER DELETE ON channel_messages BEGIN
            DELETE FROM channel_message_events WHERE message_id = OLD.id;
         END;
         CREATE TABLE IF NOT EXISTS channel_remote_origins (origin TEXT PRIMARY KEY, sequence INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS channel_replay_floor (channel TEXT PRIMARY KEY, floor INTEGER NOT NULL);
         CREATE TRIGGER IF NOT EXISTS channel_replay_deleted AFTER DELETE ON channel_messages BEGIN
            INSERT INTO channel_replay_floor(channel, floor) VALUES (OLD.channel, OLD.id)
            ON CONFLICT(channel) DO UPDATE SET floor = MAX(floor, excluded.floor);
         END;
         UPDATE channel_metadata SET latest_message_id = MAX(latest_message_id, COALESCE(
            (SELECT MAX(id) FROM channel_messages WHERE channel_messages.channel = channel_metadata.channel), 0));"
    )?;
    let mut epoch = [0u8; 16];
    getrandom::fill(&mut epoch).map_err(storage_err)?;
    conn.execute("INSERT INTO channel_store_state(singleton, epoch, limits_json) VALUES(1, ?1, ?2) ON CONFLICT DO NOTHING",
        rusqlite::params![epoch.as_slice(), serde_json::to_string(&SqliteReplayLimits::default()).map_err(storage_err)?])?;
    ensure_budget_counters(conn)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS channel_replay_state (singleton INTEGER PRIMARY KEY CHECK(singleton = 1), epoch BLOB NOT NULL);
         INSERT INTO channel_replay_state SELECT singleton, epoch FROM channel_store_state WHERE singleton = 1 ON CONFLICT DO NOTHING;"
    )?;
    if let Some(limits) = limits {
        conn.execute(
            "UPDATE channel_store_state SET limits_json = ?1 WHERE singleton = 1",
            [serde_json::to_string(&limits).map_err(storage_err)?],
        )?;
    }
    prune_replay(conn, load_limits(conn)?, now_unix_ms())?;
    let saved: Vec<u8> = conn.query_row(
        "SELECT epoch FROM channel_store_state WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    let epoch = saved
        .try_into()
        .map_err(|_| storage_err("invalid channel epoch"))?;
    tx.commit()?;
    Ok(epoch)
}

fn ensure_channel_metadata_schema(conn: &rusqlite::Connection) -> Result<(), ChannelError> {
    let exists: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'channel_metadata'",
            [],
            |row| row.get(0),
        )
        .map_err(storage_err)?;

    if exists == 0 {
        conn.execute_batch(
            "CREATE TABLE channel_metadata (
                channel TEXT PRIMARY KEY,
                replay_window_ms INTEGER NOT NULL,
                inactivity_ttl_ms INTEGER NOT NULL,
                keepalive_interval_ms INTEGER NOT NULL,
                max_connection_lifetime_ms INTEGER NOT NULL,
                last_activity_unix_ms INTEGER NOT NULL,
                latest_message_id INTEGER NOT NULL DEFAULT 0
            );",
        )
        .map_err(storage_err)?;
        return Ok(());
    }

    let mut columns = conn
        .prepare("PRAGMA table_info(channel_metadata)")
        .map_err(storage_err)?;
    let columns = columns
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(storage_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(storage_err)?;

    if !columns.iter().any(|column| column == "latest_message_id") {
        conn.execute_batch(
            "ALTER TABLE channel_metadata ADD COLUMN latest_message_id INTEGER NOT NULL DEFAULT 0;",
        )
        .map_err(storage_err)?;
    }

    if columns.iter().any(|column| column == "retention_ms")
        && !columns.iter().any(|column| column == "replay_window_ms")
    {
        conn.execute_batch(
            "ALTER TABLE channel_metadata RENAME COLUMN retention_ms TO replay_window_ms;",
        )
        .map_err(storage_err)?;
    }

    Ok(())
}

/// Per-stream and store-wide message counts and byte sizes let publishes check
/// their budgets without scanning retained rows. Triggers keep them exact for
/// every writer of this file; opening recomputes them from the table.
fn ensure_budget_counters(conn: &rusqlite::Connection) -> Result<(), ChannelError> {
    for table in ["channel_metadata", "channel_store_state"] {
        for column in ["message_count", "message_bytes"] {
            ensure_column(conn, table, column, "INTEGER NOT NULL DEFAULT 0")?;
        }
    }
    ensure_column(
        conn,
        "channel_store_state",
        "stream_count",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_channel_metadata_retained
            ON channel_metadata(channel) WHERE message_count > 0;
         CREATE INDEX IF NOT EXISTS idx_channel_metadata_idle
            ON channel_metadata(last_activity_unix_ms + max_connection_lifetime_ms)
            WHERE message_count = 0;
         CREATE INDEX IF NOT EXISTS idx_presence_expires ON presence(expires_at);
         UPDATE channel_store_state SET stream_count = (SELECT COUNT(*) FROM channel_metadata)
            WHERE singleton = 1;
         CREATE TRIGGER IF NOT EXISTS channel_stream_counted AFTER INSERT ON channel_metadata BEGIN
            UPDATE channel_store_state SET stream_count = stream_count + 1 WHERE singleton = 1;
         END;
         CREATE TRIGGER IF NOT EXISTS channel_stream_uncounted AFTER DELETE ON channel_metadata BEGIN
            UPDATE channel_store_state SET stream_count = stream_count - 1 WHERE singleton = 1;
         END;
         CREATE INDEX IF NOT EXISTS idx_channel_messages_channel_created
            ON channel_messages(channel, created_at_unix_ms);
         UPDATE channel_metadata SET
            message_count = (SELECT COUNT(*) FROM channel_messages m WHERE m.channel = channel_metadata.channel),
            message_bytes = (SELECT COALESCE(SUM(length(CAST(channel AS BLOB)) + length(CAST(type AS BLOB)) + length(CAST(data_json AS BLOB))), 0)
                FROM channel_messages m WHERE m.channel = channel_metadata.channel);
         UPDATE channel_store_state SET
            message_count = (SELECT COUNT(*) FROM channel_messages),
            message_bytes = (SELECT COALESCE(SUM(length(CAST(channel AS BLOB)) + length(CAST(type AS BLOB)) + length(CAST(data_json AS BLOB))), 0) FROM channel_messages)
            WHERE singleton = 1;
         CREATE TRIGGER IF NOT EXISTS channel_message_counted AFTER INSERT ON channel_messages BEGIN
            UPDATE channel_store_state SET message_count = message_count + 1,
                message_bytes = message_bytes + length(CAST(NEW.channel AS BLOB)) + length(CAST(NEW.type AS BLOB)) + length(CAST(NEW.data_json AS BLOB))
                WHERE singleton = 1;
            UPDATE channel_metadata SET message_count = message_count + 1,
                message_bytes = message_bytes + length(CAST(NEW.channel AS BLOB)) + length(CAST(NEW.type AS BLOB)) + length(CAST(NEW.data_json AS BLOB))
                WHERE channel = NEW.channel;
         END;
         CREATE TRIGGER IF NOT EXISTS channel_message_uncounted AFTER DELETE ON channel_messages BEGIN
            UPDATE channel_store_state SET message_count = message_count - 1,
                message_bytes = message_bytes - length(CAST(OLD.channel AS BLOB)) - length(CAST(OLD.type AS BLOB)) - length(CAST(OLD.data_json AS BLOB))
                WHERE singleton = 1;
            UPDATE channel_metadata SET message_count = message_count - 1,
                message_bytes = message_bytes - length(CAST(OLD.channel AS BLOB)) - length(CAST(OLD.type AS BLOB)) - length(CAST(OLD.data_json AS BLOB))
                WHERE channel = OLD.channel;
         END;",
    )?;
    Ok(())
}

fn ensure_column(
    conn: &rusqlite::Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<(), ChannelError> {
    let mut columns = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let exists = columns
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?
        .iter()
        .any(|existing| existing == column);
    if !exists {
        conn.execute_batch(&format!(
            "ALTER TABLE {table} ADD COLUMN {column} {definition};"
        ))?;
    }
    Ok(())
}

fn run_cleanup_maintenance(conn: &rusqlite::Connection, deleted_rows: usize) {
    let vacuum_sql = format!("PRAGMA incremental_vacuum({INCREMENTAL_VACUUM_PAGES});");
    let _ = conn.execute_batch(&vacuum_sql);

    if deleted_rows >= WAL_TRUNCATE_DELETED_ROWS_THRESHOLD {
        let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_query_seeks_to_cursor_instead_of_scanning_channel_history() {
        let store = SqliteChannelStore::open_in_memory().unwrap();
        let conn = store.conn.lock();
        let detail: String = conn
            .query_row(
                &format!("EXPLAIN QUERY PLAN {READ_AFTER_SQL}"),
                rusqlite::params!["feed", 100_000, 100],
                |row| row.get(3),
            )
            .unwrap();
        assert!(detail.contains("channel=? AND id>?"), "{detail}");
    }

    fn plan(conn: &rusqlite::Connection, sql: &str, params: impl rusqlite::Params) -> String {
        let mut statement = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        statement
            .query_map(params, |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .join("; ")
    }

    #[test]
    fn maintenance_scans_seek_their_partial_indexes() {
        let store = SqliteChannelStore::open_in_memory().unwrap();
        let conn = store.conn.lock();
        let idle = plan(&conn, RELEASE_IDLE_SQL, rusqlite::params![0, 1_000]);
        assert!(idle.contains("idx_channel_metadata_idle"), "{idle}");
        assert!(!idle.contains("TEMP B-TREE"), "{idle}");
        let retained = plan(&conn, RETAINED_STREAMS_SQL, rusqlite::params!["", 1_000]);
        assert!(
            retained.contains("idx_channel_metadata_retained"),
            "{retained}"
        );
        assert!(!retained.contains("TEMP B-TREE"), "{retained}");
        let presence = plan(&conn, "DELETE FROM presence WHERE expires_at <= ?1", [0]);
        assert!(presence.contains("idx_presence_expires"), "{presence}");
    }

    #[test]
    fn close_marker_lookup_seeks_the_partial_marker_index() {
        let store = SqliteChannelStore::open_in_memory().unwrap();
        let conn = store.conn.lock();
        let detail: String = conn
            .query_row(
                &format!("EXPLAIN QUERY PLAN {}", close_marker_sql()),
                rusqlite::params!["feed", 0],
                |row| row.get(3),
            )
            .unwrap();
        assert!(detail.contains("idx_channel_messages_close"), "{detail}");
    }
}
