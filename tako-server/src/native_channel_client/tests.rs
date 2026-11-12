use super::*;
use std::{
    collections::VecDeque,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::sync::Mutex;

#[derive(Default)]
pub(super) struct FakeBroker {
    streams: Mutex<VecDeque<Subscription>>,
    pub(super) calls: parking_lot::Mutex<Vec<&'static str>>,
    pub(super) reject: AtomicBool,
    pub(super) publishes: AtomicUsize,
    /// Channel names publications reached the broker under.
    pub(super) channels: parking_lot::Mutex<Vec<String>>,
}
impl FakeBroker {
    async fn stream(&self) -> DuplexStream {
        let (client, server) = tokio::io::duplex(16 * 1024);
        self.streams
            .lock()
            .await
            .push_back(Subscription::from_stream(client));
        server
    }
}
#[async_trait]
impl BrokerLink for FakeBroker {
    async fn register(&self, _: AppEnv, _: Option<String>) -> io::Result<()> {
        self.calls.lock().push("register");
        if self.reject.load(Ordering::SeqCst) {
            Err(io::Error::other("secret URL must never escape"))
        } else {
            Ok(())
        }
    }
    async fn subscribe(&self, _: AppEnv) -> io::Result<Subscription> {
        self.calls.lock().push("subscribe");
        self.streams
            .lock()
            .await
            .pop_front()
            .ok_or_else(|| io::Error::other("offline"))
    }
    async fn sync(&self, _: AppEnv, _: String, _: ChannelAuthResponse) -> io::Result<()> {
        self.calls.lock().push("sync");
        if self.reject.load(Ordering::SeqCst) {
            Err(io::Error::other("offline"))
        } else {
            Ok(())
        }
    }
    async fn publish(
        &self,
        _: AppEnv,
        channel: String,
        payload: ChannelPublishPayload,
        _: bool,
    ) -> io::Result<ChannelMessage> {
        self.calls.lock().push("publish");
        self.publishes.fetch_add(1, Ordering::SeqCst);
        self.channels.lock().push(channel.clone());
        if self.reject.load(Ordering::SeqCst) {
            return Err(io::Error::other("offline"));
        }
        Ok(ChannelMessage {
            id: "ack".into(),
            event_id: None,
            channel,
            r#type: payload.r#type,
            data: payload.data,
        })
    }
    async fn update_presence(&self, _: AppEnv, _: String, _: PresenceMember) -> io::Result<()> {
        self.calls.lock().push("update_presence");
        Ok(())
    }
    async fn presence(&self, _: AppEnv, _: String) -> io::Result<Vec<PresenceMember>> {
        self.calls.lock().push("presence");
        Ok(Vec::new())
    }
    async fn remove_presence(&self, _: AppEnv, _: String, _: String) -> io::Result<()> {
        self.calls.lock().push("remove_presence");
        Ok(())
    }
}
fn scope() -> AppEnv {
    app_scope("chat/production").unwrap()
}
fn auth() -> ChannelAuthResponse {
    serde_json::from_value(serde_json::json!({"ok":true})).unwrap()
}
fn message() -> ChannelMessage {
    ChannelMessage {
        id: "1".into(),
        event_id: None,
        channel: "events".into(),
        r#type: "update".into(),
        data: serde_json::json!(42),
    }
}
async fn notice(stream: &mut DuplexStream, notification: Notification) {
    let bytes = serde_json::to_vec(
        &serde_json::json!({"kind":"notification", "notification": notification}),
    )
    .unwrap();
    stream.write_u32(bytes.len() as u32).await.unwrap();
    stream.write_all(&bytes).await.unwrap();
}
async fn attached() -> (ProxyChannelStore, Arc<FakeBroker>, DuplexStream) {
    let link = Arc::new(FakeBroker::default());
    let server = link.stream().await;
    let store = Arc::new(ChannelStore::open_in_memory().unwrap());
    let proxy = ProxyChannelStore::subscribe(store, scope(), None, link.clone(), None)
        .await
        .unwrap();
    (proxy, link, server)
}

#[tokio::test]
async fn initial_registration_and_subscription_precede_reader_exposure() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("channels.sqlite");
    let writer = ChannelStore::open_sqlite(&path).unwrap();
    let link = Arc::new(FakeBroker::default());
    let _server = link.stream().await;
    let proxy = ProxyChannelStore::open_with_link(
        ChannelStoreConfig::sqlite(path),
        scope(),
        Some(link.clone()),
    )
    .await
    .unwrap();
    proxy.ensure_ready().await.unwrap();
    assert_eq!(*link.calls.lock(), vec!["register", "subscribe"]);
    assert!(proxy.read_after("events", None, 100).unwrap().is_empty());
    drop(writer);
}

