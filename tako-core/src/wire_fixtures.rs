//! Owner of the internal-socket wire fixtures in `test-support/wire/`.
//!
//! Every command an SDK sends on the internal socket, every reply it parses,
//! and the bootstrap envelope are built here from the typed values the server
//! reads and writes. Each must serialize to its checked-in fixture and the
//! fixture must deserialize back to the same value. The JS, Go and Rust SDK
//! tests load the same files and pin their request builders and reply parsers
//! against them. Comparisons use parsed JSON, so `bun run fmt` may reformat
//! the files. Regenerate them with
//! `TAKO_UPDATE_WIRE_FIXTURES=1 cargo test -p tako-core --lib wire_fixtures`.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::bootstrap::envelope_bytes;
use crate::network::NetworkRequest;
use crate::{
    Command, EnqueueOpts, EnqueueRunResponse, ErrorCode, Response, RunPayload, RunResult,
    ScheduleSpec, StorageBinding, StorageProvider,
};

const FIXTURE_DIR: &str = "test-support/wire";
const UPDATE_ENV: &str = "TAKO_UPDATE_WIRE_FIXTURES";

const APP: &str = "demo";
const WORKFLOW: &str = "send-email";
const RUN_ID: &str = "run-1";
/// The claim owner `claim_run` returns; worker writes echo it as `worker_id`.
const CLAIM: &str = "claim-7";
/// 2026-01-01T00:00:00Z.
const AT_MS: i64 = 1_767_225_600_000;

struct Fixture {
    path: String,
    json: Value,
    /// Deserializes the checked-in file and asserts it is the same value.
    round_trip: Box<dyn Fn(&Value)>,
}

/// A fixture owned by a typed value: the file deserializes into `T` and
/// serializes back to the same JSON.
fn typed<T: Serialize + DeserializeOwned + 'static>(path: &str, value: T) -> Fixture {
    let json = serde_json::to_value(&value).expect("fixture value serializes");
    let path = path.to_string();
    let label = path.clone();
    Fixture {
        path,
        json,
        round_trip: Box::new(move |file| {
            let parsed: T = serde_json::from_value(file.clone())
                .unwrap_or_else(|error| panic!("{label} does not deserialize: {error}"));
            assert_eq!(
                &serde_json::to_value(&parsed).unwrap(),
                file,
                "{label} does not deserialize to the value that produced it"
            );
        }),
    }
}

fn command(name: &str, command: Command) -> Fixture {
    typed(&format!("commands/{name}.json"), command)
}

fn reply(name: &str, response: Response) -> Fixture {
    typed(&format!("replies/{name}.json"), response)
}

fn schedules(schedules: Vec<ScheduleSpec>) -> Command {
    Command::RegisterSchedules {
        app: APP.into(),
        schedules,
    }
}

fn channel_lifecycle() -> Value {
    // The lifecycle a durable channel definition with default settings carries.
    json!({
        "replayWindowMs": 600_000,
        "inactivityTtlMs": 0,
        "keepaliveIntervalMs": 25_000,
        "maxConnectionLifetimeMs": 7_200_000,
    })
}

fn network(name: &str, address: &str, request: NetworkRequest) -> Fixture {
    command(
        name,
        Command::NetworkCall {
            app: APP.into(),
            address: address.into(),
            request,
        },
    )
}

