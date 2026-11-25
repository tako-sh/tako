use super::{NOW, Statement};
use crate::CLOSE_MESSAGE_TYPE;

const TABLES: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS tako_replay_apps (
        app TEXT PRIMARY KEY,
        epoch TEXT NOT NULL CHECK(length(epoch) = 32 AND epoch NOT GLOB '*[^0-9a-f]*'),
        seq INTEGER NOT NULL DEFAULT 0,
        stream_count INTEGER NOT NULL DEFAULT 0,
        message_count INTEGER NOT NULL DEFAULT 0,
        message_bytes INTEGER NOT NULL DEFAULT 0
    ) STRICT",
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
    ) STRICT",
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
    ) STRICT",
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
        .map(|sql| Statement::new(*sql, vec![]))
        .collect();
    // Each channel's replay window is its only retention. Creation times are
    // nondecreasing within a stream, so the age index seeks the last expired
    // row however many rows are expired or retained.
    statements.push(Statement::new(format!(
        "CREATE VIEW IF NOT EXISTS tako_replay_bounds AS
         SELECT s.app, s.channel, s.head, s.marker,
             MAX(s.floor, CASE WHEN s.inactivity_ms > 0 AND s.activity_ms < {NOW} - s.inactivity_ms
                 THEN s.head ELSE COALESCE((
                     SELECT id FROM tako_replay_messages m WHERE m.app = s.app AND m.channel = s.channel
                         AND created_ms < {NOW} - NULLIF(s.window_ms, 0)
                     ORDER BY created_ms DESC, id DESC LIMIT 1
                 ), 0) END) AS floor
         FROM tako_replay_streams s"
    ), vec![]));
    // An append also deletes its own stream's expired history, so storage
    // follows publish rate times window however fast an app publishes.
    // Maintenance catches up on streams that stop receiving appends.
    statements.push(Statement::new(format!(
        "CREATE TRIGGER IF NOT EXISTS tako_replay_message_added AFTER INSERT ON tako_replay_messages BEGIN
            UPDATE tako_replay_apps SET seq = NEW.id, message_count = message_count + 1,
                message_bytes = message_bytes + NEW.bytes WHERE app = NEW.app;
            UPDATE tako_replay_streams SET head = NEW.id, message_count = message_count + 1,
                message_bytes = message_bytes + NEW.bytes,
                marker = CASE WHEN NEW.type = '{CLOSE_MESSAGE_TYPE}' THEN NEW.id END
                WHERE app = NEW.app AND channel = NEW.channel;
            DELETE FROM tako_replay_messages WHERE app = NEW.app AND channel = NEW.channel
                AND id < NEW.id AND id <= (SELECT floor FROM tako_replay_bounds
                    WHERE app = NEW.app AND channel = NEW.channel);
         END"
    ), vec![]));
    statements
}
