use super::*;

fn temp_store() -> (tempfile::TempDir, DevStateStore) {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = DevStateStore::open(tmp.path().join("dev-server.sqlite")).unwrap();
    (tmp, store)
}

#[test]
fn open_creates_db_and_schema() {
    let (_tmp, store) = temp_store();
    assert!(store.list().unwrap().is_empty());

    let conn = Connection::open(store.conn.path().unwrap()).unwrap();
    let columns: Vec<String> = conn
        .prepare("PRAGMA table_info(apps);")
        .unwrap()
        .query_map([], |row| row.get(1))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        columns,
        vec![
            "config_path".to_string(),
            "project_dir".to_string(),
            "name".to_string(),
            "variant".to_string(),
            "is_enabled".to_string(),
            "created_at".to_string(),
            "updated_at".to_string(),
        ]
    );
}

#[test]
fn register_and_get() {
    let (_tmp, store) = temp_store();
    store
        .register(
            "/home/user/my-app/tako.toml",
            "/home/user/my-app",
            "my-app",
            None,
        )
        .unwrap();

    let app = store.get("/home/user/my-app/tako.toml").unwrap().unwrap();
    assert_eq!(app.config_path, "/home/user/my-app/tako.toml");
    assert_eq!(app.project_dir, "/home/user/my-app");
    assert_eq!(app.name, "my-app");
    assert!(app.variant.is_none());
    assert!(app.is_enabled);
    assert!(app.created_at > 0);
    assert_eq!(app.created_at, app.updated_at);
}

#[test]
fn register_with_variant() {
    let (_tmp, store) = temp_store();
    store
        .register(
            "/home/user/my-app/tako.toml",
            "/home/user/my-app",
            "my-app",
            Some("staging"),
        )
        .unwrap();

    let app = store.get("/home/user/my-app/tako.toml").unwrap().unwrap();
    assert_eq!(app.name, "my-app");
    assert_eq!(app.variant.as_deref(), Some("staging"));
}

#[test]
fn register_upserts_name_and_updates_timestamp() {
    let (_tmp, store) = temp_store();
    store
        .register("/proj/tako.toml", "/proj", "old-name", None)
        .unwrap();
    let first = store.get("/proj/tako.toml").unwrap().unwrap();

    store
        .register("/proj/tako.toml", "/proj", "new-name", None)
        .unwrap();
    let second = store.get("/proj/tako.toml").unwrap().unwrap();

    assert_eq!(second.name, "new-name");
    assert_eq!(second.created_at, first.created_at);
    assert!(second.updated_at >= first.updated_at);
}

#[test]
fn set_enabled_toggle() {
    let (_tmp, store) = temp_store();
    store
        .register("/proj/tako.toml", "/proj", "app", None)
        .unwrap();

    assert!(store.set_enabled("/proj/tako.toml", false).unwrap());
    assert!(!store.get("/proj/tako.toml").unwrap().unwrap().is_enabled);

    assert!(store.set_enabled("/proj/tako.toml", true).unwrap());
    assert!(store.get("/proj/tako.toml").unwrap().unwrap().is_enabled);

    assert!(!store.set_enabled("/nonexistent/tako.toml", false).unwrap());
}

#[test]
fn unregister_app() {
    let (_tmp, store) = temp_store();
    store
        .register("/proj/tako.toml", "/proj", "app", None)
        .unwrap();

    assert!(store.unregister("/proj/tako.toml").unwrap());
    assert!(store.get("/proj/tako.toml").unwrap().is_none());
    assert!(!store.unregister("/proj/tako.toml").unwrap());
}

#[test]
fn cleanup_stale_removes_apps_without_tako_toml() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = DevStateStore::open(tmp.path().join("db")).unwrap();

    let real_proj = tmp.path().join("real-proj");
    std::fs::create_dir_all(&real_proj).unwrap();
    let real_config = real_proj.join("preview.toml");
    std::fs::write(&real_config, "name = \"real\"").unwrap();

    store
        .register(
            real_config.to_str().unwrap(),
            real_proj.to_str().unwrap(),
            "real",
            None,
        )
        .unwrap();
    store
        .register(
            "/nonexistent/proj/preview.toml",
            "/nonexistent/proj",
            "stale",
            None,
        )
        .unwrap();

    let removed = store.cleanup_stale().unwrap();
    assert_eq!(removed, vec!["/nonexistent/proj/preview.toml"]);

    let apps = store.list().unwrap();
    assert_eq!(apps.len(), 1);
    assert_eq!(apps[0].name, "real");
}

#[test]
fn pid_file_write_read_remove() {
    let tmp = tempfile::TempDir::new().unwrap();
    let project_dir = tmp.path().to_str().unwrap();
    let config_path = "/tmp/example/tako.toml";

    assert!(read_pid_file(project_dir, config_path).is_none());

    write_pid_file(project_dir, config_path, 12345);
    assert_eq!(read_pid_file(project_dir, config_path), Some(12345));

    remove_pid_file(project_dir, config_path);
    assert!(read_pid_file(project_dir, config_path).is_none());
}