fn commands() -> Vec<Fixture> {
    let all_opts = EnqueueOpts {
        run_at_ms: Some(AT_MS),
        max_attempts: Some(5),
        id: Some("order-42".into()),
    };
    vec![
        command(
            "enqueue_run",
            Command::EnqueueRun {
                app: APP.into(),
                name: WORKFLOW.into(),
                payload: json!({"to": "a@example.com"}),
                opts: all_opts.clone(),
            },
        ),
        command(
            "enqueue_run-defaults",
            Command::EnqueueRun {
                app: APP.into(),
                name: WORKFLOW.into(),
                payload: json!({"to": "a@example.com"}),
                opts: EnqueueOpts::default(),
            },
        ),
        command(
            "enqueue_run-null_payload",
            Command::EnqueueRun {
                app: APP.into(),
                name: WORKFLOW.into(),
                payload: Value::Null,
                opts: EnqueueOpts::default(),
            },
        ),
        command(
            "get_run_result",
            Command::GetRunResult {
                app: APP.into(),
                name: WORKFLOW.into(),
                id: "order-42".into(),
            },
        ),
        command(
            "register_schedules",
            schedules(vec![
                ScheduleSpec {
                    name: "nightly-report".into(),
                    cron: "0 3 * * *".into(),
                    max_attempts: Some(5),
                },
                ScheduleSpec {
                    name: "cleanup".into(),
                    cron: "*/15 * * * *".into(),
                    max_attempts: None,
                },
            ]),
        ),
        // A worker with no schedules still registers, so removed schedules stop.
        command("register_schedules-empty", schedules(Vec::new())),
        command(
            "claim_run",
            Command::ClaimRun {
                app: APP.into(),
                worker_id: "worker-1".into(),
                names: vec![WORKFLOW.into(), "nightly-report".into()],
                lease_ms: 30_000,
            },
        ),
        command(
            "heartbeat_run",
            Command::HeartbeatRun {
                app: APP.into(),
                id: RUN_ID.into(),
                worker_id: CLAIM.into(),
                lease_ms: 30_000,
            },
        ),
        command(
            "save_step",
            Command::SaveStep {
                app: APP.into(),
                id: RUN_ID.into(),
                worker_id: CLAIM.into(),
                step_name: "fetch-user".into(),
                result: json!({"id": "u1"}),
            },
        ),
        command(
            "complete_run",
            Command::CompleteRun {
                app: APP.into(),
                id: RUN_ID.into(),
                worker_id: CLAIM.into(),
                result: json!({"sent": true}),
            },
        ),
        command(
            "cancel_run",
            Command::CancelRun {
                app: APP.into(),
                id: RUN_ID.into(),
                worker_id: CLAIM.into(),
                reason: Some("customer asked".into()),
            },
        ),
        command(
            "cancel_run-no_reason",
            Command::CancelRun {
                app: APP.into(),
                id: RUN_ID.into(),
                worker_id: CLAIM.into(),
                reason: None,
            },
        ),
        command(
            "fail_run-retry",
            Command::FailRun {
                app: APP.into(),
                id: RUN_ID.into(),
                worker_id: CLAIM.into(),
                error: "smtp timeout".into(),
                next_run_at_ms: Some(AT_MS + 60_000),
                finalize: false,
            },
        ),
        command(
            "fail_run-finalize",
            Command::FailRun {
                app: APP.into(),
                id: RUN_ID.into(),
                worker_id: CLAIM.into(),
                error: "smtp timeout".into(),
                next_run_at_ms: None,
                finalize: true,
            },
        ),
        command(
            "defer_run",
            Command::DeferRun {
                app: APP.into(),
                id: RUN_ID.into(),
                worker_id: CLAIM.into(),
                wake_at_ms: Some(AT_MS + 3_600_000),
            },
        ),
        command(
            "defer_run-indefinite",
            Command::DeferRun {
                app: APP.into(),
                id: RUN_ID.into(),
                worker_id: CLAIM.into(),
                wake_at_ms: None,
            },
        ),
        command(
            "wait_for_event",
            Command::WaitForEvent {
                app: APP.into(),
                id: RUN_ID.into(),
                worker_id: CLAIM.into(),
                step_name: "approval".into(),
                event_name: "order-approved".into(),
                timeout_at_ms: Some(AT_MS + 86_400_000),
            },
        ),
        command(
            "wait_for_event-no_timeout",
            Command::WaitForEvent {
                app: APP.into(),
                id: RUN_ID.into(),
                worker_id: CLAIM.into(),
                step_name: "approval".into(),
                event_name: "order-approved".into(),
                timeout_at_ms: None,
            },
        ),
        command(
            "signal",
            Command::Signal {
                app: APP.into(),
                event_name: "order-approved".into(),
                payload: json!({"orderId": "42"}),
            },
        ),
        command(
            "channel_publish",
            Command::ChannelPublish {
                app: APP.into(),
                channel: "board".into(),
                params: json!({"base": "north", "page": 2}),
                build: Some("v7".into()),
                payload: json!({
                    "type": "message",
                    "data": {"text": "hi"},
                    "lifecycle": channel_lifecycle(),
                }),
            },
        ),
        // Stores publish on their reserved channel; the scope travels in the
        // payload. Invalidations preserve the subscriber's lifecycle. Build
        // metadata is optional, but when set validates the publisher's registry.
        command(
            "channel_publish-store_invalidate",
            Command::ChannelPublish {
                app: APP.into(),
                channel: "tako.store:messages".into(),
                params: Value::Null,
                build: None,
                payload: json!({
                    "type": "tako.store.invalidate",
                    "data": {},
                    "scopeParams": {"chatId": "chat-a"},
                }),
            },
        ),
        command(
            "channel_publish-store_invalidate-build",
            Command::ChannelPublish {
                app: APP.into(),
                channel: "tako.store:messages".into(),
                params: Value::Null,
                build: Some("v7".into()),
                payload: json!({"type":"tako.store.invalidate", "data":{}, "scopeParams":{"chatId":"chat-a"}}),
            },
        ),
        command(
            "channel_close",
            Command::ChannelClose {
                app: APP.into(),
                channel: "reply".into(),
                params: json!({"requestId": "r1"}),
                build: Some("v7".into()),
                lifecycle: Some(channel_lifecycle()),
            },
        ),
        network(
            "network_call-call",
            "users.lookup",
            NetworkRequest::Call {
                input: json!({"id": "u1"}),
            },
        ),
        network(
            "network_call-result",
            "mail.send-email",
            NetworkRequest::Result { id: RUN_ID.into() },
        ),
        network(
            "network_call-enqueue",
            "mail.send-email",
            NetworkRequest::Enqueue {
                payload: json!({"to": "a@example.com"}),
                opts: all_opts,
            },
        ),
        network(
            "network_call-publish",
            "chat.messages",
            NetworkRequest::Publish {
                message: json!({"type": "message", "data": {"text": "hi"}}),
                params: json!({"room": "lobby"}),
            },
        ),
    ]
}

