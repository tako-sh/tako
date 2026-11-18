//! Route validation for Tako configuration
//!
//! Route Validation Rules (all environments):
//! - Path-only routes are INVALID (e.g., `/api/*`) - must include hostname
//! - Routes must include hostname (exact or wildcard): `api.example.com`, `*.api.example.com`
//! - Optional path suffix allowed: `api.example.com/admin/*`
//!
//! Development-Specific Rules (`[envs.development]`):
//! - Routes are optional in config; if omitted, `tako dev` defaults to `{app-name}.test`
//! - `.test` and `.tako.test` hostnames are managed by Tako's local DNS
//! - External hostnames are accepted as additional routes, but users must point them at Tako
//! - Examples valid: `"my-app.test"`, `"*.my-app.test"`, `"my-app.tako.test"`, `"local.example.com"`
//! - Examples invalid: `"/api/*"` (path-only)

use tako_core::routes::split_route;
pub use tako_core::routes::{route_matches, routes_overlap};
use thiserror::Error;

/// Errors that can occur during route validation
#[derive(Debug, Error)]
pub enum RouteValidationError {
    #[error(
        "Path-only routes are not allowed: '{0}'. Routes must include a hostname (e.g., 'api.example.com/path')"
    )]
    PathOnlyRoute(String),

    #[error("Invalid route pattern: '{0}'. {1}")]
    InvalidPattern(String, String),

    #[error("Empty route is not allowed")]
    EmptyRoute,

    #[error("Invalid hostname in route: '{0}'")]
    InvalidHostname(String),
}

/// Result type for route validation
pub type RouteResult<T> = Result<T, RouteValidationError>;

/// Validates a route pattern for any environment
///
/// Rules:
/// - Must not be empty
/// - Must not start with `/` (path-only)
/// - Must have a valid hostname
pub fn validate_route(route: &str) -> RouteResult<()> {
    if route.is_empty() {
        return Err(RouteValidationError::EmptyRoute);
    }

    // Check for path-only routes (starts with /)
    if route.starts_with('/') {
        return Err(RouteValidationError::PathOnlyRoute(route.to_string()));
    }

    // Parse hostname and optional path
    let (hostname, _path) = split_route(route);

    // Validate hostname
    validate_hostname(hostname)?;

    Ok(())
}

/// Validates a route pattern for development environment.
///
/// Development routes use the same pattern validation as deployed routes.
/// Tako only manages DNS for `.test` / `.tako.test`; external hostnames are
/// accepted for callers that route traffic to the dev proxy themselves.
pub fn validate_dev_route(route: &str, _app_name: &str) -> RouteResult<()> {
    validate_route(route)
}

/// Generates the default development route for an app
pub fn default_dev_route(app_name: &str) -> String {
    format!("{}.{}", app_name, crate::dev::SHORT_DEV_DOMAIN)
}

/// Validates a hostname (with optional wildcard prefix)
fn validate_hostname(hostname: &str) -> RouteResult<()> {
    if hostname.is_empty() {
        return Err(RouteValidationError::InvalidHostname(hostname.to_string()));
    }

    // Handle wildcard prefix
    let hostname = hostname.strip_prefix("*.").unwrap_or(hostname);

    // Check for empty after stripping wildcard
    if hostname.is_empty() {
        return Err(RouteValidationError::InvalidHostname("*".to_string()));
    }

    // Basic hostname validation
    // Must contain at least one dot (TLD required)
    if !hostname.contains('.') {
        return Err(RouteValidationError::InvalidPattern(
            hostname.to_string(),
            "Hostname must include a TLD (e.g., 'example.com' not 'example')".to_string(),
        ));
    }

    // Validate each label
    for label in hostname.split('.') {
        if label.is_empty() {
            return Err(RouteValidationError::InvalidHostname(hostname.to_string()));
        }

        // Labels must start with alphanumeric
        if !label
            .chars()
            .next()
            .map(|c| c.is_alphanumeric())
            .unwrap_or(false)
        {
            return Err(RouteValidationError::InvalidHostname(hostname.to_string()));
        }

        // Labels can only contain alphanumeric and hyphens
        if !label.chars().all(|c| c.is_alphanumeric() || c == '-') {
            return Err(RouteValidationError::InvalidHostname(hostname.to_string()));
        }

        // Labels cannot end with hyphen
        if label.ends_with('-') {
            return Err(RouteValidationError::InvalidHostname(hostname.to_string()));
        }

        // Labels have max length of 63
        if label.len() > 63 {
            return Err(RouteValidationError::InvalidPattern(
                hostname.to_string(),
                "Label exceeds 63 character limit".to_string(),
            ));
        }
    }

    Ok(())
}

