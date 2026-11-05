use super::*;
use crate::native_services::PrivateRoute;

impl TakoProxy {
    pub(super) fn native_route(&self, session: &Session) -> PrivateRoute {
        let host = request_host(session.req_header());
        match &self.native_services {
            Some(services) => {
                let (destination, peer) = socket_addresses(session);
                services.route(host, destination, peer)
            }
            None if crate::native_proxy::reserved_host(host) => PrivateRoute::Forbidden,
            None => PrivateRoute::Public,
        }
    }
}

pub(super) fn socket_addresses(session: &Session) -> (Option<IpAddr>, Option<IpAddr>) {
    #[cfg(unix)]
    if let Some(socket) = session
        .digest()
        .and_then(|digest| digest.socket_digest.as_ref())
    {
        return (
            socket
                .kernel_local_addr()
                .and_then(|address| address.as_inet().map(|address| address.ip())),
            socket
                .kernel_peer_addr()
                .and_then(|address| address.as_inet().map(|address| address.ip())),
        );
    }
    (None, None)
}

pub(super) fn strip_credentials(request: &mut RequestHeader) {
    for name in [
        "x-tako-network-authorization",
        "x-tako-internal-token",
        "x-tako-key-fingerprint",
        "x-tako-timestamp",
        "x-tako-nonce",
        "x-tako-signature",
    ] {
        request.remove_header(name);
    }
}

pub(super) async fn reject(session: &mut Session, status: u16) -> Result<bool> {
    let body = match status {
        403 => "Forbidden",
        429 => "Too Many Requests",
        _ => "Service Unavailable",
    };
    let mut header = ResponseHeader::build(status, None)?;
    insert_body_headers(&mut header, "text/plain", body)?;
    header.insert_header("Cache-Control", "no-store")?;
    session
        .write_response_header(Box::new(header), false)
        .await?;
    session.write_response_body(Some(body.into()), true).await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instances::{AppConfig, AppManager};
    use crate::lb::LoadBalancer;
    use crate::proxy::{CloudflareIpRanges, ProxyConfig};
    use crate::routing::RouteTable;
    use crate::scaling::{ColdStartConfig, ColdStartManager};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn proxy(data_dir: std::path::PathBuf) -> TakoProxy {
        let manager = Arc::new(AppManager::new(data_dir));
        let routes = Arc::new(parking_lot::RwLock::new(RouteTable::default()));
        routes
            .write()
            .set_app_routes("public".into(), vec!["*.tako.internal".into()]);
        TakoProxy::new(
            Arc::new(LoadBalancer::new(manager)),
            routes,
            ProxyConfig::default(),
            Arc::new(ColdStartManager::new(ColdStartConfig::default())),
            CloudflareIpRanges::default(),
        )
    }

    #[tokio::test]
    async fn private_host_and_forwarded_headers_cannot_reach_public_route() {
        let temp = tempfile::tempdir().unwrap();
        let proxy = proxy(temp.path().into());
        assert!(
            proxy
                .routes
                .read()
                .select_with_route("api.prod.net.tako.internal", "/")
                .is_some()
        );
        let (mut client, stream) = tokio::io::duplex(8192);
        client.write_all(b"GET / HTTP/1.1\r\nHost: api.prod.net.tako.internal\r\nX-Forwarded-For: fd00::1\r\nX-Forwarded-Proto: https\r\n\r\n").await.unwrap();
        let mut session = Session::new_h1(Box::new(stream));
        session.read_request().await.unwrap();
        let mut context = proxy.new_ctx();
        assert!(
            proxy
                .request_filter(&mut session, &mut context)
                .await
                .unwrap()
        );
        assert!(context.backend.is_none());
        drop(session);
        let mut response = String::new();
        client.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 403"));
        assert!(!response.contains("Location:"));
    }

    #[tokio::test]
    async fn drain_between_backend_selection_and_admission_releases_the_counter() {
        let temp = tempfile::tempdir().unwrap();
        let proxy = proxy(temp.path().into());
        let app = proxy.lb.app_manager().register_app(AppConfig {
            name: "api".into(),
            ..Default::default()
        });
        proxy.lb.register_app(app.clone());
        let instance = app.allocate_instance();
        app.set_instance_state(&instance, crate::socket::InstanceState::Healthy);
        let mut context = proxy.new_ctx();
        context.backend = proxy.lb.get_backend("api");
        assert!(context.backend.is_some());
        proxy
            .draining
            .store(true, std::sync::atomic::Ordering::Release);
        let (_, stream) = tokio::io::duplex(8192);
        let mut session = Session::new_h1(Box::new(stream));
        let mut request = RequestHeader::build("GET", b"/", None).unwrap();
        let error = proxy
            .upstream_request_filter(&mut session, &mut request, &mut context)
            .await
            .unwrap_err();
        assert!(matches!(error.etype(), ErrorType::HTTPStatus(503)));
        assert_eq!(instance.in_flight(), 0);
        assert!(!context.backend_request_started);
    }

    #[test]
    fn management_credentials_never_reach_application_upstreams() {
        let mut request = RequestHeader::build("GET", b"/", None).unwrap();
        request
            .insert_header("x-tako-network-authorization", "signed")
            .unwrap();
        request
            .insert_header("x-tako-internal-token", "private")
            .unwrap();
        request
            .insert_header("authorization", "Bearer app-token")
            .unwrap();
        strip_credentials(&mut request);
        assert!(!request.headers.contains_key("x-tako-network-authorization"));
        assert!(!request.headers.contains_key("x-tako-internal-token"));
        assert_eq!(request.headers["authorization"], "Bearer app-token");
    }
}
