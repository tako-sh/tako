use super::*;

#[test]
fn build_spawn_env_injects_tako_runtime_contract_when_socket_available() {
    // Regression: dev used to spawn apps without TAKO_INTERNAL_SOCKET /
    // TAKO_APP_NAME, so workflow `.enqueue()` blew up only when a user
    // clicked a button. Both must be present whenever the dev-server has
    // a live internal socket.
    let app = runtime_app_with_env("demo", std::collections::HashMap::new());
    let sock = std::path::PathBuf::from("/tmp/tako.sock");

    let env = build_spawn_env(&app, Some(&sock));

    assert_eq!(env.get("TAKO_APP_NAME").map(String::as_str), Some("demo"));
    assert_eq!(
        env.get("TAKO_INTERNAL_SOCKET").map(String::as_str),
        Some("/tmp/tako.sock"),
    );
    assert_eq!(env.get("PORT").map(String::as_str), Some("0"));
    assert_eq!(env.get("HOST").map(String::as_str), Some("127.0.0.1"));
}

#[test]
fn build_spawn_env_omits_socket_but_still_sets_app_name_when_socket_missing() {
    // start_socket can fail (permissions, etc). In that case TAKO_APP_NAME
    // is still informative; TAKO_INTERNAL_SOCKET stays unset so the SDK's
    // fail-early check pairs cleanly.
    let app = runtime_app_with_env("demo", std::collections::HashMap::new());

    let env = build_spawn_env(&app, None);

    assert_eq!(env.get("TAKO_APP_NAME").map(String::as_str), Some("demo"));
    assert!(!env.contains_key("TAKO_INTERNAL_SOCKET"));
}

#[test]
fn build_spawn_env_contract_wins_over_user_env() {
    // User-supplied env must never shadow Tako's wiring. A stray
    // `HOST=0.0.0.0` would make the app unreachable via the proxy; a
    // stray `TAKO_APP_NAME=impostor` would mis-route every RPC.
    let mut user_env = std::collections::HashMap::new();
    user_env.insert("HOST".to_string(), "0.0.0.0".to_string());
    user_env.insert("TAKO_APP_NAME".to_string(), "impostor".to_string());
    user_env.insert(
        "TAKO_INTERNAL_SOCKET".to_string(),
        "/tmp/wrong.sock".to_string(),
    );
    user_env.insert("FOO".to_string(), "bar".to_string());
    let app = runtime_app_with_env("demo", user_env);
    let sock = std::path::PathBuf::from("/tmp/tako.sock");

    let env = build_spawn_env(&app, Some(&sock));

    assert_eq!(env.get("HOST").map(String::as_str), Some("127.0.0.1"));
    assert_eq!(env.get("TAKO_APP_NAME").map(String::as_str), Some("demo"));
    assert_eq!(
        env.get("TAKO_INTERNAL_SOCKET").map(String::as_str),
        Some("/tmp/tako.sock"),
    );
    // Unrelated user env passes through untouched.
    assert_eq!(env.get("FOO").map(String::as_str), Some("bar"));
}
