use super::*;

#[test]
fn test_list_apps_empty() {
    if !require_localhost_bind() {
        return;
    }

    let server = TestServer::start();

    let response = server.send_command(&serde_json::json!({ "command": "list" }));
    assert_eq!(response.get("status").and_then(|s| s.as_str()), Some("ok"));

    let apps = response
        .get("data")
        .and_then(|d| d.get("apps"))
        .and_then(|a| a.as_array())
        .expect("response should include data.apps array");
    assert!(apps.is_empty(), "expected no apps, got: {response}");
}

#[test]
fn test_deploy_and_list() {
    if !require_localhost_bind() || !e2e_enabled() || !bun_available() {
        return;
    }

    let server = TestServer::start();
    let app_id = "test-app/production";

    // Create a Bun app that serves requests on PORT.
    let app_dir = server
        .data_dir()
        .join("apps")
        .join("test-app")
        .join("production")
        .join("releases")
        .join("v1");
    fs::create_dir_all(&app_dir).unwrap();
    fs::create_dir_all(app_dir.join("node_modules/tako.sh/dist/entrypoints")).unwrap();
    fs::write(
        app_dir.join("package.json"),
        r#"{"name":"test-app","scripts":{"dev":"bun run index.ts"}}"#,
    )
    .unwrap();
    fs::write(
        app_dir.join("node_modules/tako.sh/dist/entrypoints/bun-server.mjs"),
        "await import(process.argv[2]);",
    )
    .unwrap();
    fs::write(
            app_dir.join("app.json"),
            r#"{"runtime":"bun","main":"index.ts","idle_timeout":300,"install":"true","start":["bun","{main}"]}"#,
        )
        .unwrap();
    fs::write(
        app_dir.join("index.ts"),
        r#"
import { closeSync, readFileSync } from "node:fs";

const port = Number(process.env.PORT ?? "3000");
const host = process.env.HOST ?? "127.0.0.1";
const bootstrap = JSON.parse(readFileSync(3, "utf-8"));
closeSync(3);
const internalToken = bootstrap.token;
if (!internalToken) {
  throw new Error("bootstrap envelope on fd 3 did not provide a token");
}

Bun.serve({
  hostname: host,
  port,
  fetch(request) {
    const url = new URL(request.url);
    const requestHost = (request.headers.get("host") ?? url.host).split(":")[0]?.toLowerCase();
    if (requestHost === "test-app.tako" && url.pathname === "/status") {
      if (request.headers.get("x-tako-internal-token") !== internalToken) {
        return new Response(JSON.stringify({ error: "forbidden" }), {
          status: 403,
          headers: { "Content-Type": "application/json" },
        });
      }
      return new Response(JSON.stringify({ status: "ok" }), {
        headers: {
          "Content-Type": "application/json",
          "X-Tako-Internal-Token": internalToken,
        },
      });
    }
    return new Response("test");
  },
});
"#,
    )
    .unwrap();

    let deploy_cmd = serde_json::json!({
        "command": "deploy",
        "app": app_id,
        "version": "v1",
        "path": app_dir.to_string_lossy(),
        "routes": ["test-app.localhost"],
    });

    let deploy_response = server.send_command(&deploy_cmd);
    assert_eq!(
        deploy_response.get("status").and_then(|s| s.as_str()),
        Some("ok"),
        "deploy should succeed: {deploy_response}"
    );

    // List should show the app.
    let list_response = server.send_command(&serde_json::json!({ "command": "list" }));
    let apps = list_response
        .get("data")
        .and_then(|d| d.get("apps"))
        .and_then(|a| a.as_array())
        .expect("response should include data.apps array");
    assert!(
        apps.iter()
            .any(|a| a.get("name").and_then(|n| n.as_str()) == Some(app_id)),
        "expected {app_id} in list response: {list_response}"
    );
}
