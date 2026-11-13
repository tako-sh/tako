//! Pins the SDK's internal-socket requests and reply parsing to the wire
//! fixtures `tako-core` owns in `test-support/wire/` (see its README).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::{Value, json};

use crate::network::{Client, EnqueueOptions};
use crate::{ErrorCode, StoreDefinition, StoreError, StoreRegistry};

const APP: &str = "demo";
/// 2026-01-01T00:00:00Z, the instant the fixtures are built around.
const AT_MS: i64 = 1_767_225_600_000;

fn wire_dir() -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-support/wire");
    assert!(
        dir.is_dir(),
        "wire fixtures not found at {}; run the tests from a full checkout",
        dir.display()
    );
    dir
}

fn fixture_text(path: &str) -> String {
    let path = wire_dir().join(path);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

fn fixture(path: &str) -> Value {
    serde_json::from_str(&fixture_text(path)).unwrap()
}

/// A fake internal socket that answers one connection per reply fixture, in
/// order, and returns the commands it received.
struct WireSocket {
    path: PathBuf,
    server: JoinHandle<Vec<Value>>,
}

impl WireSocket {
    fn serve(replies: &[&str]) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "tako-wire-rust-{}-{}.sock",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let replies: Vec<Value> = replies.iter().map(|path| fixture(path)).collect();
        let server = std::thread::spawn(move || {
            replies
                .into_iter()
                .map(|reply| {
                    let (mut stream, _) = listener.accept().unwrap();
                    let mut line = String::new();
                    BufReader::new(&stream).read_line(&mut line).unwrap();
                    writeln!(stream, "{reply}").unwrap();
                    serde_json::from_str(&line).unwrap()
                })
                .collect()
        });
        Self { path, server }
    }

    fn client(&self) -> Client {
        Client::at(&self.path, APP)
    }

    fn path(&self) -> &str {
        self.path.to_str().unwrap()
    }

    fn received(self) -> Vec<Value> {
        let received = self.server.join().unwrap();
        std::fs::remove_file(&self.path).unwrap();
        received
    }
}

fn assert_commands(received: Vec<Value>, fixtures: &[&str]) {
    assert_eq!(received.len(), fixtures.len());
    for (command, name) in received.into_iter().zip(fixtures) {
        assert_eq!(
            command,
            fixture(&format!("commands/{name}.json")),
            "{name} does not match its wire fixture"
        );
    }
}

#[test]
fn network_commands_and_replies_match_wire_fixtures() {
    let socket = WireSocket::serve(&[
        "replies/ok.json",
        "replies/run_result-succeeded.json",
        "replies/enqueue_run.json",
        "replies/ok.json",
        "replies/run_result-dead.json",
        "replies/ok-null.json",
    ]);
    let client = socket.client();

    client
        .call_blocking("users.lookup", json!({"id": "u1"}))
        .unwrap();
    let result = client
        .result_blocking("mail.send-email", "run-1")
        .unwrap()
        .unwrap();
    assert_eq!(
        (result.status.as_str(), &result.result, result.error),
        ("succeeded", &json!({"sent": true}), None)
    );
    let enqueued = client
        .enqueue_blocking(
            "mail.send-email",
            json!({"to": "a@example.com"}),
            EnqueueOptions {
                run_at_ms: Some(AT_MS),
                max_attempts: Some(5),
                id: Some("order-42".into()),
            },
        )
        .unwrap();
    assert_eq!(
        (enqueued.id.as_str(), enqueued.deduplicated),
        ("order-42", true)
    );
    client
        .publish_blocking(
            "chat.messages",
            json!({"room": "lobby"}),
            "message",
            json!({"text": "hi"}),
        )
        .unwrap();
    let dead = client
        .result_blocking("mail.send-email", "run-1")
        .unwrap()
        .unwrap();
    assert_eq!(
        (dead.status.as_str(), &dead.result, dead.error.as_deref()),
        ("dead", &Value::Null, Some("smtp timeout"))
    );
    assert!(
        client
            .result_blocking("mail.send-email", "run-1")
            .unwrap()
            .is_none()
    );

    assert_commands(
        socket.received(),
        &[
            "network_call-call",
            "network_call-result",
            "network_call-enqueue",
            "network_call-publish",
            "network_call-result",
            "network_call-result",
        ],
    );
}

