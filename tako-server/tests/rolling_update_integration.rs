mod support;

use std::collections::HashMap;
use std::fs;
use std::time::Duration;

use support::{
    TestServer, bun_ok, can_bind_local_ports, wait_for, write_bun_app, write_failing_bun_app,
};

#[test]
fn rolling_update_deploy_updates_version_and_serves_new_code() {
    if !bun_ok() {
        return;
    }
    if !can_bind_local_ports() {
        return;
    }

    let server = TestServer::start();
    let app_id = "test-app/production";
    let app_dir_v1 = server
        .data_dir()
        .join("apps")
        .join("test-app")
        .join("production")
        .join("releases")
        .join("v1");
    let app_dir_v2 = server
        .data_dir()
        .join("apps")
        .join("test-app")
        .join("production")
        .join("releases")
        .join("v2");
    fs::create_dir_all(&app_dir_v1).unwrap();
    fs::create_dir_all(&app_dir_v2).unwrap();

    write_bun_app(&app_dir_v1, "v1");

    let host = "test.localhost";

    let resp = server.send_command(&serde_json::json!({
        "command": "deploy",
        "app": app_id,
        "version": "v1",
        "path": app_dir_v1.to_string_lossy(),
        "routes": [host],
    }));
    assert_eq!(
        resp.get("status").and_then(|s| s.as_str()),
        Some("ok"),
        "{:?}",
        resp
    );

    let mut last_v1_body = String::new();
    assert!(
        wait_for(Duration::from_secs(30), || {
            let body = server.https_get(host, "/");
            if body.contains("v1") {
                return true;
            }
            last_v1_body = body;
            false
        }),
        "timed out waiting for v1 response, last body: {}",
        last_v1_body
    );

    // Deploy v2 (rolling update).
    write_bun_app(&app_dir_v2, "v2");

    let resp = server.send_command(&serde_json::json!({
        "command": "deploy",
        "app": app_id,
        "version": "v2",
        "path": app_dir_v2.to_string_lossy(),
        "routes": [host],
    }));
    assert_eq!(
        resp.get("status").and_then(|s| s.as_str()),
        Some("ok"),
        "{:?}",
        resp
    );

    let mut last_v2_body = String::new();
    assert!(
        wait_for(Duration::from_secs(90), || {
            let body = server.https_get(host, "/");
            if body.contains("v2") {
                return true;
            }
            last_v2_body = body;
            false
        }),
        "timed out waiting for v2 response, last body: {}",
        last_v2_body
    );

    // Eventually we should converge back to 1 healthy instance, and status should report v2.
    let mut last_status = serde_json::json!({"status": "no-response"});
    assert!(
        wait_for(Duration::from_secs(90), || {
            let resp = server.send_command(&serde_json::json!({
                "command": "status",
                "app": app_id,
            }));
            last_status = resp.clone();
            let data = match resp.get("data") {
                Some(d) => d,
                None => return false,
            };

            let version_ok = data.get("version").and_then(|v| v.as_str()) == Some("v2");
            let instances = data.get("instances").and_then(|i| i.as_array());
            let instances = match instances {
                Some(i) => i,
                None => return false,
            };
            if instances.len() != 1 {
                return false;
            }
            let state_ok = instances[0].get("state").and_then(|s| s.as_str()) == Some("healthy");
            version_ok && state_ok
        }),
        "timed out waiting for converged v2 status, last status: {}",
        last_status
    );
}

#[test]
fn failed_rolling_update_keeps_previous_release_serving() {
    if !bun_ok() {
        return;
    }
    if !can_bind_local_ports() {
        return;
    }

    let server = TestServer::start();
    let app_id = "rollback-app/production";
    let app_dir_v1 = server
        .data_dir()
        .join("apps")
        .join("rollback-app")
        .join("production")
        .join("releases")
        .join("v1");
    let app_dir_v2 = server
        .data_dir()
        .join("apps")
        .join("rollback-app")
        .join("production")
        .join("releases")
        .join("v2");
    fs::create_dir_all(&app_dir_v1).unwrap();
    fs::create_dir_all(&app_dir_v2).unwrap();

    write_bun_app(&app_dir_v1, "v1");

    let old_host = "rollback.localhost";
    let new_host = "rollback-new.localhost";
    let old_secrets: HashMap<String, String> =
        HashMap::from([("API_KEY".to_string(), "old-secret".to_string())]);
    let new_secrets: HashMap<String, String> =
        HashMap::from([("API_KEY".to_string(), "new-secret".to_string())]);

    let resp = server.send_command(&serde_json::json!({
        "command": "deploy",
        "app": app_id,
        "version": "v1",
        "path": app_dir_v1.to_string_lossy(),
        "routes": [old_host],
        "secrets": old_secrets,
    }));
    assert_eq!(
        resp.get("status").and_then(|s| s.as_str()),
        Some("ok"),
        "{:?}",
        resp
    );

    let mut last_v1_body = String::new();
    assert!(
        wait_for(Duration::from_secs(30), || {
            let body = server.https_get(old_host, "/");
            if body.contains("v1") {
                return true;
            }
            last_v1_body = body;
            false
        }),
        "timed out waiting for v1 response, last body: {}",
        last_v1_body
    );

    write_failing_bun_app(&app_dir_v2, "v2 failed during startup");

    let resp = server.send_command(&serde_json::json!({
        "command": "deploy",
        "app": app_id,
        "version": "v2",
        "path": app_dir_v2.to_string_lossy(),
        "routes": [new_host],
        "secrets": new_secrets,
    }));
    assert_eq!(
        resp.get("status").and_then(|s| s.as_str()),
        Some("error"),
        "failed deploy should return an error: {:?}",
        resp
    );

    let mut last_body = String::new();
    assert!(
        wait_for(Duration::from_secs(30), || {
            let body = server.https_get(old_host, "/");
            if body.contains("v1") {
                return true;
            }
            last_body = body;
            false
        }),
        "previous release stopped serving after failed rollout, last body: {}",
        last_body
    );

    let status = server.send_command(&serde_json::json!({
        "command": "status",
        "app": app_id,
    }));
    let data = status
        .get("data")
        .unwrap_or_else(|| panic!("status should include data: {status}"));
    assert_eq!(data.get("version").and_then(|v| v.as_str()), Some("v1"));
    assert_eq!(data.get("state").and_then(|s| s.as_str()), Some("running"));
    let instances = data
        .get("instances")
        .and_then(|i| i.as_array())
        .unwrap_or_else(|| panic!("status should include instances: {status}"));
    assert!(
        instances
            .iter()
            .any(|instance| instance.get("state").and_then(|s| s.as_str()) == Some("healthy")),
        "expected a healthy v1 instance after rollback: {status}"
    );
    let builds = data
        .get("builds")
        .and_then(|b| b.as_array())
        .unwrap_or_else(|| panic!("status should include builds: {status}"));
    assert!(
        builds.iter().any(|build| {
            build.get("version").and_then(|v| v.as_str()) == Some("v1")
                && build.get("state").and_then(|s| s.as_str()) == Some("running")
        }),
        "expected running build status for v1 after rollback: {status}"
    );

    let secrets_hash = server.send_command(&serde_json::json!({
        "command": "get_secrets_hash",
        "app": app_id,
    }));
    let restored_hash = secrets_hash
        .get("data")
        .and_then(|data| data.get("hash"))
        .and_then(|hash| hash.as_str())
        .unwrap_or_else(|| panic!("secrets hash should include data.hash: {secrets_hash}"));
    let expected_hash = tako_core::compute_secrets_hash(&HashMap::from([(
        "API_KEY".to_string(),
        "old-secret".to_string(),
    )]));
    assert_eq!(
        restored_hash, expected_hash,
        "failed deploy should restore previous secrets"
    );

    let new_host_status = server.https_status(new_host, "/").unwrap_or(0);
    assert_eq!(
        new_host_status, 404,
        "failed deploy route should not replace previous routes"
    );
}

