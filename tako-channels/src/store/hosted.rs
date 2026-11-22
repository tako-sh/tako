//! The named database owns each app's replay sequence, clock, limits, and epoch.
//! Before exposing a restored/imported snapshot, its owner rotates all epochs:
//! `UPDATE tako_replay_apps SET epoch = lower(hex(randomblob(16)))`.
//! Never cache the epoch in a handle: a restore can reuse already-issued positions.

mod schema;
#[cfg(test)]
mod tests;

use super::hosted_sql::{Client, Statement};
use super::{HOSTED_HEAD_BATCH, IDLE_STREAM_GRACE_MS, MAX_STREAMS, SqliteReplayLimits};
use crate::{
    CLOSE_MESSAGE_TYPE, ChannelAuthResponse, ChannelError, ChannelLifecycle, ChannelMessage,
    ChannelPublishPayload, ReplayCursor,
};
use parking_lot::Mutex;
use serde::de::DeserializeOwned;
use serde_json::Value;

const NOW: &str = "CAST(unixepoch('subsec') * 1000 AS INTEGER)";
const MAINTENANCE_ROWS: i64 = 256;
// Keep a page of legal 1 MiB events, including JSON-in-JSON escaping, under
// the transport response limit. A short logical page must mean end-of-history.
const WIRE_PAGE: u32 = 8;
const BOUNDS: &str = "SELECT a.epoch, COALESCE(b.floor, 0) AS floor,
        COALESCE(b.head, 0) AS head, b.marker
    FROM tako_replay_apps a LEFT JOIN tako_replay_bounds b ON b.app = ?1 AND b.channel = ?2
    WHERE a.app = ?1";

pub(super) struct HostedChannelStore {
    client: Client,
    app: String,
    reader: bool,
    // Maintenance progress is disposable. Persisting it would dirty an idle
    // database (and trigger backups) just by looking for expired rows.
    prune_range: Mutex<(i64, i64)>,
    // Change detection only, never cursors: a restore, import or reset gives
    // the app a new epoch, so live subscribers must reload.
    polled_epoch: Mutex<Option<String>>,
}

type Rows = Vec<Vec<Value>>;

fn invalid_result() -> ChannelError {
    ChannelError::Storage("invalid or missing hosted replay state".into())
}

fn decode<T: DeserializeOwned>(row: Vec<Value>) -> Result<T, ChannelError> {
    serde_json::from_value(Value::Array(row)).map_err(|_| invalid_result())
}

fn one<T: DeserializeOwned>(mut rows: Rows) -> Result<T, ChannelError> {
    if rows.len() != 1 {
        return Err(invalid_result());
    }
    decode(rows.pop().ok_or_else(invalid_result)?)
}

fn epoch(hex: &str) -> Result<[u8; 16], ChannelError> {
    if hex.len() != 32 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid_result());
    }
    Ok(u128::from_str_radix(hex, 16)
        .map_err(|_| invalid_result())?
        .to_be_bytes())
}

fn cursor(
    epoch: [u8; 16],
    channel: &str,
    floor: i64,
    head: i64,
    requested: Option<ReplayCursor>,
) -> Result<ReplayCursor, ChannelError> {
    let head_cursor = ReplayCursor::sqlite(epoch, channel, head);
    if let Some(c) = requested {
        if c.scope != head_cursor.scope || !(floor..=head).contains(&c.position) {
            return Err(ChannelError::StaleCursor);
        }
        Ok(c)
    } else {
        Ok(head_cursor)
    }
}

fn message(
    epoch: [u8; 16],
    channel: &str,
    id: i64,
    r#type: String,
    data_json: &str,
) -> Result<ChannelMessage, ChannelError> {
    Ok(ChannelMessage {
        id: ReplayCursor::sqlite(epoch, channel, id).to_string(),
        event_id: None,
        channel: channel.into(),
        r#type,
        data: serde_json::from_str(data_json).map_err(|_| invalid_result())?,
    })
}

