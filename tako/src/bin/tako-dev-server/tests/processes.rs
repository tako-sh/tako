use super::*;

#[test]
fn kill_all_app_processes_sends_sigterm_to_tracked_pids() {
    let (state, _tmp) = test_state();

    // Spawn a long-lived process we can check.
    let mut child = std::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .unwrap();
    let pid = child.id();

    // Register it in memory with a PID.
    {
        let mut s = state.lock().unwrap();
        s.apps.insert(
            "/proj/tako.toml".to_string(),
            state::RuntimeApp {
                project_dir: "/proj".to_string(),
                name: "my-app".to_string(),
                variant: None,
                hosts: vec!["my-app.test".to_string()],
                upstream_port: 3000,
                is_idle: false,
                command: vec!["sleep".to_string(), "60".to_string()],
                env: std::collections::HashMap::new(),
                log_buffer: state::LogBuffer::new(),
                pid: Some(pid),
                client_pid: None,
                tunnel: None,
                readiness_failure_hint: None,
                bootstrap_token: "dev-token".to_string(),
                secrets: std::collections::HashMap::new(),
                storages: std::collections::HashMap::new(),
            },
        );
    }

    // Verify the process is alive.
    assert_eq!(unsafe { libc::kill(pid as i32, 0) }, 0);

    kill_all_app_processes(&state);

    // wait() will return once the process has been terminated by SIGTERM.
    let status = child.wait().unwrap();
    assert!(!status.success());
}

// -----------------------------------------------------------------------
// Variant support
// -----------------------------------------------------------------------
