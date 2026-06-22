mod support;

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::Duration;

use support::{
    TestServer, bun_ok, can_bind_local_ports, wait_for, write_bun_app,
    write_second_instance_flapping_bun_app,
};

fn write_failing_bun_app(app_dir: &Path, message: &str) {
    fs::create_dir_all(app_dir.join("src")).unwrap();
    fs::create_dir_all(app_dir.join("node_modules/tako.sh/dist/entrypoints")).unwrap();
    fs::write(
        app_dir.join("package.json"),
        r#"{"name":"test-app","scripts":{"dev":"bun src/index.ts"}}"#,
    )
    .unwrap();
    fs::write(
        app_dir.join("node_modules/tako.sh/dist/entrypoints/bun-server.mjs"),
        "await import(process.argv[2]);",
    )
    .unwrap();
    fs::write(
        app_dir.join("app.json"),
        r#"{"runtime":"bun","main":"src/index.ts","idle_timeout":300,"install":"true","start":["bun","{main}"]}"#,
    )
    .unwrap();
    fs::write(
        app_dir.join("src/index.ts"),
        format!("throw new Error({message:?});\n"),
    )
    .unwrap();
}

fn write_flapping_health_bun_app(app_dir: &Path, body: &str) {
    fs::create_dir_all(app_dir.join("src")).unwrap();
    fs::create_dir_all(app_dir.join("node_modules/tako.sh/dist/entrypoints")).unwrap();
    fs::write(
        app_dir.join("package.json"),
        r#"{"name":"test-app","scripts":{"dev":"bun src/index.ts"}}"#,
    )
    .unwrap();
    fs::write(
        app_dir.join("node_modules/tako.sh/dist/entrypoints/bun-server.mjs"),
        "await import(process.argv[2]);",
    )
    .unwrap();
    fs::write(
        app_dir.join("app.json"),
        r#"{"runtime":"bun","main":"src/index.ts","idle_timeout":300,"install":"true","start":["bun","{main}"]}"#,
    )
    .unwrap();
    fs::write(
        app_dir.join("src/index.ts"),
        format!(
            r#"import {{ closeSync, fstatSync, readFileSync, writeSync }} from "node:fs";

const port = Number(process.env.PORT ?? "3000");
const host = process.env.HOST ?? "127.0.0.1";
const bootstrap = JSON.parse(readFileSync(3, "utf-8"));
closeSync(3);
const internalToken = bootstrap.token;
const internalAppName = (process.env.TAKO_APP_NAME ?? "app").split("/")[0] || "app";
const internalHost = `${{internalAppName}}.tako`;
const startedAt = Date.now();

function signalReady(port) {{
  try {{
    const stat = fstatSync(4);
    if (!stat.isFIFO()) return;
    writeSync(4, `${{port}}\n`);
    closeSync(4);
  }} catch {{}}
}}

const server = Bun.serve({{
  hostname: host,
  port,
  fetch(req) {{
    const url = new URL(req.url);
    const requestHost = (req.headers.get("host") ?? url.host).split(":")[0]?.toLowerCase();
    if (requestHost === internalHost && url.pathname === "/status") {{
      if (req.headers.get("x-tako-internal-token") !== internalToken) {{
        return new Response(JSON.stringify({{ error: "forbidden" }}), {{
          status: 403,
          headers: {{ "content-type": "application/json" }},
        }});
      }}
      if (Date.now() - startedAt > 250) {{
        return new Response(JSON.stringify({{ status: "unhealthy" }}), {{
          status: 500,
          headers: {{
            "content-type": "application/json",
            "X-Tako-Internal-Token": internalToken,
          }},
        }});
      }}
      return new Response(JSON.stringify({{ status: "healthy" }}), {{
        headers: {{
          "content-type": "application/json",
          "X-Tako-Internal-Token": internalToken,
        }},
      }});
    }}
    if (url.pathname === "/") {{
      return new Response({body:?}, {{ headers: {{ "content-type": "text/plain" }} }});
    }}
    return new Response("not found", {{ status: 404 }});
  }},
}});

signalReady(server.port);
"#,
        ),
    )
    .unwrap();
}

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
        "ssl": {
            "provider": "letsencrypt",
            "cloudflare_api_token": "old-token"
        },
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
    let message = resp
        .get("message")
        .and_then(|s| s.as_str())
        .unwrap_or_default();
    assert!(
        message.contains("rolled back to previous release"),
        "failed deploy should explain rollback: {resp}"
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

    let conn = rusqlite::Connection::open(server.data_dir().join("state.sqlite")).unwrap();
    let ssl_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM app_ssl WHERE app = ?1",
            [app_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(ssl_rows, 1, "failed deploy should restore previous SSL");

    let new_host_status = server.https_status(new_host, "/").unwrap_or(0);
    assert_eq!(
        new_host_status, 404,
        "failed deploy route should not replace previous routes"
    );
}

#[test]
fn flapping_rolling_update_rolls_back_before_draining_previous_release() {
    if !bun_ok() {
        return;
    }
    if !can_bind_local_ports() {
        return;
    }

    let server = TestServer::start();
    let app_id = "flap-app/production";
    let app_dir_v1 = server
        .data_dir()
        .join("apps")
        .join("flap-app")
        .join("production")
        .join("releases")
        .join("v1");
    let app_dir_v2 = server
        .data_dir()
        .join("apps")
        .join("flap-app")
        .join("production")
        .join("releases")
        .join("v2");
    fs::create_dir_all(&app_dir_v1).unwrap();
    fs::create_dir_all(&app_dir_v2).unwrap();

    write_bun_app(&app_dir_v1, "v1");

    let host = "flap.localhost";
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

    write_flapping_health_bun_app(&app_dir_v2, "v2");

    let resp = server.send_command(&serde_json::json!({
        "command": "deploy",
        "app": app_id,
        "version": "v2",
        "path": app_dir_v2.to_string_lossy(),
        "routes": [host],
    }));
    assert_eq!(
        resp.get("status").and_then(|s| s.as_str()),
        Some("error"),
        "flapping deploy should fail and roll back: {:?}",
        resp
    );
    let message = resp
        .get("message")
        .and_then(|s| s.as_str())
        .unwrap_or_default();
    assert!(
        message.contains("rolled back to previous release"),
        "failed deploy should explain rollback: {resp}"
    );
    assert!(
        message.contains("rollout stability"),
        "failed deploy should identify stability failure: {resp}"
    );

    let mut last_body = String::new();
    assert!(
        wait_for(Duration::from_secs(30), || {
            let body = server.https_get(host, "/");
            if body.contains("v1") {
                return true;
            }
            last_body = body;
            false
        }),
        "previous release stopped serving after flapping rollout, last body: {}",
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
}

#[test]
fn later_batch_failure_keeps_all_previous_instances_serving() {
    if !bun_ok() {
        return;
    }
    if !can_bind_local_ports() {
        return;
    }

    let server = TestServer::start();
    let app_id = "partial-rollback-app/production";
    let app_dir_v1 = server
        .data_dir()
        .join("apps")
        .join("partial-rollback-app")
        .join("production")
        .join("releases")
        .join("v1");
    let app_dir_v2 = server
        .data_dir()
        .join("apps")
        .join("partial-rollback-app")
        .join("production")
        .join("releases")
        .join("v2");
    fs::create_dir_all(&app_dir_v1).unwrap();
    fs::create_dir_all(&app_dir_v2).unwrap();

    write_bun_app(&app_dir_v1, "v1");

    let host = "partial-rollback.localhost";
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

    let resp = server.send_command(&serde_json::json!({
        "command": "scale",
        "app": app_id,
        "instances": 2,
    }));
    assert_eq!(
        resp.get("status").and_then(|s| s.as_str()),
        Some("ok"),
        "{:?}",
        resp
    );

    let mut scaled_status = serde_json::Value::Null;
    assert!(
        wait_for(Duration::from_secs(30), || {
            scaled_status = server.send_command(&serde_json::json!({
                "command": "status",
                "app": app_id,
            }));
            let Some(instances) = scaled_status
                .get("data")
                .and_then(|data| data.get("instances"))
                .and_then(|instances| instances.as_array())
            else {
                return false;
            };
            instances.len() == 2
                && instances.iter().all(|instance| {
                    instance.get("state").and_then(|s| s.as_str()) == Some("healthy")
                })
        }),
        "app did not scale to two healthy v1 instances: {scaled_status}"
    );

    write_second_instance_flapping_bun_app(&app_dir_v2, "v2", "partial-v2-first-started");

    let resp = server.send_command(&serde_json::json!({
        "command": "deploy",
        "app": app_id,
        "version": "v2",
        "path": app_dir_v2.to_string_lossy(),
        "routes": [host],
    }));
    assert_eq!(
        resp.get("status").and_then(|s| s.as_str()),
        Some("error"),
        "later-batch failure should fail and roll back: {:?}",
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
    let instances = data
        .get("instances")
        .and_then(|instances| instances.as_array())
        .unwrap_or_else(|| panic!("status should include instances: {status}"));
    assert_eq!(
        instances.len(),
        2,
        "rollback should keep both previous instances: {status}"
    );
    assert!(
        instances
            .iter()
            .all(|instance| instance.get("state").and_then(|s| s.as_str()) == Some("healthy")),
        "rollback should keep previous instances healthy: {status}"
    );

    let body = server.https_get(host, "/");
    assert!(
        body.contains("v1"),
        "previous release should still serve after later-batch failure: {body}"
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
