use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

async fn connect(addr: std::net::SocketAddr) -> TcpStream {
    let mut client = TcpStream::connect(addr).await.unwrap();
    client
        .write_all(
            format!("GET /_tako/channels/ws HTTP/1.1\r\nHost: feed.example\r\n{UPGRADE}\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    let mut head = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !head.ends_with(b"\r\n\r\n") {
            head.push(client.read_u8().await.unwrap());
        }
    })
    .await
    .unwrap();
    assert!(head.starts_with(b"HTTP/1.1 101"), "{head:?}");
    client
}

async fn subscribe(client: &mut TcpStream, id: &str) {
    let body =
        json!({"type":"subscribe", "id":id, "channel":"feed", "query":"id=1", "from":"start"})
            .to_string();
    assert!(body.len() < 126);
    // Client frames must be masked; an all-zero mask leaves the payload intact.
    let mut frame = vec![0x81, 0x80 | body.len() as u8, 0, 0, 0, 0];
    frame.extend_from_slice(body.as_bytes());
    client.write_all(&frame).await.unwrap();
}

async fn frame(client: &mut TcpStream) -> (u8, Vec<u8>) {
    tokio::time::timeout(Duration::from_secs(2), async {
        let opcode = client.read_u8().await.unwrap() & 0x0f;
        let size = client.read_u8().await.unwrap();
        assert_eq!(size & 0x80, 0, "server frames are unmasked");
        let size = match size {
            126 => client.read_u16().await.unwrap() as usize,
            127 => panic!("unexpectedly large frame"),
            size => size as usize,
        };
        let mut payload = vec![0; size];
        client.read_exact(&mut payload).await.unwrap();
        (opcode, payload)
    })
    .await
    .expect("shared socket response")
}

async fn control(client: &mut TcpStream) -> Value {
    let (opcode, payload) = frame(client).await;
    assert_eq!(opcode, 0x1);
    serde_json::from_slice(&payload).unwrap()
}

async fn reconnect_required(client: &mut TcpStream, id: &str) {
    let closed = control(client).await;
    assert_eq!(closed["type"], "closed");
    assert_eq!(closed["id"], id);
    assert_eq!(closed["code"], 1013);
    let (opcode, payload) = frame(client).await;
    assert_eq!(opcode, 0x8);
    assert_eq!(u16::from_be_bytes([payload[0], payload[1]]), 1012);
}

#[tokio::test]
async fn compose_subscriptions_keep_the_handshake_service() {
    let temp = TempDir::new().unwrap();
    let proxy = proxy_for_feed(temp.path(), feed_app().await);
    let app = proxy.lb.app_manager().get_app("feed-app").unwrap();
    let mut config = app.config.read().clone();
    config.compose = Some(
        serde_json::from_value(json!({
            "file": "compose.yaml",
            "services": {
                "api": {"sdk": true, "envs": {"": {"route": "feed.example"}}},
                "web": {"http": true}
            }
        }))
        .unwrap(),
    );
    config.env_vars.insert(
        crate::release::TAKO_APP_DATA_DIR_ENV.into(),
        temp.path().join("data/app").display().to_string(),
    );
    std::fs::write(temp.path().join("compose.yaml"), "services: {}\n").unwrap();
    let runtime = crate::compose_runtime::for_config(&config).unwrap();
    let instance = app.get_instances().into_iter().next().unwrap();
    app.set_instance_state(&instance, InstanceState::Starting);
    instance.set_compose(runtime, "fixture-api".into(), "api".into());
    app.update_config(config.clone());
    app.set_instance_state(&instance, InstanceState::Healthy);
    proxy
        .routes
        .write()
        .set_config_routes(&config, vec!["feed.example".into()]);
    assert!(
        proxy.lb.get_backend("feed-app").is_none(),
        "a group has no implicit app"
    );

    let mut client = connect(serve(proxy).await).await;
    for id in ["layout", "page"] {
        subscribe(&mut client, id).await;
        assert_eq!(
            control(&mut client).await,
            json!({"type":"subscribed", "id":id})
        );
    }
}