#[tokio::test]
async fn replaced_sqlite_tree_invalidates_cached_reader_and_notifications() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("data");
    std::fs::create_dir(&root).unwrap();
    let path = root.join("channels.sqlite");
    let _writer = ChannelStore::open_sqlite(&path).unwrap();
    let link = Arc::new(FakeBroker::default());
    let mut notices = link.stream().await;
    let proxy = ProxyChannelStore::open_with_link(
        ChannelStoreConfig::sqlite(path),
        scope(),
        Some(link.clone()),
    )
    .await
    .unwrap();
    let mut resets = proxy.resets("events");
    std::fs::rename(&root, temp.path().join("previous-data")).unwrap();
    std::fs::create_dir(&root).unwrap();
    assert!(
        proxy.check_backend(None).is_err(),
        "cache must reopen a replaced database"
    );
    assert!(
        proxy.ensure_ready().await.is_err(),
        "old readers must fail closed"
    );
    notice(&mut notices, Notification::Gap).await;
    tokio::time::timeout(Duration::from_secs(1), resets.changed())
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while !proxy.native.as_ref().unwrap().task.is_finished() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("stale subscriptions must stop instead of reconnecting the old reader");
}
#[tokio::test]
async fn notification_delivery_does_not_reinsert_or_republish() {
    let (proxy, _link, mut server) = attached().await;
    let mut changes = proxy.changes("events");
    let mut publications = proxy.publications();
    notice(
        &mut server,
        Notification::Committed {
            message: message(),
            ephemeral: false,
        },
    )
    .await;
    tokio::time::timeout(Duration::from_secs(1), changes.changed())
        .await
        .unwrap()
        .unwrap();
    assert!(proxy.read_after("events", None, 100).unwrap().is_empty());
    assert!(publications.try_recv().is_err());
    let mut events = proxy.subscribe_ephemeral("events").unwrap();
    notice(
        &mut server,
        Notification::Committed {
            message: message(),
            ephemeral: true,
        },
    )
    .await;
    assert_eq!(
        *tokio::time::timeout(Duration::from_secs(1), events.recv())
            .await
            .unwrap()
            .unwrap(),
        message()
    );
    assert!(publications.try_recv().is_err());
    let mut resets = proxy.resets("events");
    notice(&mut server, Notification::Gap).await;
    tokio::time::timeout(Duration::from_secs(1), resets.changed())
        .await
        .unwrap()
        .unwrap();
    assert!(proxy.read_after("events", None, 100).unwrap().is_empty());
}
#[tokio::test]
async fn disconnect_resets_and_reconnects_registration_before_delivery() {
    let (proxy, link, first) = attached().await;
    let mut second = link.stream().await;
    let mut resets = proxy.resets("events");
    drop(first);
    tokio::time::timeout(Duration::from_secs(1), resets.changed())
        .await
        .unwrap()
        .unwrap();
    proxy.ensure_ready().await.unwrap();
    assert_eq!(
        *link.calls.lock(),
        vec!["subscribe", "register", "subscribe"]
    );
    let mut changes = proxy.changes("events");
    notice(
        &mut second,
        Notification::Wake {
            channel: "events".into(),
        },
    )
    .await;
    tokio::time::timeout(Duration::from_secs(1), changes.changed())
        .await
        .unwrap()
        .unwrap();
    assert!(proxy.read_after("events", None, 100).unwrap().is_empty());
}
#[tokio::test]
async fn native_sync_and_publish_never_write_the_reader_or_deliver_ack_twice() {
    let (proxy, link, _server) = attached().await;
    let mut changes = proxy.changes("events");
    let mut publications = proxy.publications();
    proxy.sync_channel_async("events", &auth()).await.unwrap();
    proxy
        .publish(
            "events",
            &ChannelPublishPayload {
                r#type: "update".into(),
                data: serde_json::json!(1),
            },
            false,
        )
        .await
        .unwrap();
    assert_eq!(*link.calls.lock(), vec!["subscribe", "sync", "publish"]);
    assert!(proxy.read_after("events", None, 100).unwrap().is_empty());
    assert!(!changes.has_changed().unwrap());
    assert!(publications.try_recv().is_err());
    link.reject.store(true, Ordering::SeqCst);
    assert!(
        proxy
            .publish(
                "events",
                &ChannelPublishPayload {
                    r#type: "update".into(),
                    data: serde_json::json!(2)
                },
                true
            )
            .await
            .is_err()
    );
    assert!(proxy.read_after("events", None, 100).unwrap().is_empty());
    assert!(publications.try_recv().is_err());
    changes.borrow_and_update();
}
#[tokio::test]
async fn backend_change_is_explicit_and_subscription_task_does_not_retain_store() {
    let (proxy, _link, mut server) = attached().await;
    let error = proxy
        .check_backend(Some("postgres://secret"))
        .unwrap_err()
        .to_string();
    assert!(error.contains("configuration changed"));
    assert!(!error.contains("secret"));
    let weak = Arc::downgrade(proxy.store());
    drop(proxy);
    let mut byte = [0];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), server.read(&mut byte))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    assert!(weak.upgrade().is_none());
}
#[tokio::test]
async fn failed_initial_subscription_does_not_expose_a_ready_reader() {
    let link = Arc::new(FakeBroker::default());
    let store = Arc::new(ChannelStore::open_in_memory().unwrap());
    assert!(
        ProxyChannelStore::subscribe(store, scope(), None, link, None)
            .await
            .is_err()
    );
}
#[test]
fn canonical_scope_defaults_to_production_without_allowing_paths() {
    assert_eq!(app_scope("chat").unwrap().as_str(), "chat/production");
    for input in ["../chat", "chat/prod/extra", "/chat", "chat/../etc"] {
        assert!(app_scope(input).is_err());
    }
}

#[tokio::test]
async fn terminated_subscription_worker_cannot_leave_reader_marked_ready() {
    let (proxy, _link, _server) = attached().await;
    proxy.native.as_ref().unwrap().task.abort();
    tokio::task::yield_now().await;
    assert!(proxy.ensure_ready().await.is_err());
}

#[tokio::test]
async fn native_presence_uses_ipc_writer_and_drop_cleanup_keeps_wrapper_until_leave() {
    let (proxy, link, _server) = attached().await;
    let proxy = Arc::new(proxy);
    let mut presence = tako_channels::PresenceSession::new_with_writer(
        proxy.clone(),
        "room",
        Some("verified-user".into()),
    )
    .unwrap();
    presence
        .update(br#"{"type":"tako.presence","data":{"typing":true}}"#)
        .await
        .unwrap();
    presence.snapshot().await.unwrap();
    assert_eq!(
        *link.calls.lock(),
        vec!["subscribe", "update_presence", "presence"]
    );
    assert!(proxy.store().presence("room", 0).unwrap().is_empty());
    drop(presence);
    tokio::time::timeout(Duration::from_secs(1), async {
        while !link.calls.lock().contains(&"remove_presence") {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
