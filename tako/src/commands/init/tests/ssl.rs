use super::super::ssl::production_route_needs_wildcard_ssl;

#[test]
fn init_offers_ssl_setup_for_wildcard_production_route() {
    assert!(production_route_needs_wildcard_ssl(
        "*.demo-app.example.com"
    ));
    assert!(production_route_needs_wildcard_ssl(
        "*.demo-app.example.com/api"
    ));
}

#[test]
fn init_skips_ssl_setup_for_exact_production_route() {
    assert!(!production_route_needs_wildcard_ssl("demo-app.example.com"));
    assert!(!production_route_needs_wildcard_ssl(
        "demo-app.example.com/api"
    ));
}
