use super::build_worker_env;

#[test]
fn build_worker_env_sets_data_dir_under_project() {
    let env = build_worker_env(
        "demo",
        std::path::Path::new("/tmp/myproj"),
        std::path::Path::new("/tmp/internal.sock"),
        Some("src"),
    );
    assert_eq!(env.get("TAKO_APP_NAME").map(String::as_str), Some("demo"));
    assert_eq!(
        env.get("TAKO_INTERNAL_SOCKET").map(String::as_str),
        Some("/tmp/internal.sock"),
    );
    assert_eq!(
        env.get("TAKO_DATA_DIR").map(String::as_str),
        Some("/tmp/myproj/.tako/data/app"),
    );
    assert_eq!(env.get("TAKO_APP_ROOT").map(String::as_str), Some("src"));
}
