use super::*;

#[test]
fn redirect_location_strips_default_http_port() {
    let location = redirect_location("bun-example.test:80", "/hello");
    assert_eq!(location, "https://bun-example.test/hello");
}

#[test]
fn redirect_location_keeps_non_default_port() {
    let location = redirect_location("bun-example.test:8080", "/");
    assert_eq!(location, "https://bun-example.test:8080/");
}

#[test]
fn ensure_tcp_listener_can_bind_succeeds_when_port_is_available() {
    // On busy CI hosts, another process can race us for a just-freed port.
    // Retry a few times with fresh ephemeral ports to keep this deterministic.
    for _ in 0..8 {
        let Ok(listener) = std::net::TcpListener::bind(("127.0.0.1", 0)) else {
            return;
        };
        let addr = listener.local_addr().unwrap();
        drop(listener);
        if ensure_tcp_listener_can_bind(&addr.to_string()).is_ok() {
            return;
        }
    }
    panic!("failed to find an available loopback port after retries");
}

#[test]
fn ensure_tcp_listener_can_bind_reports_error_when_port_in_use() {
    let Ok(listener) = std::net::TcpListener::bind(("127.0.0.1", 0)) else {
        return;
    };
    let addr = listener.local_addr().unwrap();
    let err = ensure_tcp_listener_can_bind(&addr.to_string())
        .unwrap_err()
        .to_string();
    assert!(err.contains("dev proxy could not bind on"));
    assert!(err.contains(&addr.to_string()));
    drop(listener);
}
