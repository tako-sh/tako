use super::{NOW, Statement};
use crate::CLOSE_MESSAGE_TYPE;

const TABLES: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS tako_replay_apps (
        app TEXT PRIMARY KEY,
        epoch TEXT NOT NULL CHECK(length(epoch) = 32 AND epoch NOT GLOB '*[^0-9a-f]*'),
        seq INTEGER NOT NULL DEFAULT 0,
        stream_count INTEGER NOT NULL DEFAULT 0,
        message_count INTEGER NOT NULL DEFAULT 0,
        message_bytes INTEGER NOT NULL DEFAULT 0,
        max_stream_messages INTEGER NOT NULL CHECK(max_stream_messages > 0),
        max_stream_bytes INTEGER NOT NULL CHECK(max_stream_bytes > 0),
        max_messages INTEGER NOT NULL CHECK(max_messages > 0),
        max_bytes INTEGER NOT NULL CHECK(max_bytes > 0),
        max_age_ms INTEGER NOT NULL CHECK(max_age_ms >= 0)
    ) STRICT, WITHOUT ROWID",
    "CREATE TABLE IF NOT EXISTS tako_replay_streams (
        app TEXT NOT NULL REFERENCES tako_replay_apps(app),
        channel TEXT NOT NULL,
        head INTEGER NOT NULL,
        floor INTEGER NOT NULL,
        marker INTEGER,
        message_count INTEGER NOT NULL DEFAULT 0,
        message_bytes INTEGER NOT NULL DEFAULT 0,
        activity_ms INTEGER NOT NULL,
        window_ms INTEGER NOT NULL,
        inactivity_ms INTEGER NOT NULL,
        lifetime_ms INTEGER NOT NULL,
        PRIMARY KEY(app, channel)
    ) STRICT, WITHOUT ROWID",
    "CREATE TABLE IF NOT EXISTS tako_replay_messages (
        app TEXT NOT NULL,
        id INTEGER NOT NULL,
        channel TEXT NOT NULL,
        type TEXT NOT NULL,
        data_json TEXT NOT NULL,
        bytes INTEGER NOT NULL,
        created_ms INTEGER NOT NULL,
        PRIMARY KEY(app, id),
        FOREIGN KEY(app, channel) REFERENCES tako_replay_streams(app, channel)
    ) STRICT, WITHOUT ROWID",
    "CREATE INDEX IF NOT EXISTS tako_replay_stream_ids ON tako_replay_messages(app, channel, id)",
    "CREATE INDEX IF NOT EXISTS tako_replay_stream_age
        ON tako_replay_messages(app, channel, created_ms, id)",
    "CREATE INDEX IF NOT EXISTS tako_replay_idle ON tako_replay_streams(app, activity_ms + lifetime_ms)
        WHERE message_count = 0",
    "CREATE TRIGGER IF NOT EXISTS tako_replay_stream_added AFTER INSERT ON tako_replay_streams BEGIN
        UPDATE tako_replay_apps SET stream_count = stream_count + 1 WHERE app = NEW.app;
    END",
    "CREATE TRIGGER IF NOT EXISTS tako_replay_stream_deleted AFTER DELETE ON tako_replay_streams BEGIN
        UPDATE tako_replay_apps SET stream_count = stream_count - 1 WHERE app = OLD.app;
    END",
    "CREATE TRIGGER IF NOT EXISTS tako_replay_message_deleted AFTER DELETE ON tako_replay_messages BEGIN
        UPDATE tako_replay_streams SET message_count = message_count - 1,
            message_bytes = message_bytes - OLD.bytes, floor = MAX(floor, OLD.id),
            marker = NULLIF(marker, OLD.id)
            WHERE app = OLD.app AND channel = OLD.channel;
        UPDATE tako_replay_apps SET message_count = message_count - 1,
            message_bytes = message_bytes - OLD.bytes WHERE app = OLD.app;
    END",
];

pub(super) fn init() -> Vec<Statement> {
    let mut statements: Vec<_> = TABLES
        .iter()
        .map(|sql| Statement::new(sql, vec![]))
        .collect();
    // Creation times are nondecreasing within a stream. The age index can seek
    // its last expired row, regardless of the number of expired/retained rows.
    statements.push(Statement::new(&format!(
        "CREATE VIEW IF NOT EXISTS tako_replay_bounds AS
         SELECT s.app, s.channel, s.head, s.marker,
             MAX(s.floor, CASE WHEN s.inactivity_ms > 0 AND s.activity_ms < {NOW} - s.inactivity_ms
                 THEN s.head ELSE COALESCE((
                     SELECT id FROM tako_replay_messages m WHERE m.app = s.app AND m.channel = s.channel
                         AND created_ms < {NOW} - NULLIF(CASE
                             WHEN a.max_age_ms = 0 THEN s.window_ms
                             WHEN s.window_ms = 0 THEN a.max_age_ms
                             ELSE MIN(a.max_age_ms, s.window_ms) END, 0)
                     ORDER BY created_ms DESC, id DESC LIMIT 1
                 ), 0) END) AS floor
         FROM tako_replay_streams s JOIN tako_replay_apps a ON a.app = s.app"
    ), vec![]));
    statements.push(Statement::new(&format!(
        "CREATE TRIGGER IF NOT EXISTS tako_replay_message_added AFTER INSERT ON tako_replay_messages BEGIN
            UPDATE tako_replay_apps SET seq = NEW.id, message_count = message_count + 1,
                message_bytes = message_bytes + NEW.bytes WHERE app = NEW.app;
            UPDATE tako_replay_streams SET head = NEW.id, message_count = message_count + 1,
                message_bytes = message_bytes + NEW.bytes,
                marker = CASE WHEN NEW.type = '{CLOSE_MESSAGE_TYPE}' THEN NEW.id END
                WHERE app = NEW.app AND channel = NEW.channel;
            {};
            {};
         END", evict(true), evict(false)
    ), vec![]));
    statements
}

// Only enter the ordered window scan when the counters exceed a budget.
// Stop at the first prefix that frees BOTH enough rows and enough bytes.
// Stream eviction precedes app eviction so other streams do not pay twice.
fn evict(stream: bool) -> String {
    let (filter, counters, count, bytes) = if stream {
        (
            " AND channel = NEW.channel",
            "JOIN tako_replay_streams s ON s.app = a.app AND s.channel = NEW.channel",
            "s.message_count - a.max_stream_messages",
            "s.message_bytes - a.max_stream_bytes",
        )
    } else {
        (
            "",
            "",
            "a.message_count - a.max_messages",
            "a.message_bytes - a.max_bytes",
        )
    };
    format!(
        "DELETE FROM tako_replay_messages WHERE app = NEW.app {filter} AND id <= (
            SELECT (SELECT id FROM (
                SELECT id, row_number() OVER w AS n, SUM(bytes) OVER w AS b
                FROM tako_replay_messages WHERE app = NEW.app {filter}
                WINDOW w AS (ORDER BY id ROWS UNBOUNDED PRECEDING)
            ) WHERE n >= {count} AND b >= {bytes} LIMIT 1)
            FROM tako_replay_apps a {counters}
            WHERE a.app = NEW.app AND ({count} > 0 OR {bytes} > 0)
        )"
    )
}
