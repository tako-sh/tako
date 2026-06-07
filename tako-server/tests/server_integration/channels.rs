use super::*;
use std::io::{Read, Write};
use std::net::TcpStream;

fn deploy_chat_app(server: &TestServer) {
    let app_id = "chat-app/production";
    let app_dir = server
        .data_dir()
        .join("apps")
        .join("chat-app")
        .join("production")
        .join("releases")
        .join("v1");
    fs::create_dir_all(&app_dir).unwrap();
    fs::create_dir_all(app_dir.join("node_modules/tako.sh/dist/entrypoints")).unwrap();
    fs::write(
        app_dir.join("package.json"),
        r#"{"name":"chat-app","scripts":{"dev":"bun run index.ts"}}"#,
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
  async fetch(request) {
    const url = new URL(request.url);
    const requestHost = (request.headers.get("host") ?? url.host).split(":")[0]?.toLowerCase();
    if (requestHost === "chat-app.tako" && url.pathname === "/status") {
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

    if (requestHost === "chat-app.tako" && url.pathname === "/channels/authorize") {
      if (request.headers.get("x-tako-internal-token") !== internalToken) {
        return new Response(JSON.stringify({ error: "forbidden" }), {
          status: 403,
          headers: { "Content-Type": "application/json" },
        });
      }
      const payload = await request.json();
      const authz = payload.header?.scheme
        ? `${payload.header.scheme} ${payload.header.value}`
        : payload.header?.value;
      if (payload.channel !== "chat" || payload.params?.roomId !== "room-123") {
        return new Response(JSON.stringify({ ok: false, error: "not_defined" }), {
          status: 404,
          headers: { "Content-Type": "application/json" },
        });
      }
      if (authz !== "Bearer good") {
        return new Response(JSON.stringify({ ok: false }), {
          status: 403,
          headers: { "Content-Type": "application/json" },
        });
      }
      return new Response(JSON.stringify({
        ok: true,
        subject: "user-123",
        replayWindowMs: 86400000,
        keepaliveIntervalMs: 25,
        maxConnectionLifetimeMs: 200,
        transport: "ws"
      }), { headers: { "Content-Type": "application/json" } });
    }

    if (requestHost === "chat-app.tako" && url.pathname === "/channels/registry") {
      if (request.headers.get("x-tako-internal-token") !== internalToken) {
        return new Response(JSON.stringify({ error: "forbidden" }), {
          status: 403,
          headers: { "Content-Type": "application/json" },
        });
      }
      return new Response(JSON.stringify([{
        channel: "chat",
        paramsSchema: {
          type: "object",
          properties: {
            roomId: { type: "string", minLength: 1 },
          },
          required: ["roomId"],
        },
        auth: { headerName: "authorization" },
        transport: "ws",
      }]), { headers: { "Content-Type": "application/json" } });
    }

    return new Response("chat-app");
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
        "routes": ["chat-app.localhost"],
    });

    let deploy_response = server.send_command(&deploy_cmd);
    assert_eq!(
        deploy_response.get("status").and_then(|s| s.as_str()),
        Some("ok"),
        "deploy should succeed: {deploy_response}"
    );
}

#[test]
fn test_publish_and_sse_auth_with_app_auth() {
    if !require_localhost_bind() || !e2e_enabled() || !bun_available() {
        return;
    }

    let server = TestServer::start();
    deploy_chat_app(&server);

    let published = publish_via_websocket(&server, "hi");
    assert!(published.contains(r#""channel":"chat""#), "{published}");
    assert!(published.contains(r#""type":"message""#), "{published}");

    let events = server
        .http_get_with_host_and_headers(
            "chat-app.localhost",
            "/_tako/channels/chat?roomId=room-123",
            &[
                ("X-Forwarded-Proto", "https"),
                ("Authorization", "Bearer good"),
                ("Accept", "text/event-stream"),
                ("Last-Event-ID", "0"),
            ],
        )
        .expect("events should succeed");
    assert!(
        events.starts_with("HTTP/1.1 200") || events.starts_with("HTTP/1.0 200"),
        "expected 200 SSE response: {events}"
    );
    assert!(events.contains(r#""text":"hi""#), "{events}");

    let denied = server
        .http_get_with_host_and_headers(
            "chat-app.localhost",
            "/_tako/channels/chat?roomId=room-123",
            &[
                ("X-Forwarded-Proto", "https"),
                ("Accept", "text/event-stream"),
                ("Last-Event-ID", "0"),
            ],
        )
        .expect("denied SSE should complete");
    assert!(
        denied.starts_with("HTTP/1.1 403") || denied.starts_with("HTTP/1.0 403"),
        "expected 403 SSE response: {denied}"
    );
}

#[test]
fn test_events_stream_returns_sse_messages() {
    if !require_localhost_bind() || !e2e_enabled() || !bun_available() {
        return;
    }

    let server = TestServer::start();
    deploy_chat_app(&server);

    publish_via_websocket(&server, "hello sse");

    let events = server
        .http_get_with_host_and_headers(
            "chat-app.localhost",
            "/_tako/channels/chat?roomId=room-123",
            &[
                ("X-Forwarded-Proto", "https"),
                ("Authorization", "Bearer good"),
                ("Accept", "text/event-stream"),
                ("Last-Event-ID", "0"),
            ],
        )
        .expect("events request should succeed");
    assert!(
        events.starts_with("HTTP/1.1 200") || events.starts_with("HTTP/1.0 200"),
        "expected 200 events response: {events}"
    );
    assert!(
        events
            .to_ascii_lowercase()
            .contains("content-type: text/event-stream"),
        "expected text/event-stream response: {events}"
    );
    assert!(
        events.contains(
            r#"data: {"id":"1","channel":"chat","type":"message","data":{"text":"hello sse"}}"#
        ),
        "{events}"
    );
}

fn websocket_connect(server: &TestServer, path: &str) -> (TcpStream, String) {
    let mut stream =
        TcpStream::connect(("127.0.0.1", server.http_port)).expect("connect websocket tcp");
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();
    stream
        .set_write_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();

    let request = format!(
        "GET {path} HTTP/1.1\r\n\
             Host: chat-app.localhost\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Version: 13\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Authorization: Bearer good\r\n\
             X-Forwarded-Proto: https\r\n\
             \r\n"
    );
    stream.write_all(request.as_bytes()).unwrap();

    let mut response = Vec::new();
    let mut byte = [0u8; 1];
    while !response.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).unwrap();
        response.push(byte[0]);
    }

    (stream, String::from_utf8_lossy(&response).to_string())
}

fn publish_via_websocket(server: &TestServer, text: &str) -> String {
    let (mut stream, handshake) = websocket_connect(server, "/_tako/channels/chat?roomId=room-123");
    assert!(
        handshake.starts_with("HTTP/1.1 101") || handshake.starts_with("HTTP/1.0 101"),
        "expected websocket upgrade response: {handshake}"
    );

    write_masked_text_frame(
        &mut stream,
        &format!(
            r#"{{"type":"message","data":{{"text":{}}}}}"#,
            serde_json::to_string(text).unwrap()
        ),
    );
    String::from_utf8(read_server_frame(&mut stream)).unwrap()
}

fn read_server_frame(stream: &mut TcpStream) -> Vec<u8> {
    let mut first = [0u8; 2];
    stream.read_exact(&mut first).unwrap();
    let payload_len = usize::from(first[1] & 0x7f);
    if payload_len == 126 {
        let mut extended = [0u8; 2];
        stream.read_exact(&mut extended).unwrap();
        let len = u16::from_be_bytes(extended) as usize;
        let mut payload = vec![0u8; len];
        stream.read_exact(&mut payload).unwrap();
        return payload;
    }
    if payload_len == 127 {
        let mut extended = [0u8; 8];
        stream.read_exact(&mut extended).unwrap();
        let len = u64::from_be_bytes(extended) as usize;
        let mut payload = vec![0u8; len];
        stream.read_exact(&mut payload).unwrap();
        return payload;
    }
    let mut payload = vec![0u8; payload_len];
    stream.read_exact(&mut payload).unwrap();
    payload
}

fn write_masked_text_frame(stream: &mut TcpStream, text: &str) {
    let payload = text.as_bytes();
    let mask = [0x37, 0xfa, 0x21, 0x3d];
    let mut frame = Vec::with_capacity(6 + payload.len());
    frame.push(0x81);
    if payload.len() < 126 {
        frame.push(0x80 | payload.len() as u8);
    } else {
        frame.push(0x80 | 126);
        frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    }
    frame.extend_from_slice(&mask);
    for (index, byte) in payload.iter().enumerate() {
        frame.push(byte ^ mask[index % 4]);
    }
    stream.write_all(&frame).unwrap();
}

#[test]
fn test_websocket_stream_replays_messages_and_accepts_publish_frames() {
    if !require_localhost_bind() || !e2e_enabled() || !bun_available() {
        return;
    }

    let server = TestServer::start();
    deploy_chat_app(&server);

    publish_via_websocket(&server, "hello ws");

    let (mut stream, handshake) = websocket_connect(
        &server,
        "/_tako/channels/chat?roomId=room-123&last_message_id=0",
    );
    assert!(
        handshake.starts_with("HTTP/1.1 101") || handshake.starts_with("HTTP/1.0 101"),
        "expected websocket upgrade response: {handshake}"
    );
    assert!(
        handshake
            .to_ascii_lowercase()
            .contains("sec-websocket-accept:"),
        "expected websocket accept header: {handshake}"
    );

    let first = String::from_utf8(read_server_frame(&mut stream)).unwrap();
    assert!(
        first.contains(r#""text":"hello ws""#),
        "expected websocket replay frame: {first}"
    );

    write_masked_text_frame(
        &mut stream,
        r#"{"type":"message","data":{"text":"sent over ws"}}"#,
    );

    let second = String::from_utf8(read_server_frame(&mut stream)).unwrap();
    assert!(
        second.contains(r#""text":"sent over ws""#),
        "expected websocket published frame: {second}"
    );
}
