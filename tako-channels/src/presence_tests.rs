use crate::{ChannelStore, PresenceMember};
use serde_json::json;

#[test]
fn presence_scopes_are_canonical_and_isolate_conversations() {
    use crate::presence_scope;
    assert_eq!(
        presence_scope("chat", &json!({"chatId":"one", "version":1})),
        presence_scope("chat", &json!({"version":1, "chatId":"one"}))
    );
    assert_ne!(
        presence_scope("chat", &json!({"chatId":"one"})),
        presence_scope("chat", &json!({"chatId":"two"}))
    );
    let store = ChannelStore::open_in_memory().unwrap();
    let now = crate::store::now_unix_ms();
    let member = PresenceMember {
        connection_id: "one".into(),
        subject: Some("user".into()),
        data: json!({}),
    };
    store
        .update_presence(
            &presence_scope("chat", &json!({"chatId":"one"})),
            &member,
            now,
        )
        .unwrap();
    assert!(
        store
            .presence(&presence_scope("chat", &json!({"chatId":"two"})), now)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn presence_tracks_connections_updates_and_channel_isolation() {
    let store = ChannelStore::open_in_memory().unwrap();
    let now = crate::store::now_unix_ms();
    let member = PresenceMember {
        connection_id: "tab-1".into(),
        subject: Some("user-1".into()),
        data: json!({"typing": false}),
    };
    store.update_presence("chat", &member, now).unwrap();
    let mut second = member.clone();
    second.connection_id = "tab-2".into();
    store.update_presence("chat", &second, now).unwrap();
    assert_eq!(store.presence("chat", now + 1).unwrap().len(), 2);
    assert!(store.presence("other", now + 1).unwrap().is_empty());
    let mut updated = member.clone();
    updated.data = json!({"typing": true});
    store.update_presence("chat", &updated, now + 100).unwrap();
    assert_eq!(store.presence("chat", now + 101).unwrap()[0], updated);
    store.remove_presence("chat", "tab-1").unwrap();
    assert_eq!(store.presence("chat", now + 101).unwrap(), vec![second]);
}

#[test]
fn presence_expires_and_is_not_replayed() {
    let store = ChannelStore::open_in_memory().unwrap();
    let now = crate::store::now_unix_ms();
    let member = PresenceMember {
        connection_id: "tab-1".into(),
        subject: None,
        data: json!(null),
    };
    store.update_presence("chat", &member, now).unwrap();
    assert_eq!(store.presence("chat", now + 44_999).unwrap(), vec![member]);
    assert!(store.presence("chat", now + 45_000).unwrap().is_empty());
    assert!(store.read_after("chat", None, 100).unwrap().is_empty());
}

#[test]
fn presence_rejects_large_client_metadata() {
    let store = ChannelStore::open_in_memory().unwrap();
    let now = crate::store::now_unix_ms();
    let member = PresenceMember {
        connection_id: "tab-1".into(),
        subject: None,
        data: json!("x".repeat(16_385)),
    };
    assert!(store.update_presence("chat", &member, now).is_err());
}

#[tokio::test]
async fn session_uses_verified_identity_and_cleans_up_on_drop() {
    use crate::PresenceSession;
    use std::sync::Arc;
    let store = Arc::new(ChannelStore::open_in_memory().unwrap());
    let mut session =
        PresenceSession::new(store.clone(), "lobby", Some("verified".into())).unwrap();
    session.update(br#"{"type":"tako.presence","subject":"forged","connectionId":"forged","data":{"typing":true}}"#).await.unwrap();
    let snapshot: serde_json::Value =
        serde_json::from_str(&session.snapshot().await.unwrap().unwrap()).unwrap();
    assert_eq!(snapshot["members"][0]["subject"], "verified");
    assert_ne!(snapshot["members"][0]["connectionId"], "forged");
    assert!(session.snapshot().await.unwrap().is_none());
    drop(session);
    for _ in 0..100 {
        let members = store
            .run(|store| store.presence("lobby", crate::store::now_unix_ms()))
            .await
            .unwrap();
        if members.is_empty() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("Disconnected session remained present");
}

#[tokio::test]
async fn cancellation_removes_presence_after_in_flight_joins_and_renewals_commit() {
    use crate::{ChannelError, ChannelWriter, PresenceSession};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use tokio::sync::Notify;

    struct DelayedWriter {
        store: Arc<ChannelStore>,
        delay: AtomicBool,
        started: Notify,
        release: Arc<Notify>,
        committed: Arc<Notify>,
        removed: Notify,
    }
    #[async_trait::async_trait]
    impl ChannelWriter for DelayedWriter {
        async fn sync_channel(
            &self,
            _: &str,
            _: &crate::ChannelAuthResponse,
        ) -> Result<(), ChannelError> {
            unreachable!("presence sessions do not sync channels")
        }
        async fn publish(
            &self,
            _: &str,
            _: &crate::ChannelPublishPayload,
            _: bool,
        ) -> Result<crate::ChannelMessage, ChannelError> {
            unreachable!("presence sessions do not publish messages")
        }
        async fn update_presence(
            &self,
            channel: &str,
            member: &PresenceMember,
        ) -> Result<(), ChannelError> {
            if !self.delay.load(Ordering::Relaxed) {
                return self
                    .store
                    .update_presence(channel, member, crate::store::now_unix_ms());
            }
            let (store, release, committed) = (
                self.store.clone(),
                self.release.clone(),
                self.committed.clone(),
            );
            let (channel, member) = (channel.to_owned(), member.clone());
            self.started.notify_one();
            // Like a database worker or a request already accepted by a broker,
            // this write keeps running if its awaiting caller disappears.
            tokio::spawn(async move {
                release.notified().await;
                store
                    .update_presence(&channel, &member, crate::store::now_unix_ms())
                    .unwrap();
                committed.notify_one();
            })
            .await
            .unwrap();
            Ok(())
        }
        async fn presence(&self, channel: &str) -> Result<Vec<PresenceMember>, ChannelError> {
            self.store.presence(channel, crate::store::now_unix_ms())
        }
        async fn remove_presence(&self, channel: &str, id: &str) -> Result<(), ChannelError> {
            self.store.remove_presence(channel, id)?;
            self.removed.notify_one();
            Ok(())
        }
    }

    for already_joined in [false, true] {
        let writer = Arc::new(DelayedWriter {
            store: Arc::new(ChannelStore::open_in_memory().unwrap()),
            delay: AtomicBool::new(false),
            started: Notify::new(),
            release: Arc::new(Notify::new()),
            committed: Arc::new(Notify::new()),
            removed: Notify::new(),
        });
        let mut session = PresenceSession::new_with_writer(writer.clone(), "room", None).unwrap();
        if already_joined {
            session
                .update(br#"{"type":"tako.presence","data":{}}"#)
                .await
                .unwrap();
        }
        writer.delay.store(true, Ordering::Relaxed);
        let task = tokio::spawn(async move {
            session
                .update(br#"{"type":"tako.presence","data":{"typing":true}}"#)
                .await
                .unwrap();
        });
        writer.started.notified().await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        // The page has left, but its backend write has not yet committed.
        writer.release.notify_one();
        writer.committed.notified().await;
        tokio::time::timeout(std::time::Duration::from_secs(2), writer.removed.notified())
            .await
            .expect("cancelled presence update was not cleaned up");
        assert!(
            writer.presence("room").await.unwrap().is_empty(),
            "late update resurrected a disconnected member"
        );
    }
}

#[test]
fn postgres_presence_is_shared_and_scoped_by_app_when_configured() {
    let Ok(url) = std::env::var("TAKO_TEST_POSTGRES_URL") else {
        return;
    };
    let app = format!(
        "presence-test/{}-{}",
        std::process::id(),
        crate::store::now_unix_ms()
    );
    let writer = ChannelStore::open_postgres(&url, &app).unwrap();
    let reader = ChannelStore::open_postgres(&url, &app).unwrap();
    let other = ChannelStore::open_postgres(&url, &format!("{app}-other")).unwrap();
    let member = PresenceMember {
        connection_id: "tab".into(),
        subject: Some("user".into()),
        data: json!({}),
    };
    let now = crate::store::now_unix_ms();
    writer.update_presence("lobby", &member, now).unwrap();
    assert_eq!(reader.presence("lobby", now).unwrap(), vec![member.clone()]);
    assert!(other.presence("lobby", now).unwrap().is_empty());
    writer.remove_presence("lobby", "tab").unwrap();
    assert!(reader.presence("lobby", now).unwrap().is_empty());

    let mut context = [0u8; 4096];
    getrandom::fill(&mut context).unwrap();
    let context: String = context.iter().map(|byte| format!("{byte:02x}")).collect();
    let scope = crate::presence_scope("chat", &json!({"context": context}));
    writer.update_presence(&scope, &member, now).unwrap();
    assert_eq!(reader.presence(&scope, now).unwrap(), vec![member]);
    writer.remove_presence(&scope, "tab").unwrap();
}

#[test]
fn postgres_presence_ignores_each_servers_clock() {
    let Ok(url) = std::env::var("TAKO_TEST_POSTGRES_URL") else {
        return;
    };
    let app = format!(
        "presence-clock/{}-{}",
        std::process::id(),
        crate::store::now_unix_ms()
    );
    let writer = ChannelStore::open_postgres(&url, &app).unwrap();
    let reader = ChannelStore::open_postgres(&url, &app).unwrap();
    let member = PresenceMember {
        connection_id: "tab".into(),
        subject: None,
        data: json!({}),
    };
    writer.update_presence("lobby", &member, 0).unwrap();
    reader.prune_at(i64::MAX);
    assert_eq!(
        reader.presence("lobby", i64::MAX).unwrap(),
        vec![member.clone()]
    );
    writer.update_presence("lobby", &member, i64::MAX).unwrap();
    let mut db = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    db.execute(
        "UPDATE tako_channels.presence SET expires_at = $2 WHERE app_id = $1",
        &[&app, &i64::MAX],
    )
    .unwrap();
    writer.prune_at(0);
    let remaining: i64 = db
        .query_one(
            "SELECT expires_at - (extract(epoch from now()) * 1000)::BIGINT
             FROM tako_channels.presence WHERE app_id = $1",
            &[&app],
        )
        .unwrap()
        .get(0);
    assert!(
        (0..=45_000).contains(&remaining),
        "old lease kept for {remaining} ms"
    );
    db.execute(
        "UPDATE tako_channels.presence SET expires_at = 0 WHERE app_id = $1",
        &[&app],
    )
    .unwrap();
    assert!(reader.presence("lobby", 0).unwrap().is_empty());
    writer.prune_at(0);
    let count: i64 = db
        .query_one(
            "SELECT COUNT(*) FROM tako_channels.presence WHERE app_id = $1",
            &[&app],
        )
        .unwrap()
        .get(0);
    assert_eq!(count, 0);
}
