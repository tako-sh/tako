use super::*;
use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::Duration;

use tempfile::TempDir;

#[test]
fn telemetry_enabled_by_default_for_official_installs() {
    assert!(telemetry_enabled(true, None, None, false, false));
}

#[test]
fn telemetry_disabled_without_token() {
    assert!(!telemetry_enabled(false, None, None, false, false));
    assert!(!telemetry_enabled(false, Some("1"), None, false, false));
}

#[test]
fn telemetry_opt_out_values() {
    for value in ["0", "false", "off", "no", "FALSE", " 0 "] {
        assert!(
            !telemetry_enabled(true, Some(value), None, false, false),
            "expected opt-out for {value:?}"
        );
    }
}

#[test]
fn telemetry_explicit_opt_in_enables_local_builds() {
    for value in ["1", "true", "on", "yes", "TRUE", " 1 "] {
        assert!(telemetry_enabled(true, Some(value), None, false, true));
    }
}

#[test]
fn telemetry_disabled_in_ci_even_with_explicit_opt_in() {
    for telemetry_env in [None, Some("1"), Some("true"), Some("on"), Some("yes")] {
        for local_build in [false, true] {
            for ci_env in ["true", "1", "on", "yes", "TRUE", " 1 "] {
                assert!(!telemetry_enabled(
                    true,
                    telemetry_env,
                    Some(ci_env),
                    false,
                    local_build,
                ));
            }
            assert!(!telemetry_enabled(
                true,
                telemetry_env,
                None,
                true,
                local_build,
            ));
        }
    }
}

#[test]
fn false_ci_env_allows_telemetry() {
    for value in ["0", "false", "off", "no", "FALSE", " 0 ", ""] {
        assert!(telemetry_enabled(true, None, Some(value), false, false));
    }
}

#[test]
fn telemetry_disabled_for_local_cargo_builds() {
    assert!(!telemetry_enabled(true, None, None, false, true));
}

#[test]
fn unknown_telemetry_env_uses_default_policy() {
    assert!(telemetry_enabled(true, Some(""), None, false, false));
    assert!(telemetry_enabled(true, Some("  "), None, false, false));
    assert!(telemetry_enabled(true, Some("maybe"), None, false, false));
    assert!(!telemetry_enabled(
        true,
        Some("maybe"),
        Some("true"),
        false,
        false
    ));
}

#[test]
fn capture_url_strips_trailing_slash() {
    assert_eq!(
        capture_url("https://us.i.posthog.com"),
        "https://us.i.posthog.com/i/v0/e/"
    );
    assert_eq!(
        capture_url("https://eu.i.posthog.com/"),
        "https://eu.i.posthog.com/i/v0/e/"
    );
}

#[test]
fn capture_payload_is_anonymous_and_closed() {
    let payload = capture_payload(
        "phc_test",
        "aabbccddeeff00112233445566778899",
        "deploy",
        "0.0.0-abc1234",
        "macos",
        "aarch64",
    );

    assert_eq!(payload["event"], "cli_command");
    assert_eq!(payload["api_key"], "phc_test");
    assert_eq!(payload["distinct_id"], "aabbccddeeff00112233445566778899");
    assert_eq!(payload["properties"]["command"], "deploy");
    assert_eq!(payload["properties"]["version"], "0.0.0-abc1234");
    assert_eq!(payload["properties"]["os"], "macos");
    assert_eq!(payload["properties"]["arch"], "aarch64");
    assert_eq!(payload["properties"]["$lib"], "tako-cli");
    assert_eq!(payload["properties"]["$geoip_disable"], true);
    assert_eq!(payload["properties"]["$ip"], "0.0.0.0");
    assert_eq!(payload["properties"]["$process_person_profile"], false);

    let top_keys: BTreeSet<&str> = payload
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        top_keys,
        BTreeSet::from(["api_key", "distinct_id", "event", "properties"])
    );

    let property_keys: BTreeSet<&str> = payload["properties"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        property_keys,
        BTreeSet::from([
            "$lib",
            "$geoip_disable",
            "$ip",
            "$process_person_profile",
            "command",
            "version",
            "os",
            "arch",
        ])
    );
}

