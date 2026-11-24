use crate::support::*;

#[test]
fn upgrade_stops_daemon_only_after_installing_a_new_release() {
    use std::os::unix::fs::PermissionsExt;
    let temp = TempDir::new().unwrap();
    let home = temp.path();
    let sock = home.join("dev-server.sock");
    let _daemon = FakeDevServer::start(home).unwrap();
    let brew = home.join("brew");
    fs::write(
        &brew,
        r#"#!/bin/sh
case "$1" in
    --version) echo Homebrew ;;
    list) read -r version < version; echo "tako $version" ;;
    upgrade) [ "$RELEASE" != fail ] && echo "$RELEASE" > version ;;
    *) exit 1 ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(brew, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(home.join("version"), "1\n").unwrap();
    let upgrade = |release| {
        let path = home.to_str().unwrap();
        run_tako_with_extra_env(
            &["upgrade", "--ci", "--json"],
            home,
            &[
                ("HOME", path),
                ("TAKO_HOME", path),
                ("PATH", path),
                ("RELEASE", release),
            ],
        )
    };

    assert!(!upgrade("fail").status.success());
    assert!(sock.exists());
    assert!(upgrade("1").status.success());
    assert!(sock.exists());
    let out = upgrade("2");
    assert!(out.status.success(), "{}", stderr_str(&out));
    assert!(!sock.exists(), "upgrade returned before daemon shutdown");
    assert!(upgrade("3").status.success(), "an absent daemon is a no-op");
    drop(std::os::unix::net::UnixListener::bind(&sock).unwrap());
    assert!(sock.exists(), "fixture should leave a stale socket");
    let out = upgrade("4");
    assert!(out.status.success(), "stale socket: {}", stderr_str(&out));
}

#[test]
fn dev_stop_all_attempts_remaining_apps_and_reports_every_failure() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    let tako_home = temp.path().join("tako-home");
    fs::create_dir_all(&home).unwrap();
    let attempts = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = attempts.clone();
    let _fake = FakeDevServer::start_with_handler(&tako_home, move |request| match request["type"]
        .as_str()?
    {
        "ListRegisteredApps" => {
            let apps: Vec<_> = [
                ("broken-first", "test-e2e"),
                ("healthy", "test-e2e"),
                ("broken-last", "test-e2e"),
                ("development", "development"),
            ]
            .into_iter()
            .map(|(name, environment)| {
                serde_json::json!({
                    "app_name": name, "environment": environment,
                    "config_file": format!("/{name}/tako.toml"),
                    "config_path": format!("{environment}:/{name}/tako.toml"),
                    "project_dir": format!("/{name}"), "upstream_port": 0,
                    "hosts": [], "status": "running"
                })
            })
            .collect();
            Some(serde_json::json!({"type": "RegisteredApps", "apps": apps}))
        }
        "UnregisterApp" => {
            let key = request["config_path"].as_str().unwrap();
            recorded.lock().unwrap().push(key.to_string());
            Some(if key.contains("broken") {
                serde_json::json!({"type": "Error", "message": "Compose cleanup failed"})
            } else {
                serde_json::json!({"type": "AppUnregistered", "config_path": key})
            })
        }
        _ => None,
    })
    .unwrap();
    let out = run_tako_with_env(
        &["--ci", "dev", "stop", "--all", "--env", "test-e2e"],
        temp.path(),
        &home,
        &tako_home,
    );
    assert!(!out.status.success());
    assert_eq!(
        *attempts.lock().unwrap(),
        [
            "test-e2e:/broken-first/tako.toml",
            "test-e2e:/healthy/tako.toml",
            "test-e2e:/broken-last/tako.toml"
        ]
    );
    let stderr = stderr_str(&out);
    for expected in [
        "Stopped healthy",
        "broken-first",
        "broken-last",
        "Compose cleanup failed",
    ] {
        assert!(stderr.contains(expected), "{stderr}");
    }
}

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

#[test]
fn old_daemon_is_reported_instead_of_an_empty_app_list() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    let tako_home = temp.path().join("tako-home");
    fs::create_dir_all(&home).unwrap();
    let _fake = FakeDevServer::start_with_handler(&tako_home, |request| {
        (request["type"] == "Info").then(|| {
            serde_json::json!({
                "type": "Info", "info": { "listen": "127.0.0.1:8443" }
            })
        })
    })
    .unwrap();
    for args in [
        vec!["dev", "list", "--json"],
        vec!["dev", "stop", "--all", "--json"],
    ] {
        let out = run_tako_with_env(&args, temp.path(), &home, &tako_home);
        assert!(!out.status.success(), "{}", stdout_str(&out));
        let output = format!("{}{}", stdout_str(&out), stderr_str(&out));
        assert!(
            output.contains("daemon protocol does not match"),
            "{output}"
        );
        assert!(output.contains("tako dev --restart"), "{output}");
    }
}

#[test]
fn cloud_preview_returns_the_tunnel_on_start_attach_and_restart() {
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let temp = TempDir::new().unwrap();
    let project = temp.path().canonicalize().unwrap();
    let home = project.join("tako-home");
    let config = project.join("tako.toml");
    fs::write(&config, "name = 'preview'\nruntime = 'go'\nmain = 'app'\n").unwrap();
    let registrations = Arc::new(AtomicUsize::new(0));
    let launches = registrations.clone();
    let mut enable_attempts = 0;
    const URL: &str = "https://preview.example.com/";
    let _daemon =
        FakeDevServer::start_with_handler(&home, move |req| match req["type"].as_str()? {
            "Info" => Some(json!({"type":"Info","info":{
                "protocol_version": tako_core::PROTOCOL_VERSION, "listen":"127.0.0.1:47831",
                "advertised_ip":"127.0.0.1", "local_dns_enabled":false
            }})),
            "RegisterApp" => {
                launches.fetch_add(1, Ordering::SeqCst);
                Some(json!({"type":"AppRegistered","url":"https://preview.test/"}))
            }
            "ListRegisteredApps" if launches.load(Ordering::SeqCst) > 0 => Some(json!({
                "type":"RegisteredApps","apps":[{
                    "config_path":config, "config_file":config, "project_dir":config.parent(),
                    "environment":"development", "app_name":"preview", "status":"running",
                    "hosts":["preview.test"], "upstream_port":12345, "tunnel_url":URL
                }]
            })),
            "ToggleTunnel" => {
                assert_eq!(req["enabled"], true);
                enable_attempts += 1;
                Some(if enable_attempts == 4 {
                    json!({"type":"Error","message":"relay unavailable"})
                } else {
                    json!({"type":"TunnelToggled","url":URL,"enabled":true})
                })
            }
            _ => None,
        })
        .unwrap();
    let mut args = vec!["dev", "--tunnel", "--background", "--json"];
    for (restart, expected_launches) in [(false, 1), (false, 1), (true, 2)] {
        if restart {
            args.push("--restart");
        }
        let out = run_tako_with_env(&args, &project, &project, &home);
        assert!(out.status.success(), "{}", stderr_str(&out));
        let ready: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(ready["url"], URL);
        assert_eq!(ready["hosts"], json!([]));
        assert_eq!(ready["type"], "ready");
        assert_eq!(registrations.load(Ordering::SeqCst), expected_launches);
    }
    let out = run_tako_with_env(&args, &project, &project, &home);
    assert!(!out.status.success());
    assert!(stdout_str(&out).contains("relay unavailable"));
    assert!(
        !home.join("ca").exists(),
        "the CLI should leave the private loopback CA to the daemon"
    );
}