fn replies() -> Vec<Fixture> {
    let mut replies = vec![
        // Worker writes acknowledge with an empty object.
        reply("ok", Response::ok(json!({}))),
        // Nothing due on claim_run; no retained run on get_run_result.
        reply("ok-null", Response::ok(Value::Null)),
        reply(
            "error",
            Response::error("Internal storage operation failed"),
        ),
        reply(
            "claim_run",
            Response::ok(RunPayload {
                id: RUN_ID.into(),
                worker_id: CLAIM.into(),
                name: WORKFLOW.into(),
                payload: json!({"to": "a@example.com"}),
                status: "running".into(),
                attempts: 2,
                max_attempts: 5,
                run_at_ms: AT_MS,
                step_state: json!({"fetch-user": {"id": "u1"}}),
            }),
        ),
        reply(
            "enqueue_run",
            Response::ok(EnqueueRunResponse {
                id: "order-42".into(),
                deduplicated: true,
            }),
        ),
        reply(
            "run_result-succeeded",
            Response::ok(RunResult {
                status: "succeeded".into(),
                result: json!({"sent": true}),
                error: None,
            }),
        ),
        reply(
            "run_result-dead",
            Response::ok(RunResult {
                status: "dead".into(),
                result: Value::Null,
                error: Some("smtp timeout".into()),
            }),
        ),
        // The workflow socket builds the signal reply with `json!`.
        reply("signal", Response::ok(json!({"woken": 2}))),
    ];
    for code in ErrorCode::ALL {
        replies.push(reply(
            &format!("error-{}", code.name()),
            Response::coded_error(*code, format!("rejected: {}", code.name())),
        ));
    }
    // No server sends a code this build does not know, so this file is
    // written by hand: readers must treat the code as unclassified.
    replies.push(Fixture {
        path: "replies/error-unknown_code.json".into(),
        json: json!({
            "status": "error",
            "message": "rejected by a newer server",
            "code": "from_a_newer_server",
        }),
        round_trip: Box::new(|file| {
            let parsed: Response = serde_json::from_value(file.clone()).unwrap();
            assert_eq!(parsed.error_message(), Some("rejected by a newer server"));
            assert_eq!(parsed.error_code(), None);
        }),
    });
    replies
}