/// Validate that a new app's routes don't conflict with existing deployed apps.
///
/// `existing` is a list of (app_name, routes). Empty lists describe private groups;
/// native application configuration validates route presence separately.
pub fn validate_no_route_conflicts(
    _existing: &[(String, Vec<String>)],
    _new_app: &str,
    _new_routes: &[String],
) -> Result<(), String> {
    for (existing_app, existing_routes) in _existing {
        if existing_app == _new_app {
            continue;
        }

        for er in existing_routes {
            for nr in _new_routes {
                if routes_overlap(er, nr) {
                    return Err(format!(
                        "route conflict: '{}' route '{}' overlaps '{}' route '{}'",
                        existing_app, er, _new_app, nr
                    ));
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_route_rejects_empty_and_path_only() {
        let empty = validate_route("").unwrap_err();
        assert!(matches!(empty, RouteValidationError::EmptyRoute));

        let path_only = validate_route("/api/*").unwrap_err();
        assert!(matches!(path_only, RouteValidationError::PathOnlyRoute(_)));
    }

    #[test]
    fn test_validate_route_rejects_hostname_without_tld() {
        let err = validate_route("localhost").unwrap_err();
        assert!(matches!(err, RouteValidationError::InvalidPattern(_, _)));
    }

    #[test]
    fn test_validate_route_accepts_wildcard_and_path() {
        validate_route("*.example.com/api/*").unwrap();
    }

    #[test]
    fn test_validate_dev_route_accepts_external_hostname() {
        validate_dev_route("tunnel.example.com", "my-app").unwrap();
        validate_dev_route("tunnel.example.com/api/*", "my-app").unwrap();
    }

    #[test]
    fn test_validate_dev_route_accepts_any_tako_hostname() {
        // Any .tako.test hostname is allowed, not just app-name.tako.test.
        validate_dev_route("my-app.tako.test", "my-app").unwrap();
        validate_dev_route("other.tako.test", "my-app").unwrap();
        validate_dev_route("shared.tako.test/api", "my-app").unwrap();
        validate_dev_route("*.my-app.tako.test", "my-app").unwrap();
        validate_dev_route("api.my-app.tako.test", "my-app").unwrap();
    }

    #[test]
    fn test_validate_dev_route_accepts_short_test_domain() {
        validate_dev_route("my-app.test", "my-app").unwrap();
        validate_dev_route("other.test", "my-app").unwrap();
        validate_dev_route("shared.test/api", "my-app").unwrap();
        validate_dev_route("*.my-app.test", "my-app").unwrap();
        validate_dev_route("api.my-app.test", "my-app").unwrap();
    }

    #[test]
    fn test_default_dev_route_uses_app_name() {
        assert_eq!(default_dev_route("dashboard"), "dashboard.test");
    }

    #[test]
    fn test_route_matches_handles_exact_and_path_wildcards() {
        assert!(route_matches("api.example.com", "api.example.com", "/"));
        assert!(!route_matches("api.example.com", "www.example.com", "/"));
        assert!(route_matches(
            "api.example.com/v1",
            "api.example.com",
            "/v1/"
        ));
        assert!(route_matches(
            "api.example.com/v1/",
            "api.example.com",
            "/v1"
        ));

        assert!(route_matches(
            "api.example.com/v1/*",
            "api.example.com",
            "/v1"
        ));
        assert!(route_matches(
            "api.example.com/v1/*",
            "api.example.com",
            "/v1/users"
        ));
        assert!(!route_matches(
            "api.example.com/v1/*",
            "api.example.com",
            "/v2/users"
        ));
    }

    #[test]
    fn test_route_matches_handles_prefix_star_patterns() {
        assert!(route_matches(
            "api.example.com/v1*",
            "api.example.com",
            "/v1"
        ));
        assert!(route_matches(
            "api.example.com/v1*",
            "api.example.com",
            "/v1alpha"
        ));
        assert!(!route_matches(
            "api.example.com/v1*",
            "api.example.com",
            "/v2"
        ));
    }

    #[test]
    fn test_routes_overlap_exact_same_host() {
        assert!(routes_overlap("api.example.com", "api.example.com"));
        assert!(!routes_overlap("api.example.com", "www.example.com"));
    }

    #[test]
    fn test_routes_overlap_wildcard_and_exact() {
        assert!(routes_overlap("*.example.com", "api.example.com"));
        assert!(!routes_overlap("*.example.com", "example.com"));
    }

    #[test]
    fn test_routes_overlap_nested_wildcard_suffixes() {
        assert!(routes_overlap("*.example.com", "*.api.example.com"));
        assert!(routes_overlap("*.api.example.com", "*.example.com"));
    }

    #[test]
    fn test_routes_overlap_path_specificity() {
        assert!(routes_overlap("example.com/api/*", "example.com/api/v1/*"));
        assert!(!routes_overlap("example.com/api/*", "example.com/admin/*"));
        assert!(routes_overlap("example.com/api", "example.com/api/"));
    }

    #[test]
    fn test_validate_no_route_conflicts_allows_existing_private_group() {
        let existing = vec![("other".to_string(), vec![])];
        assert!(
            validate_no_route_conflicts(&existing, "new", &["api.example.com".to_string()]).is_ok()
        );
    }

    #[test]
    fn test_validate_no_route_conflicts_allows_private_group() {
        let existing = vec![("other".to_string(), vec![])];
        assert!(validate_no_route_conflicts(&existing, "new", &[]).is_ok());
    }

    #[test]
    fn test_validate_no_route_conflicts_overlapping_routes_conflict() {
        let existing = vec![("other".to_string(), vec!["*.example.com".to_string()])];
        let err = validate_no_route_conflicts(&existing, "new", &["api.example.com".to_string()])
            .unwrap_err();
        assert!(err.contains("conflict"));
    }

    #[test]
    fn test_validate_no_route_conflicts_same_app_update_allowed() {
        let existing = vec![("my-app".to_string(), vec!["api.example.com".to_string()])];
        validate_no_route_conflicts(&existing, "my-app", &["api.example.com".to_string()]).unwrap();
    }

    #[test]
    fn test_validate_no_route_conflicts_non_overlapping_routes_allowed() {
        let existing = vec![
            ("a".to_string(), vec!["a.example.com".to_string()]),
            ("b".to_string(), vec!["b.example.com/admin/*".to_string()]),
        ];
        validate_no_route_conflicts(&existing, "new", &["new.example.com".to_string()]).unwrap();
    }
}
