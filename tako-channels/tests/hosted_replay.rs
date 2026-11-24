//! One channel history, accessed on the database's server and through an app server.
use std::net::SocketAddr;
use std::sync::Arc;
use tako_channels::{
    ChannelError, ChannelPublishPayload, ChannelStore, ChannelStoreConfig, ReplayCursor,
};
use tako_db::{
    Binding, DatabaseHost, DatabaseId, Home, HostConfig, generate_token, hash_token,
    http::{self, Access},
};

async fn serve(host: &DatabaseHost, access: Access) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(http::serve(host.clone(), listener, access));
    address
}

fn bind(
    host: &DatabaseHost,
    address: SocketAddr,
    db: &DatabaseId,
    app: &str,
    home: Home,
) -> ChannelStoreConfig {
    let token = generate_token();
    host.set_bindings(
        &format!("{app}#channels"),
        vec![Binding {
            token_hash: hash_token(&token),
            database: db.clone(),
            home,
        }],
    );
    ChannelStoreConfig::hosted(format!("http://{address}"), token, app)
}

#[tokio::test]
async fn two_writers_and_a_remote_reader_share_one_ordered_history_but_other_apps_do_not() {
    let dir = tempfile::tempdir().unwrap();
    let db = DatabaseId::generate();
    let path = dir.path().join("database.sqlite");
    DatabaseHost::create_file(&path).unwrap();
    let home = DatabaseHost::new(HostConfig::default());
    home.register(db.clone(), path).unwrap();
    let edge = DatabaseHost::new(HostConfig::default());
    let apps = serve(&home, Access::Apps).await;
    let upstream = serve(
        &home,
        Access::Peers {
            allow: Arc::new(|ip| ip.is_loopback()),
        },
    )
    .await;
    let remote_apps = serve(&edge, Access::Apps).await;
    let app = "chat/production";
    let key = generate_token();
    home.set_remote_grants(&format!("{app}#channels"), [db.clone()].into(), &key);
    let a = bind(&home, apps, &db, app, Home::Local);
    let b = bind(&edge, remote_apps, &db, app, Home::Remote { upstream, key });
    let c = bind(&home, apps, &db, "helpdesk/production", Home::Local);
    tokio::task::spawn_blocking(move || {
        let a = ChannelStore::open_config(a).unwrap();
        let b_writer = ChannelStore::open_config(b.clone()).unwrap();
        let b_reader = ChannelStore::open_reader_config(b).unwrap();
        let c = ChannelStore::open_config(c).unwrap();
        let payload = |text: &str| ChannelPublishPayload {
            r#type: "message".into(),
            data: text.into(),
        };
        let room = "room-42";
        let first = a.append(room, &payload("Alice")).unwrap();
        let cursor: ReplayCursor = first.id.parse().unwrap();
        let second = b_writer.append(room, &payload("Bob")).unwrap();
        let read = b_reader.read_after(room, Some(cursor), 20).unwrap();
        assert_eq!(read, vec![second.clone()]);
        assert_eq!(a.read_after(room, None, 20).unwrap(), vec![first, second]);
        assert!(c.read_after(room, None, 20).unwrap().is_empty());
        assert!(matches!(
            c.replay_cursor(room, Some(cursor)),
            Err(ChannelError::StaleCursor)
        ));
        let end = b_writer.close(room).unwrap();
        assert_eq!(a.close(room).unwrap().id, end.id);
        assert!(matches!(
            a.append(room, &payload("late")),
            Err(ChannelError::StreamClosed)
        ));
        assert!(c.append(room, &payload("still open")).is_ok());
        assert!(b_reader.append(room, &payload("read only")).is_err());
    })
    .await
    .unwrap();
}
