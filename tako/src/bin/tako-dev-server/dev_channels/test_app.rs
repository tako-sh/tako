//! A fake app serving the internal channel endpoints, and a dev proxy that
//! routes to it, for dev-server channel tests.

use super::*;
use pingora_core::protocols::l4::stream::Stream;
use serde_json::{Value, json};
use std::net::SocketAddr;
use tokio::net::TcpListener;

pub(super) const TOKEN: &str = "test-token";

/// Every `/channels/authorize` body the app received, in order.
pub(super) type AuthorizeCalls = Arc<Mutex<Vec<Value>>>;

/// Serve `definitions` from the registry and answer `/channels/authorize`
/// with `authorize(request body)`.
pub(super) async fn fake_app(
    definitions: Value,
    authorize: impl Fn(&Value) -> (u16, Value) + Send + Sync + 'static,
) -> (u16, AuthorizeCalls) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let calls = AuthorizeCalls::default();
    let authorize = Arc::new(authorize);
    let recorded = calls.clone();
    tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let (definitions, authorize, calls) =
                (definitions.clone(), authorize.clone(), recorded.clone());
            tokio::spawn(async move {
                let mut session = Session::new_h1(Box::new(Stream::from(socket)));
                session.read_request().await.unwrap();
                assert_eq!(session.req_header().headers["host"], "app.tako");
                assert_eq!(session.req_header().headers["x-tako-internal-token"], TOKEN);
                let mut body = Vec::new();
                while let Some(chunk) = session.read_request_body().await.unwrap() {
                    body.extend_from_slice(&chunk);
                }
                let (status, data) = match session.req_header().uri.path() {
                    "/channels/registry" => (200, definitions),
                    "/channels/authorize" => {
                        let input: Value = serde_json::from_slice(&body).unwrap();
                        calls.lock().unwrap().push(input.clone());
                        authorize(&input)
                    }
                    path => panic!("unexpected app request {path}"),
                };
                let body = data.to_string();
                let mut header = ResponseHeader::build(status, None).unwrap();
                header.insert_header("Content-Length", body.len()).unwrap();
                header
                    .insert_header("Content-Type", "application/json")
                    .unwrap();
                header
                    .insert_header("x-tako-internal-token", TOKEN)
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
        }
    });
    (port, calls)
}

/// The SDK's answer for an allowed operation: `ok` plus the definition's
/// lifecycle and transport, as `definitionLifecycleConfig` builds it.
pub(super) fn grant(definition: &Value) -> Value {
    let mut grant = json!({"ok": true});
    for field in [
        "transport",
        "ephemeral",
        "replayWindowMs",
        "inactivityTtlMs",
        "keepaliveIntervalMs",
        "maxConnectionLifetimeMs",
    ] {
        if let Some(value) = definition.get(field) {
            grant[field] = value.clone();
        }
    }
    grant
}

/// Serve `try_handle` for the app on `app_port`; returns the proxy address
/// and the app's channel store key.
pub(super) async fn dev_proxy(app_port: u16, dev_store: &DevChannelStore) -> (SocketAddr, String) {
    let routes = crate::proxy::Routes::default();
    routes.set_routes("app".into(), vec!["app.test".into()], app_port, true);
    routes.set_store_auth("app", "app.tako".into(), TOKEN.into());
    let target = routes.lookup("app.test", "/").unwrap();
    let app_id = target.channel_store_key.clone();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = listener.local_addr().unwrap();
    let dev_store = dev_store.clone();
    tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let (dev_store, target) = (dev_store.clone(), target.clone());
            tokio::spawn(async move {
                let mut session = Session::new_h1(Box::new(Stream::from(socket)));
                session.read_request().await.unwrap();
                let path = session.req_header().uri.path().to_owned();
                let _ = try_handle(&mut session, &dev_store, &target, &path, "GET").await;
            });
        }
    });
    (endpoint, app_id)
}
