use crate::{ChannelMessage, ChannelPublishPayload, ChannelStore};
use std::sync::{
    Barrier, Mutex,
    atomic::{AtomicUsize, Ordering},
};

#[path = "../../test-support/history.rs"]
mod history;
use history::{Operation, Seed, linearizable};

#[derive(Clone, Debug)]
enum Event {
    Append(i64, u64),
    Read {
        after: i64,
        limit: u32,
        messages: Vec<(i64, u64)>,
    },
}

fn record(message: ChannelMessage) -> (i64, u64) {
    (message.id.parse().unwrap(), message.data.as_u64().unwrap())
}

fn check(model: &mut Vec<(i64, u64)>, event: &Event) -> bool {
    match event {
        Event::Append(id, value) => {
            if model.last().is_some_and(|(previous, _)| previous >= id) {
                return false;
            }
            model.push((*id, *value));
            true
        }
        Event::Read {
            after,
            limit,
            messages,
        } => {
            let expected: Vec<_> = model
                .iter()
                .copied()
                .filter(|(id, _)| id > after)
                .take(*limit as usize)
                .collect();
            &expected == messages
        }
    }
}

fn concurrent_history(seed: u64, stores: Vec<ChannelStore>) {
    let mut random = Seed(seed);
    let plans: Vec<_> = (0..stores.len())
        .map(|_| (random.choose(2) == 0, random.choose(3) as u32 + 1))
        .collect();
    let barrier = Barrier::new(stores.len());
    let clock = AtomicUsize::new(0);
    let events = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for (writer, (store, (read_first, limit))) in stores.into_iter().zip(plans).enumerate() {
            let (barrier, clock, events) = (&barrier, &clock, &events);
            scope.spawn(move || {
                barrier.wait();
                let mut after = 0;
                for step in 0..3 {
                    let start = clock.fetch_add(1, Ordering::SeqCst);
                    let event = if (step == 0 && !read_first) || (step == 1 && read_first) {
                        let value = writer as u64;
                        let (id, _) = record(
                            store
                                .append(
                                    "events",
                                    &ChannelPublishPayload {
                                        r#type: "event".into(),
                                        data: value.into(),
                                    },
                                )
                                .unwrap(),
                        );
                        Event::Append(id, value)
                    } else {
                        let messages: Vec<_> = store
                            .read_after("events", Some(after), limit)
                            .unwrap()
                            .into_iter()
                            .map(record)
                            .collect();
                        let event = Event::Read {
                            after,
                            limit,
                            messages: messages.clone(),
                        };
                        if let Some((id, _)) = messages.last() {
                            after = *id;
                        }
                        event
                    };
                    let end = clock.fetch_add(1, Ordering::SeqCst);
                    events.lock().unwrap().push(Operation {
                        start,
                        end,
                        value: event,
                    });
                    std::thread::yield_now();
                }
            });
        }
    });
    let events = events.into_inner().unwrap();
    assert!(
        linearizable(&events, Vec::new(), check),
        "seed={seed}, history={events:#?}"
    );
}

#[test]
fn sqlite_channel_histories_are_linearizable() {
    for seed in history::seeds() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("channels.sqlite");
        concurrent_history(
            seed,
            (0..3).map(|_| ChannelStore::open(&path).unwrap()).collect(),
        );
    }
}

#[test]
fn postgres_channel_histories_are_linearizable() {
    let Ok(url) = std::env::var("TAKO_TEST_POSTGRES_URL") else {
        return;
    };
    let schema = format!("channel_history_{}", std::process::id());
    for seed in history::seeds() {
        let app = format!("history-{seed}");
        concurrent_history(
            seed,
            (0..3)
                .map(|_| ChannelStore::open_postgres_with_schema(&url, &schema, &app).unwrap())
                .collect(),
        );
    }
    let mut client = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    client
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .unwrap();
}

#[test]
fn postgres_store_open_does_not_deadlock_with_active_publishers() {
    let Ok(url) = std::env::var("TAKO_TEST_POSTGRES_URL") else {
        return;
    };
    let schema = format!("channel_reopen_{}", std::process::id());
    let writer = ChannelStore::open_postgres_with_schema(&url, &schema, "app").unwrap();
    let barrier = Barrier::new(2);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            barrier.wait();
            for value in 0..300 {
                writer
                    .append(
                        "events",
                        &ChannelPublishPayload {
                            r#type: "event".into(),
                            data: value.into(),
                        },
                    )
                    .unwrap();
            }
        });
        barrier.wait();
        for _ in 0..20 {
            let reader = ChannelStore::open_postgres_with_schema(&url, &schema, "app").unwrap();
            reader.read_after("events", None, 100).unwrap();
        }
    });
    assert_eq!(writer.read_after("events", None, 1000).unwrap().len(), 300);
    drop(writer);
    let mut client = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    client
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .unwrap();
}