#[tokio::test]
async fn channel_routes_of_plain_compose_services_explain_sdk_true() {
    let temp = TempDir::new().unwrap();
    let proxy = proxy_for_feed(temp.path(), feed_app().await);
    let app = proxy.lb.app_manager().get_app("feed-app").unwrap();
    let mut config = app.config.read().clone();
    config.compose = Some(
        serde_json::from_value(json!({
            "file": "compose.yaml",
            "services": {"nginx": {"envs": {"": {"route": "feed.example"}}}}
        }))
        .unwrap(),
    );
    app.update_config(config.clone());
    proxy
        .routes
        .write()
        .set_config_routes(&config, vec!["feed.example".into()]);
    let addr = serve(proxy).await;
    for path in ["/_tako/channels/sse/feed?id=1", "/_tako/stores/cart"] {
        let mut client = TcpStream::connect(addr).await.unwrap();
        client
            .write_all(
                format!("GET {path} HTTP/1.1\r\nHost: feed.example\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        let mut response = String::new();
        tokio::time::timeout(Duration::from_secs(2), client.read_to_string(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert!(response.starts_with("HTTP/1.1 404"), "{path}: {response}");
        assert!(
            response.contains("Compose service 'nginx' isn't marked sdk: true in x-tako"),
            "{path}: {response}"
        );
    }
}

#[tokio::test]
async fn credential_rotation_reconnects_shared_socket_and_refreshes_credentials() {
    let temp = TempDir::new().unwrap();
    let proxy = proxy_for_feed(temp.path(), feed_app().await);
    let resolutions = Arc::new(AtomicUsize::new(0));
    let calls = resolutions.clone();
    // The fixture's resolver owns the app's database: keep it alive.
    let previous = proxy.channel_target.read().clone().unwrap();
    proxy.set_channel_target_resolver(Arc::new(move |app| {
        calls.fetch_add(1, Ordering::SeqCst);
        previous(app)
    }));
    let addr = serve(proxy.clone()).await;
    let mut client = connect(addr).await;
    for id in ["layout", "page"] {
        subscribe(&mut client, id).await;
        assert_eq!(
            control(&mut client).await,
            json!({"type":"subscribed", "id":id})
        );
    }
    assert_eq!(resolutions.load(Ordering::SeqCst), 1);

    proxy
        .channel_credentials_generation
        .fetch_add(1, Ordering::AcqRel);
    subscribe(&mut client, "new-page").await;
    reconnect_required(&mut client, "new-page").await;

    // Even with a layout holding the old pool open, logical retries must move
    // to a new socket and resolve credentials through the normal cache path.
    let mut client = connect(addr).await;
    subscribe(&mut client, "new-page").await;
    assert_eq!(control(&mut client).await["type"], "subscribed");
    assert_eq!(resolutions.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn credential_rotation_during_handshake_invalidates_the_captured_store() {
    let temp = TempDir::new().unwrap();
    let proxy = proxy_for_feed(temp.path(), feed_app().await);
    let generation = proxy.channel_credentials_generation.clone();
    let previous = proxy.channel_target.read().clone().unwrap();
    proxy.set_channel_target_resolver(Arc::new(move |app| {
        // Simulate a deployment landing while the initial credentials resolve.
        generation
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .ok();
        previous(app)
    }));
    let addr = serve(proxy).await;
    let mut client = connect(addr).await;
    subscribe(&mut client, "page").await;
    reconnect_required(&mut client, "page").await;

    let mut client = connect(addr).await;
    subscribe(&mut client, "page").await;
    assert_eq!(control(&mut client).await["type"], "subscribed");
}

#[tokio::test]
async fn replaced_database_reconnects_shared_socket_and_replays_from_the_new_store() {
    let temp = TempDir::new().unwrap();
    let proxy = proxy_for_feed(temp.path(), feed_app().await);
    let addr = serve(proxy.clone()).await;
    let mut client = connect(addr).await;
    subscribe(&mut client, "layout").await;
    assert_eq!(control(&mut client).await["type"], "subscribed");

    // A deploy binds the app to another database, and another server's
    // writer publishes there first.
    let replaced = crate::channels::test_replay::TestReplay::start("feed-app/production");
    let target = replaced.target();
    let config = target.config(&crate::native_channel_client::app_scope("feed-app").unwrap());
    let previous = proxy.channel_target.read().clone().unwrap();
    proxy.set_channel_target_resolver(Arc::new(move |_| {
        let _databases = (&previous, &replaced);
        Ok(target.clone())
    }));
    proxy
        .channel_credentials_generation
        .fetch_add(1, Ordering::AcqRel);
    let store = tokio::task::spawn_blocking(move || {
        Arc::new(tako_channels::ChannelStore::open_config(config).unwrap())
    })
    .await
    .unwrap();
    let publication = tako_channels::TrustedPublication::parse(
        "feed",
        &json!({"id":1}),
        json!({"type":"token", "data":"new backend"}),
    )
    .unwrap();
    super::publish_trusted(&store, publication).await.unwrap();

    subscribe(&mut client, "page").await;
    reconnect_required(&mut client, "page").await;
    let mut client = connect(addr).await;
    subscribe(&mut client, "page").await;
    assert_eq!(control(&mut client).await["type"], "subscribed");
    let event = control(&mut client).await;
    assert_eq!(event["type"], "data");
    assert_eq!(event["id"], "page");
    assert_eq!(event["end"], true);
    let message: Value = serde_json::from_str(event["data"].as_str().unwrap()).unwrap();
    assert_eq!(message["data"], "new backend");
}
