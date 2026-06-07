use super::*;

#[test]
fn test_server_info_includes_pid() {
    if !require_localhost_bind() {
        return;
    }

    let server = TestServer::start();
    let response = server.send_command(&serde_json::json!({ "command": "server_info" }));
    assert_eq!(response.get("status").and_then(|s| s.as_str()), Some("ok"));

    let pid = response
        .get("data")
        .and_then(|d| d.get("pid"))
        .and_then(|p| p.as_u64())
        .expect("server_info response should include data.pid");

    // The PID should match the child process we spawned
    let child_pid = server.child.as_ref().unwrap().id();
    assert_eq!(pid, child_pid as u64);
}

#[test]
fn test_sighup_reload_replaces_process() {
    if !require_localhost_bind() {
        return;
    }

    let server = TestServer::start();

    // Read initial PID from server_info
    let response = server.send_command(&serde_json::json!({ "command": "server_info" }));
    let old_pid = response
        .get("data")
        .and_then(|d| d.get("pid"))
        .and_then(|p| p.as_u64())
        .expect("initial server_info should include pid") as u32;

    // Send SIGHUP to trigger zero-downtime reload
    let child_pid = server.child.as_ref().unwrap().id();
    assert_eq!(old_pid, child_pid);
    unsafe {
        libc::kill(child_pid as i32, libc::SIGHUP);
    }

    // Poll server_info until PID changes (new process takes over the socket)
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut new_pid = None;
    while std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(500));
        let resp = server.send_command(&serde_json::json!({ "command": "server_info" }));
        if let Some(pid) = resp
            .get("data")
            .and_then(|d| d.get("pid"))
            .and_then(|p| p.as_u64())
            && pid as u32 != old_pid
        {
            new_pid = Some(pid as u32);
            break;
        }
    }

    let new_pid = new_pid.expect("new server process should have a different PID after SIGHUP");
    assert_ne!(old_pid, new_pid);

    // Verify the new process responds to commands
    let list_response = server.send_command(&serde_json::json!({ "command": "list" }));
    assert_eq!(
        list_response.get("status").and_then(|s| s.as_str()),
        Some("ok"),
        "new process should respond to list command"
    );

    // Clean up the new child process (not tracked by TestServer)
    unsafe {
        libc::kill(new_pid as i32, libc::SIGTERM);
    }
}

#[test]
fn test_sigterm_exits_after_shutdown_drain() {
    if !require_localhost_bind() {
        return;
    }

    let mut server = TestServer::start();
    let pid = server
        .child
        .as_ref()
        .expect("server child should be tracked")
        .id();

    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }

    let child = server
        .child
        .as_mut()
        .expect("server child should still be tracked");
    let status = wait_for_child_exit(child, Duration::from_secs(15))
        .expect("server should exit promptly after SIGTERM shutdown drain");
    server.child = None;

    assert!(
        status.success(),
        "SIGTERM shutdown should exit cleanly, got {status}"
    );
}

#[test]
fn test_upgrade_mode_enter_exit() {
    if !require_localhost_bind() {
        return;
    }

    let server = TestServer::start();

    // Enter upgrading mode
    let resp = server.send_command(&serde_json::json!({
        "command": "enter_upgrading",
        "owner": "test-owner"
    }));
    assert_eq!(
        resp.get("status").and_then(|s| s.as_str()),
        Some("ok"),
        "enter_upgrading should succeed: {resp}"
    );

    // Verify server_info reflects upgrading mode
    let info = server.send_command(&serde_json::json!({ "command": "server_info" }));
    let mode = info
        .get("data")
        .and_then(|d| d.get("mode"))
        .and_then(|m| m.as_str())
        .expect("server_info should include mode");
    assert_eq!(mode, "upgrading", "mode should be upgrading: {info}");

    // Exit upgrading mode
    let resp = server.send_command(&serde_json::json!({
        "command": "exit_upgrading",
        "owner": "test-owner"
    }));
    assert_eq!(
        resp.get("status").and_then(|s| s.as_str()),
        Some("ok"),
        "exit_upgrading should succeed: {resp}"
    );

    // Verify server_info shows normal mode
    let info = server.send_command(&serde_json::json!({ "command": "server_info" }));
    let mode = info
        .get("data")
        .and_then(|d| d.get("mode"))
        .and_then(|m| m.as_str())
        .expect("server_info should include mode");
    assert_eq!(mode, "normal", "mode should be normal: {info}");
}

