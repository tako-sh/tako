use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    time::Duration,
};

pub const INTERNAL_SOCKET_ENV: &str = "TAKO_INTERNAL_SOCKET";
pub const WORKFLOW_SOCKET_ENV: &str = "TAKO_WORKFLOW_SOCKET";
pub const APP_NAME_ENV: &str = "TAKO_APP_NAME";
const DEFAULT_RPC_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_DIAL_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct Client {
    socket_path: PathBuf,
    app: String,
    timeout: Duration,
}

impl Client {
    pub fn new(socket_path: impl Into<PathBuf>, app: impl Into<String>) -> Self {
        Self {
            socket_path: socket_path.into(),
            app: app.into(),
            timeout: DEFAULT_RPC_TIMEOUT,
        }
    }

    pub fn from_env() -> Result<Self, Error> {
        let socket = std::env::var(INTERNAL_SOCKET_ENV)
            .or_else(|_| std::env::var(WORKFLOW_SOCKET_ENV))
            .ok();
        let app = std::env::var(APP_NAME_ENV).ok();
        match (socket, app) {
            (Some(socket), Some(app)) => Ok(Self::new(socket, app)),
            (Some(_), None) => Err(Error::MissingEnv(APP_NAME_ENV)),
            (None, Some(_)) => Err(Error::MissingEnv(INTERNAL_SOCKET_ENV)),
            (None, None) => Err(Error::MissingEnv(INTERNAL_SOCKET_ENV)),
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn app_name(&self) -> &str {
        &self.app
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub fn call<T: DeserializeOwned>(&self, command: impl Serialize) -> Result<T, Error> {
        let data = self.call_value(command)?;
        Ok(serde_json::from_value(data)?)
    }

    pub fn call_value(&self, command: impl Serialize) -> Result<Value, Error> {
        let mut stream =
            UnixStream::connect(&self.socket_path).map_err(|source| Error::Dial { source })?;
        stream.set_read_timeout(Some(self.timeout))?;
        stream.set_write_timeout(Some(DEFAULT_DIAL_TIMEOUT))?;

        serde_json::to_writer(&mut stream, &command)?;
        stream.write_all(b"\n")?;
        stream.flush()?;

        let mut line = String::new();
        let mut reader = BufReader::new(stream);
        reader.read_line(&mut line)?;
        if line.trim().is_empty() {
            return Err(Error::EmptyResponse);
        }
        let response: WireResponse = serde_json::from_str(&line)?;
        if response.status != "ok" {
            return Err(Error::Rpc(
                response.message.unwrap_or_else(|| "rpc failed".to_string()),
            ));
        }
        Ok(response.data.unwrap_or(Value::Null))
    }

    pub fn enqueue<P: Serialize>(
        &self,
        name: impl Into<String>,
        payload: P,
        opts: EnqueueOpts,
    ) -> Result<EnqueueResult, Error> {
        self.call(json!({
            "command": "enqueue_run",
            "app": self.app,
            "name": name.into(),
            "payload": payload,
            "opts": opts,
        }))
    }

    pub fn register_schedules(&self, schedules: &[ScheduleSpec]) -> Result<(), Error> {
        self.call_value(json!({
            "command": "register_schedules",
            "app": self.app,
            "schedules": schedules,
        }))?;
        Ok(())
    }

    pub fn claim(
        &self,
        worker_id: impl Into<String>,
        names: &[String],
        lease_ms: u64,
    ) -> Result<Option<Run>, Error> {
        let data = self.call_value(json!({
            "command": "claim_run",
            "app": self.app,
            "worker_id": worker_id.into(),
            "names": names,
            "lease_ms": lease_ms,
        }))?;
        if data.is_null() {
            return Ok(None);
        }
        Ok(Some(serde_json::from_value(data)?))
    }

    pub fn heartbeat(
        &self,
        id: impl Into<String>,
        worker_id: impl Into<String>,
        lease_ms: u64,
    ) -> Result<(), Error> {
        self.call_value(json!({
            "command": "heartbeat_run",
            "app": self.app,
            "id": id.into(),
            "worker_id": worker_id.into(),
            "lease_ms": lease_ms,
        }))?;
        Ok(())
    }

    pub fn save_step<P: Serialize>(
        &self,
        id: impl Into<String>,
        worker_id: impl Into<String>,
        step_name: impl Into<String>,
        result: P,
    ) -> Result<(), Error> {
        self.call_value(json!({
            "command": "save_step",
            "app": self.app,
            "id": id.into(),
            "worker_id": worker_id.into(),
            "step_name": step_name.into(),
            "result": result,
        }))?;
        Ok(())
    }

    pub fn complete(
        &self,
        id: impl Into<String>,
        worker_id: impl Into<String>,
    ) -> Result<(), Error> {
        self.call_value(json!({
            "command": "complete_run",
            "app": self.app,
            "id": id.into(),
            "worker_id": worker_id.into(),
        }))?;
        Ok(())
    }

    pub fn cancel(
        &self,
        id: impl Into<String>,
        worker_id: impl Into<String>,
        reason: Option<&str>,
    ) -> Result<(), Error> {
        self.call_value(json!({
            "command": "cancel_run",
            "app": self.app,
            "id": id.into(),
            "worker_id": worker_id.into(),
            "reason": reason,
        }))?;
        Ok(())
    }

    pub fn defer_run(
        &self,
        id: impl Into<String>,
        worker_id: impl Into<String>,
        wake_at_ms: Option<i64>,
    ) -> Result<(), Error> {
        self.call_value(json!({
            "command": "defer_run",
            "app": self.app,
            "id": id.into(),
            "worker_id": worker_id.into(),
            "wake_at_ms": wake_at_ms,
        }))?;
        Ok(())
    }

    pub fn wait_for_event(
        &self,
        id: impl Into<String>,
        worker_id: impl Into<String>,
        step_name: impl Into<String>,
        event_name: impl Into<String>,
        timeout_at_ms: Option<i64>,
    ) -> Result<(), Error> {
        self.call_value(json!({
            "command": "wait_for_event",
            "app": self.app,
            "id": id.into(),
            "worker_id": worker_id.into(),
            "step_name": step_name.into(),
            "event_name": event_name.into(),
            "timeout_at_ms": timeout_at_ms,
        }))?;
        Ok(())
    }

    pub fn signal<P: Serialize>(
        &self,
        event_name: impl Into<String>,
        payload: P,
    ) -> Result<u64, Error> {
        let response: SignalResponse = self.call(json!({
            "command": "signal",
            "app": self.app,
            "event_name": event_name.into(),
            "payload": payload,
        }))?;
        Ok(response.woken)
    }

    pub fn fail(
        &self,
        id: impl Into<String>,
        worker_id: impl Into<String>,
        error: impl Into<String>,
        next_run_at_ms: Option<i64>,
        finalize: bool,
    ) -> Result<(), Error> {
        self.call_value(json!({
            "command": "fail_run",
            "app": self.app,
            "id": id.into(),
            "worker_id": worker_id.into(),
            "error": error.into(),
            "next_run_at_ms": next_run_at_ms,
            "finalize": finalize,
        }))?;
        Ok(())
    }

    pub fn publish_channel<P: Serialize>(
        &self,
        channel: impl Into<String>,
        payload: P,
    ) -> Result<Value, Error> {
        self.call_value(json!({
            "command": "channel_publish",
            "app": self.app,
            "channel": channel.into(),
            "payload": payload,
        }))
    }
}

pub fn enqueue<P: Serialize>(
    name: impl Into<String>,
    payload: P,
    opts: EnqueueOpts,
) -> Result<EnqueueResult, Error> {
    Client::from_env()?.enqueue(name, payload, opts)
}

pub fn signal<P: Serialize>(event_name: impl Into<String>, payload: P) -> Result<u64, Error> {
    Client::from_env()?.signal(event_name, payload)
}

pub fn publish_channel<P: Serialize>(
    channel: impl Into<String>,
    payload: P,
) -> Result<Value, Error> {
    Client::from_env()?.publish_channel(channel, payload)
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct EnqueueOpts {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_at_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unique_key: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct EnqueueResult {
    pub id: String,
    #[serde(default)]
    pub deduplicated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScheduleSpec {
    pub name: String,
    pub cron: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Run {
    pub id: String,
    pub name: String,
    pub payload: Value,
    pub status: String,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub max_attempts: u32,
    #[serde(default)]
    pub run_at_ms: i64,
    #[serde(default)]
    pub step_state: serde_json::Map<String, Value>,
}

#[derive(Debug, Deserialize)]
struct WireResponse {
    status: String,
    data: Option<Value>,
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SignalResponse {
    #[serde(default)]
    woken: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("missing required Tako env var {0}")]
    MissingEnv(&'static str),
    #[error("failed to connect to Tako internal socket")]
    Dial { source: std::io::Error },
    #[error("Tako RPC failed: {0}")]
    Rpc(String),
    #[error("empty response from Tako internal socket")]
    EmptyResponse,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{
        io::{BufRead, BufReader, Write},
        os::unix::net::UnixListener,
        sync::{Arc, Mutex},
        thread,
    };
    use tempfile::TempDir;

    #[test]
    fn enqueue_sends_app_payload_and_opts() {
        let server = TestSocket::start(|cmd| {
            assert_eq!(cmd["command"], "enqueue_run");
            assert_eq!(cmd["app"], "demo");
            assert_eq!(cmd["name"], "email");
            assert_eq!(cmd["payload"]["to"], "a@example.com");
            assert_eq!(cmd["opts"]["max_attempts"], 4);
            json!({"status":"ok","data":{"id":"run-1","deduplicated":false}})
        });

        let client = Client::new(server.path(), "demo");
        let result = client
            .enqueue(
                "email",
                json!({"to":"a@example.com"}),
                EnqueueOpts {
                    max_attempts: Some(4),
                    ..EnqueueOpts::default()
                },
            )
            .unwrap();

        assert_eq!(result.id, "run-1");
        assert!(!result.deduplicated);
    }

    #[test]
    fn claim_returns_none_for_null_data() {
        let server = TestSocket::start(|cmd| {
            assert_eq!(cmd["command"], "claim_run");
            json!({"status":"ok","data":null})
        });
        let client = Client::new(server.path(), "demo");

        let run = client
            .claim("worker-1", &["send-email".to_string()], 60_000)
            .unwrap();

        assert_eq!(run, None);
    }

    #[test]
    fn publish_channel_uses_internal_socket_command() {
        let server = TestSocket::start(|cmd| {
            assert_eq!(cmd["command"], "channel_publish");
            assert_eq!(cmd["channel"], "chat");
            json!({"status":"ok","data":{"id":"msg-1","payload":{"text":"hi"}}})
        });
        let client = Client::new(server.path(), "demo");

        let message = client
            .publish_channel("chat", json!({"text":"hi"}))
            .unwrap();

        assert_eq!(message["id"], "msg-1");
        assert_eq!(message["payload"]["text"], "hi");
    }

    #[test]
    fn rpc_error_response_is_returned() {
        let server = TestSocket::start(
            |_| json!({"status":"error","message":"workflow runtime is draining"}),
        );
        let client = Client::new(server.path(), "demo");

        let err = client.signal("ready", json!({})).unwrap_err();

        assert!(matches!(err, Error::Rpc(message) if message == "workflow runtime is draining"));
    }

    struct TestSocket {
        _dir: TempDir,
        path: PathBuf,
        _commands: Arc<Mutex<Vec<Value>>>,
    }

    impl TestSocket {
        fn start(handler: impl Fn(Value) -> Value + Send + Sync + 'static) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("rpc.sock");
            let listener = UnixListener::bind(&path).unwrap();
            let commands = Arc::new(Mutex::new(Vec::new()));
            let commands_for_thread = Arc::clone(&commands);
            let handler = Arc::new(handler);
            thread::spawn(move || {
                for stream in listener.incoming().take(1) {
                    let mut stream = stream.unwrap();
                    let mut line = String::new();
                    {
                        let mut reader = BufReader::new(&stream);
                        reader.read_line(&mut line).unwrap();
                    }
                    let cmd: Value = serde_json::from_str(&line).unwrap();
                    commands_for_thread.lock().unwrap().push(cmd.clone());
                    let response = handler(cmd);
                    serde_json::to_writer(&mut stream, &response).unwrap();
                    stream.write_all(b"\n").unwrap();
                }
            });
            Self {
                _dir: dir,
                path,
                _commands: commands,
            }
        }

        fn path(&self) -> PathBuf {
            self.path.clone()
        }
    }
}