impl HostedChannelStore {
    pub(super) fn open(
        url: &str,
        token: &str,
        app: &str,
        reader: bool,
    ) -> Result<Self, ChannelError> {
        let store = Self {
            client: Client::new(url, token)?,
            app: app.into(),
            reader,
            prune_range: Mutex::new((0, i64::MAX)),
            polled_epoch: Mutex::new(None),
        };
        if !reader {
            store.init()?;
        }
        let (hex,): (String,) = one(store.query(Statement::new(
            "SELECT epoch FROM tako_replay_apps WHERE app = ?",
            vec![app.into()],
        ))?)?;
        epoch(&hex)?;
        Ok(store)
    }

    /// Idempotent: creates missing tables and this app's row with a fresh epoch.
    fn init(&self) -> Result<(), ChannelError> {
        let limits = SqliteReplayLimits::default();
        let mut init = schema::init();
        init.push(Statement::new(
            "INSERT INTO tako_replay_apps(app, epoch, max_stream_messages, max_stream_bytes,
                max_messages, max_bytes, max_age_ms)
             VALUES(?, lower(hex(randomblob(16))), ?, ?, ?, ?, ?) ON CONFLICT(app) DO NOTHING",
            vec![
                self.app.as_str().into(),
                limits.max_messages_per_stream.into(),
                limits.max_bytes_per_stream.into(),
                limits.max_messages.into(),
                limits.max_bytes.into(),
                limits.max_age_ms.into(),
            ],
        ));
        self.client.batch(&init).map(drop)
    }

    /// A reset, or a restore or import from before this app used the database,
    /// replaces the file under an open handle. Recreate the app's state only
    /// once the database confirms it is missing, so an ambiguous transport
    /// failure never repeats a write that may have committed.
    fn recovering<T>(&self, op: impl Fn() -> Result<T, ChannelError>) -> Result<T, ChannelError> {
        match op() {
            Err(error) if !self.reader && self.state_missing() => {
                if self.init().is_err() {
                    return Err(error);
                }
                op()
            }
            result => result,
        }
    }

    fn state_missing(&self) -> bool {
        let tables = self.query(Statement::new(
            "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name = 'tako_replay_apps'",
            vec![],
        ));
        match tables.and_then(one::<(i64,)>) {
            Ok((0,)) => true,
            Ok(_) => self
                .query(Statement::new(
                    "SELECT 1 FROM tako_replay_apps WHERE app = ?",
                    vec![self.app.as_str().into()],
                ))
                .is_ok_and(|rows| rows.is_empty()),
            Err(_) => false,
        }
    }

    /// True once per epoch change seen by head polling, but not the first poll.
    fn epoch_replaced(&self, hex: Option<String>) -> bool {
        let mut polled = self.polled_epoch.lock();
        let replaced = polled.is_some() && *polled != hex;
        *polled = hex;
        replaced
    }

    fn writable(&self) -> Result<(), ChannelError> {
        if self.reader {
            Err(ChannelError::Storage(
                "channel mutations require the owning broker".into(),
            ))
        } else {
            Ok(())
        }
    }

    fn query(&self, stmt: Statement) -> Result<Rows, ChannelError> {
        self.client
            .read_batch(&[stmt])?
            .pop()
            .ok_or_else(invalid_result)
    }

    fn key(&self, channel: &str) -> Vec<Value> {
        vec![self.app.as_str().into(), channel.into()]
    }

    // For publish, admit and touch only if the message can be appended. For
    // subscribe, touch even a closed stream. Freeze any expired prefix before
    // a longer window or renewed activity could make it visible again.
    fn touch(
        &self,
        channel: &str,
        lifecycle: Option<&ChannelAuthResponse>,
        bytes: Option<i64>,
    ) -> Statement {
        let defaults = ChannelLifecycle::default().auth();
        let settings = lifecycle.unwrap_or(&defaults);
        let mut args = self.key(channel);
        args.extend([
            settings.replay_window_ms.into(),
            settings.inactivity_ttl_ms.into(),
            settings.max_connection_lifetime_ms.into(),
            i64::from(lifecycle.is_some()).into(),
            bytes.into(),
            i64::from(crate::stream::counts_toward_stream_limit(channel)).into(),
        ]);
        Statement::new(&format!(
            "INSERT INTO tako_replay_streams(app, channel, head, floor, activity_ms,
                    window_ms, inactivity_ms, lifetime_ms)
             SELECT a.app, ?2, a.seq, a.seq, {NOW}, ?3, ?4, ?5
             FROM tako_replay_apps a LEFT JOIN tako_replay_streams s ON s.app = a.app AND s.channel = ?2
             WHERE a.app = ?1 AND (?7 IS NULL OR (s.marker IS NULL AND ?7 <= MIN(a.max_bytes, a.max_stream_bytes)))
                 AND (?8 = 0 OR a.stream_count < {MAX_STREAMS} OR s.app IS NOT NULL)
             ON CONFLICT(app, channel) DO UPDATE SET
                 floor = (SELECT floor FROM tako_replay_bounds WHERE app = ?1 AND channel = ?2),
                 activity_ms = excluded.activity_ms,
                 window_ms = CASE WHEN ?6 THEN excluded.window_ms ELSE window_ms END,
                 inactivity_ms = CASE WHEN ?6 THEN excluded.inactivity_ms ELSE inactivity_ms END,
                 lifetime_ms = CASE WHEN ?6 THEN MAX(lifetime_ms, excluded.lifetime_ms) ELSE lifetime_ms END
             RETURNING 1"
        ), args)
    }

    pub(super) fn publish(
        &self,
        channel: &str,
        payload: &ChannelPublishPayload,
        lifecycle: Option<&ChannelLifecycle>,
    ) -> Result<(ChannelMessage, bool), ChannelError> {
        self.writable()?;
        self.recovering(|| self.append(channel, payload, lifecycle))
    }

    fn append(
        &self,
        channel: &str,
        payload: &ChannelPublishPayload,
        lifecycle: Option<&ChannelLifecycle>,
    ) -> Result<(ChannelMessage, bool), ChannelError> {
        let data = serde_json::to_string(&payload.data)
            .map_err(|e| ChannelError::BadRequest(e.to_string()))?;
        let bytes = (channel.len() + payload.r#type.len() + data.len()) as i64;
        let mut args = self.key(channel);
        args.extend([payload.r#type.as_str().into(), data.into(), bytes.into()]);
        let mut status_args = self.key(channel);
        status_args.push(bytes.into());
        let [_, inserted, status]: [Rows; 3] = self.client.batch(&[
            self.touch(channel, lifecycle.map(ChannelLifecycle::auth).as_ref(), Some(bytes)),
            // changes() is the preceding conditional upsert's count. Triggers
            // advance the sequence and evict history inside this transaction.
            Statement::new(&format!(
                "INSERT INTO tako_replay_messages(app, channel, id, type, data_json, bytes, created_ms)
                 SELECT app, ?2, seq + 1, ?3, ?4, ?5,
                     MAX({NOW}, COALESCE((SELECT MAX(created_ms) FROM tako_replay_messages
                         WHERE app = ?1 AND channel = ?2), 0))
                 FROM tako_replay_apps WHERE app = ?1 AND changes() = 1 RETURNING id"
            ), args),
            Statement::new(
                "SELECT a.epoch, ?3 <= MIN(a.max_bytes, a.max_stream_bytes), s.marker, m.data_json
                 FROM tako_replay_apps a LEFT JOIN tako_replay_streams s ON a.app = s.app AND s.channel = ?2
                 LEFT JOIN tako_replay_messages m ON m.app = a.app AND m.id = s.marker
                 WHERE a.app = ?1", status_args)
        ])?.try_into().map_err(|_| invalid_result())?;
        let (hex, fits, marker, marker_data): (String, i64, Option<i64>, Option<String>) =
            one(status)?;
        let epoch = epoch(&hex)?;
        if !inserted.is_empty() {
            let (id,): (i64,) = one(inserted)?;
            return Ok((
                ChannelMessage {
                    id: ReplayCursor::sqlite(epoch, channel, id).to_string(),
                    channel: channel.into(),
                    event_id: None,
                    r#type: payload.r#type.clone(),
                    data: payload.data.clone(),
                },
                true,
            ));
        }
        if let Some(id) = marker {
            if payload.r#type != CLOSE_MESSAGE_TYPE {
                return Err(ChannelError::StreamClosed);
            }
            return Ok((
                message(
                    epoch,
                    channel,
                    id,
                    CLOSE_MESSAGE_TYPE.into(),
                    &marker_data.ok_or_else(invalid_result)?,
                )?,
                false,
            ));
        }
        if fits == 0 {
            return Err(ChannelError::BadRequest(
                "message exceeds hosted SQLite replay byte budget".into(),
            ));
        }
        Err(super::stream_limit_error(channel))
    }

    pub(super) fn sync_channel(
        &self,
        channel: &str,
        auth: &ChannelAuthResponse,
    ) -> Result<(), ChannelError> {
        self.writable()?;
        if auth.ephemeral {
            return Ok(());
        }
        self.recovering(|| {
            let mut results = self
                .client
                .batch(&[self.touch(channel, Some(auth), None)])?;
            if results.pop().ok_or_else(invalid_result)?.is_empty() {
                return Err(super::stream_limit_error(channel));
            }
            Ok(())
        })
    }

    pub(super) fn replay_cursor(
        &self,
        channel: &str,
        requested: Option<ReplayCursor>,
    ) -> Result<Option<ReplayCursor>, ChannelError> {
        let (hex, floor, head, _): (String, i64, i64, Option<i64>) =
            one(self.query(Statement::new(BOUNDS, self.key(channel)))?)?;
        cursor(epoch(&hex)?, channel, floor, head, requested).map(Some)
    }

    pub(super) fn close_marker(
        &self,
        channel: &str,
    ) -> Result<Option<ChannelMessage>, ChannelError> {
        let (hex, id, data): (String, Option<i64>, Option<String>) =
            one(self.query(Statement::new(
                &format!(
                    "WITH b AS ({BOUNDS}) SELECT epoch, m.id, m.data_json FROM b
             LEFT JOIN tako_replay_messages m ON m.app = ?1 AND m.id = b.marker AND m.id > b.floor"
                ),
                self.key(channel),
            ))?)?;
        id.map(|id| {
            message(
                epoch(&hex)?,
                channel,
                id,
                CLOSE_MESSAGE_TYPE.into(),
                &data.ok_or_else(invalid_result)?,
            )
        })
        .transpose()
    }

    pub(super) fn read_after(
        &self,
        channel: &str,
        mut after: Option<ReplayCursor>,
        limit: u32,
    ) -> Result<Vec<ChannelMessage>, ChannelError> {
        let sql = format!(
            "WITH b AS MATERIALIZED ({BOUNDS}), page AS MATERIALIZED (
                 SELECT id, type, data_json FROM tako_replay_messages WHERE app = ?1 AND channel = ?2
                     AND id > MAX(?3, (SELECT floor FROM b)) AND id <= ?4 ORDER BY id LIMIT ?5
             ) SELECT b.epoch, b.floor, b.head, page.id, page.type, page.data_json
               FROM b LEFT JOIN page ON 1 ORDER BY page.id"
        );
        let mut messages = Vec::new();
        let mut end = i64::MAX;
        loop {
            let take = WIRE_PAGE.min(limit - messages.len() as u32);
            let mut args = self.key(channel);
            args.extend([
                after.map_or(0, |c| c.position).into(),
                end.into(),
                take.into(),
            ]);
            let rows = self.query(Statement::new(&sql, args))?;
            if rows.is_empty() {
                return Err(invalid_result());
            }
            let mut last = None;
            let count = rows.len() as u32;
            for row in rows {
                let (hex, floor, head, id, kind, data): (
                    String,
                    i64,
                    i64,
                    Option<i64>,
                    Option<String>,
                    Option<String>,
                ) = decode(row)?;
                let epoch = epoch(&hex)?;
                if last.is_none() {
                    cursor(epoch, channel, floor, head, after)?;
                    end = end.min(head);
                }
                let Some(id) = id else { return Ok(messages) };
                messages.push(message(
                    epoch,
                    channel,
                    id,
                    kind.ok_or_else(invalid_result)?,
                    &data.ok_or_else(invalid_result)?,
                )?);
                last = Some(ReplayCursor::sqlite(epoch, channel, id));
            }
            if count < take
                || messages.len() == limit as usize
                || last.is_some_and(|c| c.position >= end)
            {
                return Ok(messages);
            }
            after = last;
        }
    }

    /// Heads of the given streams, and whether the app's epoch changed since
    /// the previous poll (the database was reset, restored or imported).
    pub(super) fn latest_ids(
        &self,
        channels: &[String],
    ) -> Result<(Vec<(String, i64)>, bool), ChannelError> {
        let mut heads = Vec::new();
        let mut current = None;
        for chunk in channels.chunks(HOSTED_HEAD_BATCH) {
            let channels = serde_json::to_string(chunk).map_err(|_| invalid_result())?;
            // One statement keeps the epoch and the heads in one snapshot.
            for row in self.query(Statement::new(
                "SELECT a.epoch, s.channel, s.head FROM tako_replay_apps a
                 LEFT JOIN tako_replay_streams s ON s.app = a.app
                     AND s.channel IN (SELECT value FROM json_each(?2)) AND s.head > 0
                 WHERE a.app = ?1",
                vec![self.app.as_str().into(), channels.into()],
            ))? {
                let (hex, channel, head): (String, Option<String>, Option<i64>) = decode(row)?;
                current = Some(hex);
                if let (Some(channel), Some(head)) = (channel, head) {
                    heads.push((channel, head));
                }
            }
        }
        // A missing app row also means replaced state; the writer recreates it.
        Ok((heads, self.epoch_replaced(current)))
    }

    pub(super) fn prune(&self, now: i64) -> Result<(), ChannelError> {
        self.writable()?;
        self.recovering(|| self.prune_once(now))
    }

    fn prune_once(&self, _now: i64) -> Result<(), ChannelError> {
        let mut range = self.prune_range.lock();
        let (after, through) = *range;
        let idle = format!(
            "SELECT channel FROM tako_replay_streams WHERE app = ?1 AND message_count = 0
            AND activity_ms + lifetime_ms < {NOW} - {IDLE_STREAM_GRACE_MS}
            ORDER BY activity_ms + lifetime_ms LIMIT {MAINTENANCE_ROWS}"
        );
        let expired =
            "m.id <= (SELECT floor FROM tako_replay_bounds WHERE app = ?1 AND channel = m.channel)";
        let args = vec![self.app.as_str().into(), after.into(), through.into()];
        let (hex, seq, end, has_expired, has_idle): (String, i64, Option<i64>, i64, i64) =
            one(self.query(Statement::new(&format!(
                "WITH chunk AS MATERIALIZED (
                    SELECT id, channel FROM tako_replay_messages WHERE app = ?1 AND id > ?2 AND id <= ?3
                    ORDER BY id LIMIT {MAINTENANCE_ROWS})
                 SELECT epoch, seq, (SELECT MAX(id) FROM chunk),
                     EXISTS(SELECT 1 FROM chunk m WHERE {expired}), EXISTS({idle})
                 FROM tako_replay_apps WHERE app = ?1"
            ), args))?)?;
        if has_expired != 0 || has_idle != 0 {
            // Another broker may have pruned this range, touched an idle
            // stream, or restored the database. Recheck under the write lock.
            let mut writes = Vec::new();
            if has_expired != 0 {
                writes.push(Statement::new(
                    &format!(
                    "DELETE FROM tako_replay_messages AS m WHERE app = ?1 AND id > ?2 AND id <= ?3
                     AND (SELECT epoch FROM tako_replay_apps WHERE app = ?1) = ?4 AND {expired}"
                ),
                    vec![
                        self.app.as_str().into(),
                        after.into(),
                        end.into(),
                        hex.as_str().into(),
                    ],
                ));
            }
            writes.push(Statement::new(&format!(
                "DELETE FROM tako_replay_streams WHERE app = ?1
                 AND (SELECT epoch FROM tako_replay_apps WHERE app = ?1) = ?2 AND channel IN ({idle})"
            ), vec![self.app.as_str().into(), hex.into()]));
            self.client.batch(&writes)?;
        }
        // Freeze a pass's high-water mark so new arrivals cannot indefinitely
        // postpone revisiting older positions that have since expired.
        let through = through.min(seq);
        *range = end
            .filter(|end| *end < through)
            .map_or((0, i64::MAX), |end| (end, through));
        Ok(())
    }
}