#[test]
fn failed_scaled_to_zero_deploy_restores_previous_release() {
    if !bun_ok() {
        return;
    }
    if !can_bind_local_ports() {
        return;
    }

    let server = TestServer::start();
    let app_id = "idle-rollback-app/production";
    let app_dir_v1 = server
        .data_dir()
        .join("apps")
        .join("idle-rollback-app")
        .join("production")
        .join("releases")
        .join("v1");
    let app_dir_v2 = server
        .data_dir()
        .join("apps")
        .join("idle-rollback-app")
        .join("production")
        .join("releases")
        .join("v2");
    fs::create_dir_all(&app_dir_v1).unwrap();
    fs::create_dir_all(&app_dir_v2).unwrap();

    write_bun_app(&app_dir_v1, "idle-v1");

    let old_host = "idle-rollback.localhost";
    let new_host = "idle-rollback-new.localhost";

    let resp = server.send_command(&serde_json::json!({
        "command": "deploy",
        "app": app_id,
        "version": "v1",
        "path": app_dir_v1.to_string_lossy(),
        "routes": [old_host],
    }));
    assert_eq!(
        resp.get("status").and_then(|s| s.as_str()),
        Some("ok"),
        "{:?}",
        resp
    );

    let resp = server.send_command(&serde_json::json!({
        "command": "scale",
        "app": app_id,
        "instances": 0,
    }));
    assert_eq!(
        resp.get("status").and_then(|s| s.as_str()),
        Some("ok"),
        "{:?}",
        resp
    );

    let mut idle_status = serde_json::Value::Null;
    assert!(
        wait_for(Duration::from_secs(30), || {
            idle_status = server.send_command(&serde_json::json!({
                "command": "status",
                "app": app_id,
            }));
            let Some(data) = idle_status.get("data") else {
                return false;
            };
            let state = data.get("state").and_then(|s| s.as_str());
            let instances = data
                .get("instances")
                .and_then(|i| i.as_array())
                .map(Vec::is_empty)
                .unwrap_or(false);
            state == Some("idle") && instances
        }),
        "app did not scale to idle before failed deploy: {idle_status}"
    );

    write_failing_bun_app(&app_dir_v2, "idle v2 failed during startup");

    let resp = server.send_command(&serde_json::json!({
        "command": "deploy",
        "app": app_id,
        "version": "v2",
        "path": app_dir_v2.to_string_lossy(),
        "routes": [new_host],
    }));
    assert_eq!(
        resp.get("status").and_then(|s| s.as_str()),
        Some("error"),
        "failed deploy should return an error: {:?}",
        resp
    );

    let status = server.send_command(&serde_json::json!({
        "command": "status",
        "app": app_id,
    }));
    let data = status
        .get("data")
        .unwrap_or_else(|| panic!("status should include data: {status}"));
    assert_eq!(data.get("version").and_then(|v| v.as_str()), Some("v1"));
    assert_eq!(data.get("state").and_then(|s| s.as_str()), Some("idle"));

    let mut last_body = String::new();
    assert!(
        wait_for(Duration::from_secs(30), || {
            let body = server.https_get(old_host, "/");
            if body.contains("idle-v1") {
                return true;
            }
            last_body = body;
            false
        }),
        "previous idle release did not cold start after failed deploy, last body: {}",
        last_body
    );

    let new_host_status = server.https_status(new_host, "/").unwrap_or(0);
    assert_eq!(
        new_host_status, 404,
        "failed deploy route should not replace previous idle routes"
    );
}