#[test]
fn postgres_store_open_migrates_unversioned_incomplete_schema() {
    let Ok(url) = std::env::var("TAKO_TEST_POSTGRES_URL") else {
        return;
    };
    let schema = format!("channel_migration_{}", std::process::id());
    let store = ChannelStore::open_postgres_with_schema(&url, &schema, "app").unwrap();
    let first = store
        .append(
            "events",
            &ChannelPublishPayload {
                r#type: "event".into(),
                data: 1.into(),
            },
        )
        .unwrap();
    drop(store);
    let mut client = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    client
        .batch_execute(&format!(
            "DROP TABLE {schema}.channel_schema_migrations;
         ALTER TABLE {schema}.channel_metadata DROP COLUMN latest_message_id;
         DROP INDEX {schema}.idx_channel_messages_app_id;"
        ))
        .unwrap();
    let reopened = ChannelStore::open_postgres_with_schema(&url, &schema, "app").unwrap();
    let second = reopened
        .append(
            "events",
            &ChannelPublishPayload {
                r#type: "event".into(),
                data: 2.into(),
            },
        )
        .unwrap();
    assert_eq!(
        reopened
            .read_after("events", first.id.parse().ok(), 10)
            .unwrap()[0]
            .id,
        second.id
    );
    let exists: bool = client
        .query_one(
            "SELECT to_regclass($1) IS NOT NULL",
            &[&format!("{schema}.idx_channel_messages_app_id")],
        )
        .unwrap()
        .get(0);
    assert!(exists);
    drop(reopened);
    client
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .unwrap();
}

#[test]
fn checker_rejects_a_read_that_skips_a_committed_message() {
    let events = [
        Operation {
            start: 0,
            end: 1,
            value: Event::Append(1, 10),
        },
        Operation {
            start: 2,
            end: 3,
            value: Event::Read {
                after: 0,
                limit: 10,
                messages: vec![],
            },
        },
    ];
    assert!(!linearizable(&events, Vec::new(), check));
}

#[test]
fn replay_and_presence_expiry_histories_are_reproducible() {
    fn run(seed: u64) -> Vec<String> {
        let store = ChannelStore::open_in_memory_without_maintenance();
        // Only explicit prune_at calls advance maintenance in this history.
        let base = 4_000_000_000_000_i64;
        let mut now = base;
        let mut random = Seed(seed);
        let mut retained = Vec::new();
        let mut latest = None;
        let mut lease_until = None;
        let mut trace = Vec::new();
        let auth =
            serde_json::from_value(serde_json::json!({"ok": true, "replayWindowMs": 100})).unwrap();
        store.sync_channel("events", &auth).unwrap();
        let member = crate::PresenceMember {
            connection_id: "connection".into(),
            subject: None,
            data: 1.into(),
        };
        for turn in 0..100 {
            match random.choose(4) {
                0 => {
                    let message = store
                        .append(
                            "events",
                            &ChannelPublishPayload {
                                r#type: "event".into(),
                                data: turn.into(),
                            },
                        )
                        .unwrap();
                    let id: i64 = message.id.parse().unwrap();
                    store.raw_execute(
                        "UPDATE channel_messages SET created_at_unix_ms=?1 WHERE id=?2",
                        (now, id),
                    );
                    retained.push((id, now));
                    latest = Some(id);
                }
                1 => {
                    now += random.choose(60_000) as i64;
                    store.prune_at(now);
                    retained.retain(|(_, created)| *created >= now - 100);
                }
                2 => {
                    store.update_presence("events", &member, now).unwrap();
                    lease_until = Some(now + 45_000);
                }
                _ => {
                    store.remove_presence("events", "connection").unwrap();
                    lease_until = None;
                }
            }
            let records: Vec<i64> = store
                .read_after("events", None, 200)
                .unwrap()
                .into_iter()
                .map(|m| m.id.parse().unwrap())
                .collect();
            assert_eq!(
                records,
                retained.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
                "seed={seed}, turn={turn}"
            );
            assert_eq!(store.replay_cursor("events", None).unwrap(), latest);
            if let Some(last) = latest {
                assert_eq!(
                    store.replay_cursor("events", Some(last)).unwrap(),
                    Some(last)
                );
                let floor = retained.first().map(|(id, _)| id - 1).unwrap_or(last);
                assert!(matches!(
                    store.replay_cursor("events", Some(floor - 1)),
                    Err(crate::ChannelError::StaleCursor)
                ));
            }
            let members = store.presence("events", now).unwrap();
            assert_eq!(
                members.len(),
                usize::from(lease_until.is_some_and(|until| until > now))
            );
            assert!(store.presence("other", now).unwrap().is_empty());
            trace.push(format!(
                "{turn}: now={now}, records={records:?}, members={members:?}"
            ));
        }
        trace
    }
    for seed in history::seeds() {
        assert_eq!(run(seed), run(seed), "seed={seed}: logical trace changed");
    }
}