#[test]
fn capture_payload_names_each_command() {
    for command in ["deploy", "dev", "dev stop", "init"] {
        let payload = capture_payload(
            "phc_test",
            "aabbccddeeff00112233445566778899",
            command,
            "0.0.0",
            "macos",
            "aarch64",
        );
        assert_eq!(payload["event"], "cli_command");
        assert_eq!(payload["properties"]["command"], command);
    }
}

#[test]
fn local_build_detects_cargo_target_binaries() {
    assert!(is_local_build_exe(Path::new(
        "/Users/me/proj/target/debug/tako"
    )));
    assert!(is_local_build_exe(Path::new(
        "/Users/me/proj/target/release/tako"
    )));
    assert!(!is_local_build_exe(Path::new("/usr/local/bin/tako")));
    assert!(!is_local_build_exe(Path::new(
        "/Applications/Tako.app/Contents/MacOS/tako"
    )));
}

#[test]
fn load_or_create_state_persists_stable_id() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("telemetry.json");

    let first = load_or_create_state(&path).unwrap();
    let second = load_or_create_state(&path).unwrap();

    assert_eq!(first.id.len(), 32);
    assert!(valid_id(&first.id));
    assert_eq!(first.id, second.id);
    assert!(!first.notice_shown);
}

#[test]
fn load_or_create_state_replaces_invalid_id() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("telemetry.json");
    fs::write(&path, r#"{"id":"not-an-id","last_sent_unix":9}"#).unwrap();

    let state = load_or_create_state(&path).unwrap();
    assert!(valid_id(&state.id));
}

#[test]
fn load_or_create_state_stops_when_id_cannot_be_persisted() {
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("not-a-directory");
    fs::write(&file, "occupied").unwrap();

    assert_eq!(load_or_create_state(&file.join("telemetry.json")), None);
}

#[test]
fn flush_without_pending_send_returns() {
    flush();
}

#[test]
fn save_state_round_trips_notice() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("nested").join("telemetry.json");
    let original = TelemetryState {
        id: "aabbccddeeff00112233445566778899".into(),
        notice_shown: true,
    };

    save_state(&path, &original).unwrap();
    let loaded = load_or_create_state(&path).unwrap();
    assert_eq!(loaded, original);
}

#[test]
fn post_capture_posts_json_to_local_server() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let request = read_http_request(&mut stream);
        let _ =
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        request
    });

    let payload = capture_payload(
        "phc_test",
        "aabbccddeeff00112233445566778899",
        "status",
        "0.0.0",
        "linux",
        "x86_64",
    );
    let url = format!("http://{addr}/i/v0/e/");
    assert!(post_capture(&url, &payload));

    let request = server.join().unwrap();
    assert!(request.starts_with("POST "));
    assert!(request.contains("cli_command"));
    assert!(request.contains("aabbccddeeff00112233445566778899"));
    assert!(request.contains("\"command\":\"status\""));
    assert!(!request.contains("/Users/"));
    assert!(!request.contains("HOME="));
}

fn read_http_request(stream: &mut std::net::TcpStream) -> String {
    let mut buf = Vec::new();
    let mut tmp = [0_u8; 1024];
    loop {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if let Some(header_end) = find_headers_end(&buf)
                    && let Some(length) = content_length(&buf[..header_end])
                    && buf.len() >= header_end + 4 + length
                {
                    break;
                }
            }
            Err(_) => break,
        }
        if buf.len() > 16 * 1024 {
            break;
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn find_headers_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|window| window == b"\r\n\r\n")
}

fn content_length(headers: &[u8]) -> Option<usize> {
    let headers = std::str::from_utf8(headers).ok()?;
    headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.eq_ignore_ascii_case("content-length") {
            value.trim().parse().ok()
        } else {
            None
        }
    })
}
