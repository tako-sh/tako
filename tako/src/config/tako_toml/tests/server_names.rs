use super::super::validation::validate_server_name;

// ==================== Server Name Validation Tests ====================

#[test]
fn test_validate_server_name_valid() {
    assert!(validate_server_name("la").is_ok());
    assert!(validate_server_name("prod-server").is_ok());
    assert!(validate_server_name("server1").is_ok());
    assert!(validate_server_name("my-prod-server-1").is_ok());
}

#[test]
fn test_validate_server_name_empty() {
    assert!(validate_server_name("").is_err());
}

#[test]
fn test_validate_server_name_too_long() {
    let long_name = "a".repeat(64);
    assert!(validate_server_name(&long_name).is_err());
}

#[test]
fn test_validate_server_name_invalid_start() {
    assert!(validate_server_name("1server").is_err());
    assert!(validate_server_name("-server").is_err());
    assert!(validate_server_name("Server").is_err());
}

#[test]
fn test_validate_server_name_invalid_chars() {
    assert!(validate_server_name("my_server").is_err());
    assert!(validate_server_name("my.server").is_err());
    assert!(validate_server_name("MY-SERVER").is_err());
}