#[test]
fn test_upgrade_mode_rejects_concurrent_owners() {
    if !require_localhost_bind() {
        return;
    }

    let server = TestServer::start();

    // First owner enters upgrading
    let resp = server.send_command(&serde_json::json!({
        "command": "enter_upgrading",
        "owner": "owner-a"
    }));
    assert_eq!(resp.get("status").and_then(|s| s.as_str()), Some("ok"));

    // Second owner should be rejected
    let resp = server.send_command(&serde_json::json!({
        "command": "enter_upgrading",
        "owner": "owner-b"
    }));
    assert_eq!(
        resp.get("status").and_then(|s| s.as_str()),
        Some("error"),
        "concurrent enter_upgrading by different owner should fail: {resp}"
    );
    let msg = resp.get("message").and_then(|m| m.as_str()).unwrap_or("");
    assert!(
        msg.contains("already upgrading"),
        "error should mention already upgrading: {msg}"
    );

    // Clean up: first owner exits
    let resp = server.send_command(&serde_json::json!({
        "command": "exit_upgrading",
        "owner": "owner-a"
    }));
    assert_eq!(resp.get("status").and_then(|s| s.as_str()), Some("ok"));
}

/// Verify that a stuck upgrade lock is cleared after a process restart
/// (SIGHUP reload). Simulates Ctrl+C during upgrade: the old process dies
/// with upgrading mode + lock held, the new process should start clean.
#[test]
fn test_upgrade_lock_clears_after_reload() {
    if !require_localhost_bind() {
        return;
    }

    let server = TestServer::start();

    // Enter upgrading mode (sets mode + acquires lock).
    let resp = server.send_command(&serde_json::json!({
        "command": "enter_upgrading",
        "owner": "crashed-cli"
    }));
    assert_eq!(resp.get("status").and_then(|s| s.as_str()), Some("ok"));

    // Verify we're in upgrading mode.
    let info = server.send_command(&serde_json::json!({ "command": "server_info" }));
    let mode = info
        .get("data")
        .and_then(|d| d.get("mode"))
        .and_then(|m| m.as_str())
        .unwrap();
    assert_eq!(mode, "upgrading");

    // SIGHUP to simulate process restart (as would happen after crash + systemd restart).
    let old_pid = server.child.as_ref().unwrap().id();
    unsafe {
        libc::kill(old_pid as i32, libc::SIGHUP);
    }

    // Wait for new process to take over.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut new_pid = None;
    while std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(500));
        let resp = server.send_command(&serde_json::json!({ "command": "server_info" }));
        if let Some(pid) = resp
            .get("data")
            .and_then(|d| d.get("pid"))
            .and_then(|p| p.as_u64())
            && pid as u32 != old_pid
        {
            new_pid = Some(pid as u32);
            break;
        }
    }
    let new_pid = new_pid.expect("new server process should have a different PID after SIGHUP");

    // New process should be in normal mode (not stuck upgrading).
    let info = server.send_command(&serde_json::json!({ "command": "server_info" }));
    let mode = info
        .get("data")
        .and_then(|d| d.get("mode"))
        .and_then(|m| m.as_str())
        .unwrap();
    assert_eq!(
        mode, "normal",
        "server should reset to normal mode after restart"
    );

    // A new owner should be able to enter upgrading immediately
    // (no 10-minute stale wait).
    let resp = server.send_command(&serde_json::json!({
        "command": "enter_upgrading",
        "owner": "new-cli"
    }));
    assert_eq!(
        resp.get("status").and_then(|s| s.as_str()),
        Some("ok"),
        "new owner should acquire upgrade lock immediately after restart: {resp}"
    );

    // Clean up: exit upgrading and kill new process.
    let _ = server.send_command(&serde_json::json!({
        "command": "exit_upgrading",
        "owner": "new-cli"
    }));
    unsafe {
        libc::kill(new_pid as i32, libc::SIGTERM);
    }
}