#[test]
fn store_invalidation_matches_wire_fixture() {
    let socket = WireSocket::serve(&["replies/ok.json"]);
    let store = StoreRegistry::new()
        .register(
            "messages",
            StoreDefinition::new(|_| async { Ok(Value::Null) }),
        )
        .unwrap();
    store
        .invalidate_at(
            socket.path(),
            APP,
            json!({"chatId": "chat-a"}),
            Duration::from_secs(5),
        )
        .unwrap();
    assert_commands(socket.received(), &["channel_publish-store_invalidate"]);
}

#[test]
fn store_invalidation_with_build_matches_wire_fixture() {
    if std::env::var("TAKO_BUILD").as_deref() != Ok("v7") {
        assert!(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "wire_fixtures::store_invalidation_with_build_matches_wire_fixture"
                ])
                .env("TAKO_BUILD", "v7")
                .status()
                .unwrap()
                .success()
        );
        return;
    }
    let socket = WireSocket::serve(&["replies/ok.json"]);
    StoreRegistry::new()
        .register(
            "messages",
            StoreDefinition::new(|_| async { Ok(Value::Null) }),
        )
        .unwrap()
        .invalidate_at(
            socket.path(),
            APP,
            json!({"chatId":"chat-a"}),
            Duration::from_secs(5),
        )
        .unwrap();
    assert_commands(
        socket.received(),
        &["channel_publish-store_invalidate-build"],
    );
}

#[test]
fn error_reply_fixtures_map_to_error_codes() {
    let expected = [
        ("error.json", None),
        ("error-unknown_code.json", None),
        ("error-run_not_owned.json", Some(ErrorCode::RunNotOwned)),
        ("error-run_id_conflict.json", Some(ErrorCode::RunIdConflict)),
        (
            "error-invalid_request.json",
            Some(ErrorCode::InvalidRequest),
        ),
        (
            "error-channel_unknown.json",
            Some(ErrorCode::ChannelUnknown),
        ),
        (
            "error-channel_params_invalid.json",
            Some(ErrorCode::ChannelParamsInvalid),
        ),
        ("error-channel_closed.json", Some(ErrorCode::ChannelClosed)),
        ("error-unavailable.json", Some(ErrorCode::Unavailable)),
    ];
    let mut present: Vec<String> = std::fs::read_dir(wire_dir().join("replies"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("error"))
        .collect();
    present.sort();
    let mut known: Vec<String> = expected.iter().map(|(name, _)| name.to_string()).collect();
    known.sort();
    assert_eq!(present, known, "every error fixture needs an expected code");

    let store = StoreRegistry::new()
        .register(
            "messages",
            StoreDefinition::new(|_| async { Ok(Value::Null) }),
        )
        .unwrap();
    for (name, code) in expected {
        let reply = format!("replies/{name}");
        let socket = WireSocket::serve(&[&reply, &reply]);
        let network = socket
            .client()
            .call_blocking("users.lookup", Value::Null)
            .unwrap_err();
        assert_eq!(ErrorCode::of(&network), code, "{name} over a network call");
        let invalidation = store
            .invalidate_at(socket.path(), APP, json!({}), Duration::from_secs(5))
            .unwrap_err();
        assert!(
            matches!(invalidation, StoreError::Rejected(rejected) if rejected == code),
            "{name} over a store invalidation: {invalidation:?}"
        );
        socket.received();
    }
}

#[test]
fn bootstrap_fixture_parses() {
    let bootstrap = crate::parse_bootstrap_data(&fixture_text("bootstrap.json")).unwrap();
    assert_eq!(bootstrap.token(), Some("tok-abc"));
    assert_eq!(bootstrap.secret("DATABASE_URL"), Some("postgres://db/app"));
    assert_eq!(bootstrap.secret("API_KEY"), Some("key-123"));
    assert_eq!(bootstrap.secrets().len(), 2);
    let storages: Value = serde_json::to_value(bootstrap.storages()).unwrap();
    assert_eq!(storages, fixture("bootstrap.json")["storages"]);
}