#[test]
fn kill_orphaned_process_cleans_up_stale_pid_file() {
    let tmp = tempfile::TempDir::new().unwrap();
    let project_dir = tmp.path().to_str().unwrap();
    let config_path = "/tmp/example/tako.toml";

    // Write a PID file with a definitely-dead PID.
    write_pid_file(project_dir, config_path, 999_999_999);
    kill_orphaned_process(project_dir, config_path);
    assert!(read_pid_file(project_dir, config_path).is_none());
}

#[test]
fn kill_orphaned_process_kills_live_process() {
    let tmp = tempfile::TempDir::new().unwrap();
    let project_dir = tmp.path().to_str().unwrap();
    let config_path = "/tmp/example/tako.toml";

    let mut child = std::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .unwrap();
    let pid = child.id();
    write_pid_file(project_dir, config_path, pid);

    kill_orphaned_process(project_dir, config_path);

    let status = child.wait().unwrap();
    assert!(!status.success());
    assert!(read_pid_file(project_dir, config_path).is_none());
}

#[test]
fn pid_files_are_scoped_by_config_path() {
    let tmp = tempfile::TempDir::new().unwrap();
    let project_dir = tmp.path().to_str().unwrap();

    write_pid_file(project_dir, "/tmp/example/one.toml", 111);
    write_pid_file(project_dir, "/tmp/example/two.toml", 222);

    assert_eq!(
        read_pid_file(project_dir, "/tmp/example/one.toml"),
        Some(111)
    );
    assert_eq!(
        read_pid_file(project_dir, "/tmp/example/two.toml"),
        Some(222)
    );
}

// -----------------------------------------------------------------------
// LogBuffer tests
// -----------------------------------------------------------------------

#[test]
fn log_buffer_push_and_subscribe_returns_backlog() {
    let buf = LogBuffer::new();
    buf.push("line-0".to_string());
    buf.push("line-1".to_string());
    buf.push("line-2".to_string());

    let (backlog, _rx, truncated) = buf.subscribe(None);
    assert!(!truncated);
    assert_eq!(backlog.len(), 3);
    assert_eq!(backlog[0].id, 0);
    assert_eq!(backlog[0].line, "line-0");
    assert_eq!(backlog[2].id, 2);
    assert_eq!(backlog[2].line, "line-2");
}

#[test]
fn log_buffer_subscribe_after_returns_entries_after_id() {
    let buf = LogBuffer::new();
    for i in 0..5 {
        buf.push(format!("line-{i}"));
    }

    let (backlog, _rx, truncated) = buf.subscribe(Some(2));
    assert!(!truncated);
    assert_eq!(backlog.len(), 2);
    assert_eq!(backlog[0].id, 3);
    assert_eq!(backlog[1].id, 4);
}

#[test]
fn log_buffer_capacity_drops_oldest() {
    let buf = LogBuffer::new();
    // Push more than capacity (500).
    for i in 0..510 {
        buf.push(format!("line-{i}"));
    }

    let (backlog, _rx, _) = buf.subscribe(None);
    assert_eq!(backlog.len(), 500);
    // Oldest should be id=10 (first 10 were dropped).
    assert_eq!(backlog[0].id, 10);
    assert_eq!(backlog[0].line, "line-10");
}

#[test]
fn log_buffer_truncated_flag_when_after_is_before_oldest() {
    let buf = LogBuffer::new();
    for i in 0..510 {
        buf.push(format!("line-{i}"));
    }

    // Request after=5, but oldest is 10 — truncated.
    let (_backlog, _rx, truncated) = buf.subscribe(Some(5));
    assert!(truncated);

    // Request after=10, oldest is 10 — not truncated.
    let (_backlog, _rx, truncated) = buf.subscribe(Some(10));
    assert!(!truncated);
}

#[test]
fn log_buffer_clear_preserves_id_counter() {
    let buf = LogBuffer::new();
    buf.push("before".to_string());
    buf.clear();
    buf.push("after".to_string());

    let (backlog, _rx, _) = buf.subscribe(None);
    assert_eq!(backlog.len(), 1);
    // ID counter is preserved across clear (was 1 after "before", now 1 for "after").
    assert_eq!(backlog[0].id, 1);
    assert_eq!(backlog[0].line, "after");
}

#[tokio::test]
async fn log_buffer_subscriber_receives_live_entries() {
    let buf = LogBuffer::new();
    let (_backlog, mut rx, _) = buf.subscribe(None);

    buf.push("live-1".to_string());
    buf.push("live-2".to_string());

    let entry = rx.recv().await.unwrap();
    assert_eq!(entry.id, 0);
    assert_eq!(entry.line, "live-1");

    let entry = rx.recv().await.unwrap();
    assert_eq!(entry.id, 1);
    assert_eq!(entry.line, "live-2");
}

#[tokio::test]
async fn log_buffer_dead_subscriber_is_cleaned_up() {
    let buf = LogBuffer::new();
    let (_backlog, rx, _) = buf.subscribe(None);
    drop(rx); // Subscriber disconnects.

    // Pushing should not panic; the dead subscriber is cleaned up.
    buf.push("after-drop".to_string());

    // Verify the entry is still in the buffer.
    let (backlog, _rx2, _) = buf.subscribe(None);
    assert_eq!(backlog.len(), 1);
    assert_eq!(backlog[0].line, "after-drop");
}
