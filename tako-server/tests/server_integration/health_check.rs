use super::*;

#[test]
fn test_http_redirects_to_https() {
    if !require_localhost_bind() {
        return;
    }

    let server = TestServer::start();
    let response = server
        .http_get("/")
        .expect("root endpoint request should succeed");

    assert!(
        response.starts_with("HTTP/1.1 307") || response.starts_with("HTTP/1.0 307"),
        "expected 307 response: {response}"
    );
    assert!(
        response.contains(&format!("Location: https://localhost:{}/", server.tls_port)),
        "expected https location header: {response}"
    );
    assert!(
        response.contains("Cache-Control: no-store"),
        "expected no-store cache control on redirect: {response}"
    );
}

#[test]
fn test_app_scoped_internal_status_host_is_not_exposed_by_proxy() {
    if !require_localhost_bind() {
        return;
    }

    let server = TestServer::start();

    let response = server
        .http_get_with_host_and_headers(
            "test-app.tako",
            "/status",
            &[("X-Forwarded-Proto", "https")],
        )
        .expect("status endpoint request should succeed");

    assert!(
        response.starts_with("HTTP/1.1 404") || response.starts_with("HTTP/1.0 404"),
        "expected 404 response: {response}"
    );
}

#[test]
fn test_orbstack_host_does_not_redirect_when_proto_header_missing() {
    if !require_localhost_bind() {
        return;
    }

    let server = TestServer::start();

    let response = server
        .http_get_with_host_and_headers(
            "test-app.orb.local",
            "/",
            &[("X-Forwarded-For", "127.0.0.1")],
        )
        .expect("orb.local request should succeed");

    assert!(
        response.starts_with("HTTP/1.1 404") || response.starts_with("HTTP/1.0 404"),
        "expected 404 response without redirect loop: {response}"
    );
    assert!(
        !response.contains("Location: https://"),
        "did not expect https redirect for orb.local forwarded request: {response}"
    );
}

#[test]
fn test_unknown_private_https_host_returns_404_instead_of_tls_handshake_failure() {
    if !require_localhost_bind() {
        return;
    }

    let server = TestServer::start();
    let status = server
        .https_status_with_host("tako-testbed.orb.local", "/404")
        .expect("expected HTTPS request to complete");
    assert_eq!(status, 404);
}
