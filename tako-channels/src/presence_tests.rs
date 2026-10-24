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
