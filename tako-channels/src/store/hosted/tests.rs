use super::*;
use crate::stream::close_payload;
use std::path::PathBuf;
use tako_db::{Binding, DatabaseHost, DatabaseId, Home, generate_token, hash_token};

struct Db {
    url: String,
    token: String,
    path: PathBuf,
}

impl Db {
    fn open(&self, app: &str, reader: bool) -> HostedChannelStore {
        HostedChannelStore::open(&self.url, &self.token, app, reader).unwrap()
    }

    fn sql(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(&self.path).unwrap()
    }
}

// Run the production SQL and result decoder through the real authenticated host.
async fn with_database(test: impl FnOnce(Db) + Send + 'static) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("database.sqlite");
    DatabaseHost::create_file(&path).unwrap();
    let db = DatabaseId::generate();
    let host = DatabaseHost::new(Default::default());
    host.register(db.clone(), path.clone()).unwrap();
    let token = generate_token();
    host.set_bindings(
        "test#channels",
        vec![Binding {
            database: db,
            token_hash: hash_token(&token),
            home: Home::Local,
        }],
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(tako_db::http::serve(
        host,
        listener,
        tako_db::http::Access::Apps,
    ));
    tokio::task::spawn_blocking(move || test(Db { url, token, path }))
        .await
        .unwrap();
    server.abort();
}

fn event(data: &str) -> ChannelPublishPayload {
    ChannelPublishPayload {
        r#type: "event".into(),
        data: data.into(),
    }
}

#[tokio::test]
async fn appends_delete_their_own_expired_history_and_leave_other_streams_and_apps() {
    with_database(|db| {
        let app = db.open("one", false);
        let other = db.open("two", false);
        let sql = db.sql();
        let rows = |app: &str, channel: &str| -> i64 {
            sql.query_row(
                "SELECT COUNT(*) FROM tako_replay_messages WHERE app = ?1 AND channel = ?2",
                [app, channel],
                |r| r.get(0),
            )
            .unwrap()
        };
        for _ in 0..3 {
            app.publish("a", &event("old"), None).unwrap();
        }
        app.publish("b", &event("quiet"), None).unwrap();
        let other_event = other.publish("a", &event("kept"), None).unwrap().0;
        // Without size caps, nothing is evicted while it is inside its window.
        assert_eq!((rows("one", "a"), rows("one", "b")), (3, 1));
        // Everything app one stored so far falls outside its 10-minute window.
        sql.execute_batch("UPDATE tako_replay_messages SET created_ms = 0 WHERE app = 'one'")
            .unwrap();
        let fresh = app.publish("a", &event("fresh"), None).unwrap().0;
        // The append deleted its own stream's expired rows in its transaction.
        assert_eq!(rows("one", "a"), 1);
        assert_eq!(app.read_after("a", None, 10).unwrap(), [fresh]);
        // A quiet stream's expired row is already invisible; maintenance deletes it.
        assert_eq!(rows("one", "b"), 1);
        assert!(app.read_after("b", None, 10).unwrap().is_empty());
        app.prune(0).unwrap();
        assert_eq!(rows("one", "b"), 0);
        // Another app's identical stream keeps its own history.
        assert_eq!(other.read_after("a", None, 10).unwrap(), [other_event]);
        let totals: (i64, i64) = sql
            .query_row(
                "SELECT message_count, message_bytes FROM tako_replay_apps WHERE app = 'one'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(totals.0, 1);
        assert_eq!(
            totals.1,
            sql.query_row(
                "SELECT bytes FROM tako_replay_messages WHERE app = 'one'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap()
        );
    })
    .await;
}

#[tokio::test]
async fn expired_rows_do_not_resurface_and_closed_rejections_leave_lifecycle_alone() {
    with_database(|db| {
        let app = db.open("one", false);
        let sql = db.sql();
        let policy = ChannelLifecycle {
            inactivity_ttl_ms: 60_000,
            ..Default::default()
        };
        let start = app.replay_cursor("room", None).unwrap();
        let _old = app.publish("room", &event("old"), Some(&policy)).unwrap().0;
        sql.execute("UPDATE tako_replay_streams SET activity_ms = 0", [])
            .unwrap();
        assert!(app.read_after("room", None, 100).unwrap().is_empty());
        assert!(matches!(
            app.read_after("room", start, 100),
            Err(ChannelError::StaleCursor)
        ));
        let fresh = app.publish("room", &event("new"), None).unwrap().0;
        assert_eq!(app.read_after("room", None, 10).unwrap(), [fresh]);
        let (marker, appended) = app.publish("room", &close_payload(), None).unwrap();
        assert!(appended);
        let rejected = ChannelLifecycle {
            replay_window_ms: 0,
            inactivity_ttl_ms: 0,
            ..Default::default()
        };
        sql.execute("UPDATE tako_replay_streams SET activity_ms = 1", [])
            .unwrap();
        assert!(matches!(
            app.publish("room", &event("late"), Some(&rejected)),
            Err(ChannelError::StreamClosed)
        ));
        assert_eq!(
            app.publish("room", &close_payload(), Some(&rejected))
                .unwrap(),
            (marker, false)
        );
        assert_eq!(app.close_marker("room").unwrap(), None);
        let old: (i64, i64) = sql
            .query_row(
                "SELECT activity_ms, inactivity_ms FROM tako_replay_streams",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(old, (1, 60_000));
        app.prune(i64::MIN).unwrap(); // Only database time counts.
        assert_eq!(
            sql.query_row("SELECT stream_count FROM tako_replay_apps", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert!(app.publish("room", &event("reopened"), None).unwrap().1);
        app.prune(i64::MAX).unwrap();
        assert_eq!(app.read_after("room", None, 10).unwrap().len(), 1);
    })
    .await;
}

#[tokio::test]
async fn range_reads_fence_stream_and_live_restore_epoch_and_fill_logical_pages() {
    with_database(|db| {
        let app = db.open("one", false);
        let reader = db.open("one", true);
        let other = db.open("two", false);
        let mut expected = Vec::new();
        for i in 0..19 {
            expected.push(app.publish("a", &event(&i.to_string()), None).unwrap().0);
        }
        let seen = expected[0].id.parse::<ReplayCursor>().unwrap();
        assert_eq!(
            reader.read_after("a", Some(seen), 16).unwrap(),
            expected[1..17]
        );
        assert_eq!(reader.read_after("a", None, u32::MAX).unwrap(), expected);
        assert!(reader.read_after("a", Some(seen), 0).unwrap().is_empty());
        assert!(matches!(
            reader.read_after("b", Some(seen), 0),
            Err(ChannelError::StaleCursor)
        ));
        assert!(matches!(
            other.replay_cursor("a", Some(seen)),
            Err(ChannelError::StaleCursor)
        ));
        let future = ReplayCursor {
            position: 20,
            ..seen
        };
        assert!(matches!(
            reader.read_after("a", Some(future), 10),
            Err(ChannelError::StaleCursor)
        ));
        db.sql()
            .execute(
                "UPDATE tako_replay_apps SET epoch = lower(hex(randomblob(16)))",
                [],
            )
            .unwrap();
        assert!(matches!(
            reader.read_after("a", Some(seen), 8),
            Err(ChannelError::StaleCursor)
        ));
        let issued = app.publish("a", &event("post-restore"), None).unwrap().0;
        assert_eq!(
            reader.replay_cursor("a", None).unwrap(),
            issued.id.parse().ok()
        );
    })
    .await;
}

#[tokio::test]
async fn concurrent_closers_get_one_marker_and_metadata_admission_is_atomic() {
    with_database(|db| {
        let a = db.open("one", false);
        let b = db.open("one", false);
        let other = db.open("two", false);
        let barrier = std::sync::Barrier::new(2);
        let (first, second) = std::thread::scope(|s| {
            let race = s.spawn(|| {
                barrier.wait();
                a.publish("a", &close_payload(), None).unwrap()
            });
            barrier.wait();
            let result = b.publish("a", &close_payload(), None).unwrap();
            (race.join().unwrap(), result)
        });
        assert_eq!(first.0, second.0);
        assert_ne!(first.1, second.1);
        assert_eq!(b.read_after("a", None, 2).unwrap(), [first.0]);

        db.sql()
            .execute(
                "UPDATE tako_replay_apps SET stream_count = ? WHERE app = 'one'",
                [MAX_STREAMS - 1],
            )
            .unwrap();
        let result = std::thread::scope(|s| {
            let one = s.spawn(|| {
                barrier.wait();
                a.publish("tako.stream:1", &event("new"), None).map(|_| ())
            });
            barrier.wait();
            let two = b.sync_channel("tako.stream:2", &ChannelLifecycle::default().auth());
            [one.join().unwrap(), two]
        });
        assert_eq!(result.iter().filter(|r| r.is_ok()).count(), 1);
        assert_eq!(
            result
                .iter()
                .filter(|r| matches!(r, Err(ChannelError::StreamLimit)))
                .count(),
            1
        );
        assert!(
            a.publish("unparameterized", &event("still admitted"), None)
                .is_ok()
        );
        assert!(
            other
                .publish("tako.stream:3", &event("other tenant"), None)
                .is_ok()
        );
    })
    .await;
}

const RESET: &str = "DROP VIEW tako_replay_bounds; DROP TABLE tako_replay_messages;
    DROP TABLE tako_replay_streams; DROP TABLE tako_replay_apps;";

#[tokio::test]
async fn writers_recreate_replaced_state_and_head_polls_report_the_new_epoch() {
    with_database(|db| {
        let app = db.open("one", false);
        let reader = db.open("one", true);
        let channels = ["a".to_string()];
        let before = app.publish("a", &event("1"), None).unwrap().0;
        assert_eq!(
            reader.latest_ids(&channels).unwrap(),
            (vec![("a".into(), 1)], false)
        );
        // `tako db reset` leaves an empty file under the open handles.
        db.sql().execute_batch(RESET).unwrap();
        assert!(reader.latest_ids(&channels).is_err());
        assert!(reader.read_after("a", None, 10).is_err());
        let after = app.publish("a", &event("2"), None).unwrap().0;
        let old = before.id.parse::<ReplayCursor>().unwrap();
        let new = after.id.parse::<ReplayCursor>().unwrap();
        assert_eq!((old.position, new.position), (1, 1));
        assert_ne!(old.scope, new.scope);
        assert_eq!(
            reader.latest_ids(&channels).unwrap(),
            (vec![("a".into(), 1)], true)
        );
        assert!(matches!(
            reader.read_after("a", Some(old), 10),
            Err(ChannelError::StaleCursor)
        ));
        assert_eq!(reader.read_after("a", None, 10).unwrap(), [after]);
        // A restore from before this app used the database lacks its rows.
        db.sql()
            .execute_batch(
                "DELETE FROM tako_replay_messages WHERE app = 'one';
                 DELETE FROM tako_replay_streams WHERE app = 'one';
                 DELETE FROM tako_replay_apps WHERE app = 'one';",
            )
            .unwrap();
        assert_eq!(reader.latest_ids(&channels).unwrap(), (vec![], true));
        app.sync_channel("a", &ChannelLifecycle::default().auth())
            .unwrap();
        let fresh = reader.replay_cursor("a", None).unwrap().unwrap();
        assert_eq!(fresh.position, 0);
        assert_ne!(fresh.scope, new.scope);
        // Maintenance recovers an idle writer, and readers never create state.
        db.sql().execute_batch(RESET).unwrap();
        assert!(HostedChannelStore::open(&db.url, &db.token, "one", true).is_err());
        app.prune(0).unwrap();
        db.open("one", true);
    })
    .await;
}
