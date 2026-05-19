use super::super::dns::production_route_needs_dns;

#[test]
fn init_offers_dns_setup_for_wildcard_production_route() {
    assert!(production_route_needs_dns("*.demo-app.example.com"));
    assert!(production_route_needs_dns("*.demo-app.example.com/api"));
}

#[test]
fn init_skips_dns_setup_for_exact_production_route() {
    assert!(!production_route_needs_dns("demo-app.example.com"));
    assert!(!production_route_needs_dns("demo-app.example.com/api"));
}
