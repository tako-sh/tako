use super::*;
use pingora_core::protocols::l4::stream::Stream;
use pingora_proxy::Session;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

fn proxy_for_app(root: &std::path::Path) -> TakoProxy {
    let manager = Arc::new(AppManager::new(root.into()));
    let app = manager.register_app(AppConfig {
        name: "security-test".into(),
        version: "v1".into(),
        path: root.into(),
        ..Default::default()
    });
    let instance = app.allocate_instance();
    app.set_instance_state(&instance, InstanceState::Healthy);
    let lb = Arc::new(LoadBalancer::new(manager));
    lb.register_app(app);
    let mut routes = RouteTable::default();
    routes.set_app_routes("security-test".into(), vec!["app.example".into()]);
    TakoProxy::new(
        lb,
        Arc::new(RwLock::new(routes)),
        ProxyConfig {
            redirect_http_to_https: false,
            ..Default::default()
        },
        Arc::new(ColdStartManager::new(ColdStartConfig::default())),
        CloudflareIpRanges::default(),
    )
}

async fn request(proxy: TakoProxy, request: &str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut session = Session::new_h1(Box::new(Stream::from(socket)));
        session.read_request().await.unwrap();
        let mut ctx = proxy.new_ctx();
        assert!(proxy.request_filter(&mut session, &mut ctx).await.unwrap());
        proxy.logging(&mut session, None, &mut ctx).await;
    });
    let mut client = TcpStream::connect(addr).await.unwrap();
    client.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), client.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    server.await.unwrap();
    String::from_utf8(response).unwrap()
}

#[tokio::test]
async fn public_request_cannot_read_symlinked_directory_index() {
    let temp = TempDir::new().unwrap();
    std::fs::create_dir_all(temp.path().join("public/leak.css")).unwrap();
    let secret = temp.path().join("private.txt");
    std::fs::write(&secret, "SYNTHETIC_PRIVATE_DATA").unwrap();
    std::os::unix::fs::symlink(secret, temp.path().join("public/leak.css/index.html")).unwrap();
    let response = request(
        proxy_for_app(temp.path()),
        "GET /leak.css HTTP/1.1\r\nHost: app.example\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 400"), "{response}");
    assert!(!response.contains("SYNTHETIC_PRIVATE_DATA"));
}

#[tokio::test]
async fn cookie_websocket_rejects_foreign_origin_before_contacting_app() {
    let temp = TempDir::new().unwrap();
    let proxy = proxy_for_app(temp.path());
    proxy
        .channel_registry
        .get_or_load("security-test", "v1", "chat", || async {
            Ok(vec![crate::channels::ChannelDefinitionMeta {
                channel: "chat".into(),
                params_schema: serde_json::json!({"type":"object"}),
                auth: crate::channels::ChannelAuthScheme::Required {
                    header_name: None,
                    cookie_name: Some("session".into()),
                },
                transport: Some(crate::channels::ChannelTransport::Ws),
                lifecycle: Default::default(),
            }])
        })
        .await
        .unwrap();
    let response = request(proxy,
        "GET /_tako/channels/chat HTTP/1.1\r\nHost: app.example\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nCookie: session=fixture\r\nOrigin: https://evil.example\r\n\r\n").await;
    assert!(response.starts_with("HTTP/1.1 403"), "{response}");
    assert!(!response.contains("101 Switching"));
}
