use crate::test_db::TestDb;
use crate::{ChannelMessage, ChannelPublishPayload, ChannelStore, ReplayCursor};
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
    (
        message.id.parse::<ReplayCursor>().unwrap().position,
        message.data.as_u64().unwrap(),
    )
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
                let mut after = store.replay_cursor("events", None).unwrap();
                barrier.wait();
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
                        let messages = store.read_after("events", after, limit).unwrap();
                        let next = messages.last().map(|message| message.id.parse().unwrap());
                        let event = Event::Read {
                            after: after.map_or(0, |cursor| cursor.position),
                            limit,
                            messages: messages.into_iter().map(record).collect(),
                        };
                        if next.is_some() {
                            after = next;
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
fn channel_histories_are_linearizable() {
    let db = TestDb::start();
    for seed in history::seeds() {
        let app = format!("history-{seed}");
        concurrent_history(seed, (0..3).map(|_| db.writer_for(&app)).collect());
    }
}

#[test]
fn store_open_does_not_deadlock_with_active_publishers() {
    let db = TestDb::start();
    let writer = db.writer();
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
            db.reader().read_after("events", None, 100).unwrap();
            db.writer().read_after("events", None, 100).unwrap();
        }
    });
    assert_eq!(writer.read_after("events", None, 1000).unwrap().len(), 300);
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
    // The database's clock decides expiry. Logical time moves stored messages
    // back in whole minutes against a 90 second window, so the real time a
    // run takes never changes what has expired. Stream activity stays real,
    // so the stream is never idle long enough to be released.
    const WINDOW_MS: i64 = 90_000;
    const STEP_MS: i64 = 60_000;
    let db = TestDb::start();
    let run = |seed: u64, attempt: u8| -> Vec<String> {
        let app = format!("expiry-{seed}-{attempt}");
        let store = db.without_maintenance_for(&app);
        let base = 4_000_000_000_000_i64;
        let mut now = base;
        let mut random = Seed(seed);
        let mut retained = Vec::new();
        let mut highest_deleted = 0;
        let mut lease_until = None;
        let mut trace = Vec::new();
        let auth =
            serde_json::from_value(serde_json::json!({"ok": true, "replayWindowMs": WINDOW_MS}))
                .unwrap();
        store.sync_channel("events", &auth).unwrap();
        let mut latest = store.replay_cursor("events", None).unwrap().unwrap();
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
                    let cursor: ReplayCursor = message.id.parse().unwrap();
                    retained.push((cursor.position, now));
                    latest = cursor;
                }
                1 => {
                    let elapsed = random.choose(3) as i64 * STEP_MS;
                    now += elapsed;
                    db.execute(&format!(
                        "UPDATE tako_replay_messages SET created_ms = created_ms - {elapsed}
                         WHERE app = '{app}'"
                    ));
                    store.prune();
                    for &(id, created) in &retained {
                        if created < now - WINDOW_MS {
                            highest_deleted = highest_deleted.max(id);
                        }
                    }
                    retained.retain(|(_, created)| *created >= now - WINDOW_MS);
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
                .map(|m| m.id.parse::<ReplayCursor>().unwrap().position)
                .collect();
            assert_eq!(
                records,
                retained.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
                "seed={seed}, turn={turn}"
            );
            assert_eq!(store.replay_cursor("events", None).unwrap(), Some(latest));
            assert_eq!(
                store.replay_cursor("events", Some(latest)).unwrap(),
                Some(latest)
            );
            let floor = ReplayCursor {
                position: highest_deleted,
                ..latest
            };
            assert_eq!(
                store.replay_cursor("events", Some(floor)).unwrap(),
                Some(floor)
            );
            if highest_deleted > 0 {
                assert!(matches!(
                    store.replay_cursor(
                        "events",
                        Some(ReplayCursor {
                            position: highest_deleted - 1,
                            ..latest
                        })
                    ),
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
    };
    for seed in history::seeds() {
        assert_eq!(
            run(seed, 0),
            run(seed, 1),
            "seed={seed}: logical trace changed"
        );
    }
}
