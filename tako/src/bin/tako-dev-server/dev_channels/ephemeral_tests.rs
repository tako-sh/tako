use super::*;
use pingora_core::protocols::l4::stream::Stream;
use serde_json::json;
use tokio::net::TcpListener;

#[tokio::test]
async fn subscription_uses_registry_mode_before_first_publish() {
    let app_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let app_port = app_listener.local_addr().unwrap().port();
    let app = tokio::spawn(async move {
        let (socket, _) = app_listener.accept().await.unwrap();
        let mut session = Session::new_h1(Box::new(Stream::from(socket)));
        session.read_request().await.unwrap();
        assert_eq!(session.req_header().uri.path(), "/channels/registry");
        assert_eq!(
            session.req_header().headers["x-tako-internal-token"],
            "test-token"
        );
        let body = json!([{
            "channel":"progress", "paramsSchema":{"type":"object"},
            "auth":false, "ephemeral":true, "maxConnectionLifetimeMs":2000,
        }])
        .to_string();
        let mut header = ResponseHeader::build(200, None).unwrap();
        header.insert_header("Content-Length", body.len()).unwrap();
        header
            .insert_header("Content-Type", "application/json")
            .unwrap();
        header
            .insert_header("x-tako-internal-token", "test-token")
            .unwrap();
        session
            .write_response_header(Box::new(header), false)
            .await
            .unwrap();
        session
            .write_response_body(Some(body.into()), true)
            .await
            .unwrap();
    });
    let routes = crate::proxy::Routes::default();
    routes.set_routes("app".into(), vec!["app.test".into()], app_port, true);
    routes.set_store_auth("app", "app.tako".into(), "test-token".into());
    let target = routes.lookup("app.test", "/").unwrap();
    let app_id = target.channel_store_key.clone();
    let dev_store = DevChannelStore::new();
    let server_store = dev_store.clone();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut session = Session::new_h1(Box::new(Stream::from(socket)));
        session.read_request().await.unwrap();
        try_handle(
            &mut session,
            &server_store,
            &target,
            "/_tako/channels/progress",
            "GET",
        )
        .await
        .unwrap();
    });
    let mut response = reqwest::get(format!("http://{endpoint}/_tako/channels/progress"))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let message = dev_store
        .publish(
            &app_id,
            "progress",
            &ChannelPublishPayload {
                r#type: "progress".into(),
                data: json!(42),
            },
            Some(tako_channels::ChannelLifecycle {
                ephemeral: true,
                ..Default::default()
            }),
        )
        .unwrap();
    assert!(message.id.is_empty());
    let chunk = tokio::time::timeout(Duration::from_secs(1), response.chunk())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        std::str::from_utf8(&chunk).unwrap(),
        "data: {\"id\":\"\",\"channel\":\"progress\",\"type\":\"progress\",\"data\":42}\n\n"
    );
    assert!(
        dev_store
            .store_for_app(&app_id)
            .read_after("progress", None, 100)
            .unwrap()
            .is_empty()
    );
    server.abort();
    app.await.unwrap();
}
