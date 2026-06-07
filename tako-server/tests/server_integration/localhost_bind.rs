use super::*;

#[test]
fn ci_env_requires_failure_when_bind_is_unavailable() {
    assert!(should_fail_when_localhost_bind_unavailable(Some("true")));
    assert!(!should_fail_when_localhost_bind_unavailable(None));
    assert!(!should_fail_when_localhost_bind_unavailable(Some("  ")));
}