fn bootstrap() -> Fixture {
    let secrets = HashMap::from([
        ("DATABASE_URL".to_string(), "postgres://db/app".to_string()),
        ("API_KEY".to_string(), "key-123".to_string()),
    ]);
    let storages = HashMap::from([
        (
            "uploads".to_string(),
            StorageBinding {
                provider: StorageProvider::S3,
                bucket: Some("demo-uploads".into()),
                endpoint: Some("https://s3.example.com".into()),
                region: Some("auto".into()),
                access_key_id: Some("AKIAEXAMPLE".into()),
                secret_access_key: Some("secret-example".into()),
                force_path_style: true,
                public_base_url: Some("https://cdn.example.com".into()),
                path: None,
                signing_key: None,
            },
        ),
        (
            "files".to_string(),
            StorageBinding {
                provider: StorageProvider::Local,
                bucket: None,
                endpoint: None,
                region: None,
                access_key_id: None,
                secret_access_key: None,
                force_path_style: false,
                public_base_url: None,
                path: Some("/var/lib/tako/apps/demo/storage/files".into()),
                signing_key: Some("signing-example".into()),
            },
        ),
    ]);
    let json = serde_json::from_slice(&envelope_bytes("tok-abc", &secrets, &storages)).unwrap();
    Fixture {
        path: "bootstrap.json".into(),
        json,
        round_trip: Box::new(|file| {
            let token = file["token"].as_str().expect("token is a string");
            let secrets: HashMap<String, String> =
                serde_json::from_value(file["secrets"].clone()).unwrap();
            let storages: HashMap<String, StorageBinding> =
                serde_json::from_value(file["storages"].clone()).unwrap();
            let rebuilt: Value =
                serde_json::from_slice(&envelope_bytes(token, &secrets, &storages)).unwrap();
            assert_eq!(
                &rebuilt, file,
                "bootstrap.json is not the envelope it describes"
            );
        }),
    }
}

fn fixtures() -> Vec<Fixture> {
    let mut all = commands();
    all.extend(replies());
    all.push(bootstrap());
    all
}

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(FIXTURE_DIR)
}

fn checked_in_files(dir: &Path) -> BTreeSet<String> {
    fn walk(dir: &Path, root: &Path, out: &mut BTreeSet<String>) {
        for entry in std::fs::read_dir(dir).unwrap_or_else(|error| {
            panic!("read {}: {error}", dir.display());
        }) {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, root, out);
            } else if path.extension().is_some_and(|ext| ext == "json") {
                let relative = path.strip_prefix(root).unwrap();
                out.insert(relative.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let mut out = BTreeSet::new();
    walk(dir, dir, &mut out);
    out
}

#[test]
fn wire_fixtures_match_the_typed_protocol() {
    let dir = fixture_dir();
    let fixtures = fixtures();
    let expected: BTreeSet<String> = fixtures.iter().map(|f| f.path.clone()).collect();
    assert_eq!(expected.len(), fixtures.len(), "duplicate fixture path");
    let regenerate = format!("{UPDATE_ENV}=1 cargo test -p tako-core --lib wire_fixtures");

    if std::env::var_os(UPDATE_ENV).is_some() {
        for stale in checked_in_files(&dir).difference(&expected) {
            std::fs::remove_file(dir.join(stale)).unwrap();
        }
        for fixture in &fixtures {
            let path = dir.join(&fixture.path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let mut rendered = serde_json::to_string_pretty(&fixture.json).unwrap();
            rendered.push('\n');
            std::fs::write(&path, rendered).unwrap();
        }
    }

    let present = checked_in_files(&dir);
    let stale: Vec<_> = present.difference(&expected).collect();
    let missing: Vec<_> = expected.difference(&present).collect();
    assert!(
        stale.is_empty() && missing.is_empty(),
        "{FIXTURE_DIR} is out of date (stale: {stale:?}, missing: {missing:?}). Regenerate it with:\n  {regenerate}"
    );
    for fixture in &fixtures {
        let path = dir.join(&fixture.path);
        let content = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        let file: Value = serde_json::from_str(&content)
            .unwrap_or_else(|error| panic!("{} is not JSON: {error}", fixture.path));
        assert!(
            file == fixture.json,
            "{FIXTURE_DIR}/{} does not match the typed value that owns it.\n  checked in: {file}\n  expected:   {}\nRegenerate it with:\n  {regenerate}",
            fixture.path,
            fixture.json
        );
        (fixture.round_trip)(&file);
    }
}