/// Tighter deadline than test_sighup_reload_replaces_process (5s vs 30s)
/// to catch regressions from the early-bind fix.
#[test]
fn test_sighup_reload_swaps_socket_quickly() {
    if !require_localhost_bind() {
        return;
    }

    let server = TestServer::start();

    let response = server.send_command(&serde_json::json!({ "command": "server_info" }));
    let old_pid = response
        .get("data")
        .and_then(|d| d.get("pid"))
        .and_then(|p| p.as_u64())
        .expect("server_info should include pid") as u32;

    let child_pid = server.child.as_ref().unwrap().id();
    assert_eq!(old_pid, child_pid);
    unsafe {
        libc::kill(child_pid as i32, libc::SIGHUP);
    }

    // The new process should swap the socket within 5s thanks to early bind
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut new_pid = None;
    while std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(200));
        let resp = server.send_command(&serde_json::json!({ "command": "server_info" }));
        if let Some(pid) = resp
            .get("data")
            .and_then(|d| d.get("pid"))
            .and_then(|p| p.as_u64())
            && pid as u32 != old_pid
        {
            new_pid = Some(pid as u32);
            break;
        }
    }

    let new_pid = new_pid.expect("new process should take over socket within 5s after SIGHUP");
    assert_ne!(old_pid, new_pid);

    unsafe {
        libc::kill(new_pid as i32, libc::SIGTERM);
    }
}

#[test]
fn test_server_info_after_reload_preserves_config() {
    if !require_localhost_bind() {
        return;
    }

    let server = TestServer::start();

    // Capture pre-reload config
    let before = server.send_command(&serde_json::json!({ "command": "server_info" }));
    let before_data = before.get("data").expect("server_info should have data");
    let before_socket = before_data["socket"].as_str().unwrap().to_string();
    let before_http = before_data["http_port"].as_u64().unwrap();
    let before_https = before_data["https_port"].as_u64().unwrap();
    let old_pid = before_data["pid"].as_u64().unwrap() as u32;

    // Trigger reload
    unsafe {
        libc::kill(old_pid as i32, libc::SIGHUP);
    }

    // Wait for new process
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let mut after_data = None;
    while std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(300));
        let resp = server.send_command(&serde_json::json!({ "command": "server_info" }));
        if let Some(data) = resp.get("data")
            && data.get("pid").and_then(|p| p.as_u64()).unwrap_or(0) as u32 != old_pid
        {
            after_data = Some(data.clone());
            break;
        }
    }

    let after = after_data.expect("new process should respond after reload");

    // Socket path should be the same stable symlink
    assert_eq!(
        after["socket"].as_str().unwrap(),
        before_socket,
        "socket path should be preserved after reload"
    );
    assert_eq!(
        after["http_port"].as_u64().unwrap(),
        before_http,
        "http_port should be preserved"
    );
    assert_eq!(
        after["https_port"].as_u64().unwrap(),
        before_https,
        "https_port should be preserved"
    );

    // Clean up
    let new_pid = after["pid"].as_u64().unwrap() as u32;
    unsafe {
        libc::kill(new_pid as i32, libc::SIGTERM);
    }
}

#[test]
fn test_socket_available_during_reload() {
    if !require_localhost_bind() {
        return;
    }

    let server = TestServer::start();

    let response = server.send_command(&serde_json::json!({ "command": "server_info" }));
    let old_pid = response
        .get("data")
        .and_then(|d| d.get("pid"))
        .and_then(|p| p.as_u64())
        .expect("server_info should include pid") as u32;

    // Trigger reload
    unsafe {
        libc::kill(old_pid as i32, libc::SIGHUP);
    }

    // Poll server_info rapidly — every call should succeed (no connection-refused gap)
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut failures = 0;
    let mut total = 0;
    let mut saw_new_pid = false;
    while std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(100));
        total += 1;
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            server.send_command(&serde_json::json!({ "command": "server_info" }))
        })) {
            Ok(resp) => {
                if let Some(pid) = resp
                    .get("data")
                    .and_then(|d| d.get("pid"))
                    .and_then(|p| p.as_u64())
                    && pid as u32 != old_pid
                {
                    saw_new_pid = true;
                    break;
                }
            }
            Err(_) => {
                failures += 1;
            }
        }
    }

    assert!(saw_new_pid, "should have seen new pid within 10s");
    assert_eq!(
        failures, 0,
        "socket should remain available during reload ({failures}/{total} calls failed)"
    );

    // Clean up new process
    let resp = server.send_command(&serde_json::json!({ "command": "server_info" }));
    if let Some(pid) = resp
        .get("data")
        .and_then(|d| d.get("pid"))
        .and_then(|p| p.as_u64())
        && pid as u32 != old_pid
    {
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
    }
}
