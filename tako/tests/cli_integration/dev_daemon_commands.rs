use crate::support::*;

#[test]
fn dev_doctor_prints_info() {
    let _guard = dev_daemon_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().to_path_buf();
    setup_minimal_bun_project(&project_dir);
    let home = temp.path().join("home");
    let tako_home = temp.path().join("tako-home");
    fs::create_dir_all(&home).unwrap();

    let Some(_fake) = FakeDevServer::start(&tako_home) else {
        return;
    };

    let out = run_tako_with_env(&["doctor"], &project_dir, &home, &tako_home);
    assert!(out.status.success(), "doctor failed: {}", stderr_str(&out));
    let combined = format!("{}{}", stdout_str(&out), stderr_str(&out));
    assert!(
        combined.contains("Development server"),
        "unexpected doctor output: {}",
        combined
    );
    assert!(
        combined.contains("Listen"),
        "expected Listen row: {}",
        combined
    );
    assert!(
        combined.contains("Apps"),
        "expected Apps section: {}",
        combined
    );
}
