use crate::rpc::{Client, Run, ScheduleSpec};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{collections::HashMap, time::Duration};

pub type Handler = Box<dyn Fn(&mut WorkflowContext, Value) -> WorkflowResult + Send + Sync>;
pub type WorkflowResult = Result<(), WorkflowError>;

pub struct Worker {
    client: Client,
    handlers: HashMap<String, WorkflowRegistration>,
    worker_id: String,
    lease_ms: u64,
    base_backoff: Duration,
    max_backoff: Duration,
}

impl Worker {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            handlers: HashMap::new(),
            worker_id: format!("worker-{}", std::process::id()),
            lease_ms: 60_000,
            base_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(3600),
        }
    }

    pub fn from_env() -> Result<Self, crate::rpc::Error> {
        Ok(Self::new(Client::from_env()?))
    }

    pub fn with_worker_id(mut self, worker_id: impl Into<String>) -> Self {
        self.worker_id = worker_id.into();
        self
    }

    pub fn register<F>(&mut self, name: impl Into<String>, handler: F) -> Result<(), Error>
    where
        F: Fn(&mut WorkflowContext, Value) -> WorkflowResult + Send + Sync + 'static,
    {
        self.register_with_options(name, WorkflowOptions::default(), handler)
    }

    pub fn register_with_options<F>(
        &mut self,
        name: impl Into<String>,
        options: WorkflowOptions,
        handler: F,
    ) -> Result<(), Error>
    where
        F: Fn(&mut WorkflowContext, Value) -> WorkflowResult + Send + Sync + 'static,
    {
        let name = name.into();
        if self.handlers.contains_key(&name) {
            return Err(Error::DuplicateWorkflow(name));
        }
        self.handlers.insert(
            name,
            WorkflowRegistration {
                handler: Box::new(handler),
                options,
            },
        );
        Ok(())
    }

    pub fn register_schedules(&self) -> Result<(), Error> {
        let schedules: Vec<_> = self
            .handlers
            .iter()
            .filter_map(|(name, registration)| {
                registration
                    .options
                    .schedule
                    .as_ref()
                    .map(|cron| ScheduleSpec {
                        name: name.clone(),
                        cron: cron.clone(),
                    })
            })
            .collect();
        if !schedules.is_empty() {
            self.client.register_schedules(&schedules)?;
        }
        Ok(())
    }

    pub fn run_once(&self) -> Result<bool, Error> {
        if self.handlers.is_empty() {
            return Err(Error::NoWorkflows);
        }
        let names = self.handlers.keys().cloned().collect::<Vec<_>>();
        let Some(run) = self.client.claim(&self.worker_id, &names, self.lease_ms)? else {
            return Ok(false);
        };
        self.execute(run)?;
        Ok(true)
    }

    fn execute(&self, run: Run) -> Result<(), Error> {
        let Some(registration) = self.handlers.get(&run.name) else {
            self.client.fail(
                &run.id,
                &self.worker_id,
                format!("no handler registered for {:?}", run.name),
                None,
                true,
            )?;
            return Ok(());
        };

        let mut context = WorkflowContext {
            run_id: run.id.clone(),
            workflow_name: run.name.clone(),
            attempts: run.attempts,
            step: StepApi {
                client: self.client.clone(),
                run_id: run.id.clone(),
                worker_id: self.worker_id.clone(),
                state: run.step_state,
            },
        };

        match (registration.handler)(&mut context, run.payload) {
            Ok(()) => self.client.complete(&run.id, &self.worker_id)?,
            Err(WorkflowError::Bail(reason)) => {
                self.client
                    .cancel(&run.id, &self.worker_id, reason.as_deref())?;
            }
            Err(WorkflowError::Fail(error)) => {
                self.client
                    .fail(&run.id, &self.worker_id, error, None, true)?;
            }
            Err(WorkflowError::Defer { wake_at_ms }) => {
                self.client
                    .defer_run(&run.id, &self.worker_id, wake_at_ms)?;
            }
            Err(WorkflowError::WaitForEvent {
                step_name,
                event_name,
                timeout_at_ms,
            }) => {
                self.client.wait_for_event(
                    &run.id,
                    &self.worker_id,
                    step_name,
                    event_name,
                    timeout_at_ms,
                )?;
            }
            Err(WorkflowError::Task(error)) => {
                let max_attempts = registration
                    .options
                    .max_attempts
                    .unwrap_or(run.max_attempts)
                    .max(1);
                let finalize = run.attempts >= max_attempts;
                let next_run_at_ms = if finalize {
                    None
                } else {
                    Some(now_ms() + backoff_ms(run.attempts, self.base_backoff, self.max_backoff))
                };
                self.client
                    .fail(&run.id, &self.worker_id, error, next_run_at_ms, finalize)?;
            }
            Err(WorkflowError::Rpc(error)) => return Err(Error::Rpc(error)),
            Err(WorkflowError::Json(error)) => return Err(Error::Json(error)),
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
pub struct WorkflowOptions {
    pub max_attempts: Option<u32>,
    pub schedule: Option<String>,
}

struct WorkflowRegistration {
    handler: Handler,
    options: WorkflowOptions,
}

pub struct WorkflowContext {
    pub run_id: String,
    pub workflow_name: String,
    pub attempts: u32,
    pub step: StepApi,
}

impl WorkflowContext {
    pub fn bail(&self, reason: impl Into<String>) -> WorkflowError {
        WorkflowError::Bail(Some(reason.into()))
    }

    pub fn fail(&self, error: impl Into<String>) -> WorkflowError {
        WorkflowError::Fail(error.into())
    }
}

pub struct StepApi {
    client: Client,
    run_id: String,
    worker_id: String,
    state: serde_json::Map<String, Value>,
}

impl StepApi {
    pub fn run<T, F>(&mut self, name: &str, f: F) -> Result<T, WorkflowError>
    where
        T: Serialize + DeserializeOwned,
        F: FnOnce() -> Result<T, WorkflowError>,
    {
        if let Some(cached) = self.state.get(name) {
            return Ok(serde_json::from_value(cached.clone())?);
        }
        let value = f()?;
        let json_value = serde_json::to_value(&value)?;
        self.client
            .save_step(&self.run_id, &self.worker_id, name, &json_value)?;
        self.state.insert(name.to_string(), json_value);
        Ok(value)
    }

    pub fn sleep(&mut self, name: &str, duration: Duration) -> Result<(), WorkflowError> {
        let key = format!("__sleep:{name}");
        if self.state.contains_key(name) {
            return Ok(());
        }
        let wake_at_ms = now_ms() + duration.as_millis() as i64;
        self.client.save_step(
            &self.run_id,
            &self.worker_id,
            &key,
            serde_json::json!({ "wakeAt": wake_at_ms }),
        )?;
        Err(WorkflowError::Defer {
            wake_at_ms: Some(wake_at_ms),
        })
    }

    pub fn wait_for(&self, name: impl Into<String>, timeout_at_ms: Option<i64>) -> WorkflowError {
        let name = name.into();
        WorkflowError::WaitForEvent {
            step_name: name.clone(),
            event_name: name,
            timeout_at_ms,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WorkflowError {
    #[error("task failed: {0}")]
    Task(String),
    #[error("workflow bailed")]
    Bail(Option<String>),
    #[error("workflow failed: {0}")]
    Fail(String),
    #[error("workflow deferred")]
    Defer { wake_at_ms: Option<i64> },
    #[error("workflow waiting for event")]
    WaitForEvent {
        step_name: String,
        event_name: String,
        timeout_at_ms: Option<i64>,
    },
    #[error(transparent)]
    Rpc(#[from] crate::rpc::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no workflows registered")]
    NoWorkflows,
    #[error("workflow {0:?} is already registered")]
    DuplicateWorkflow(String),
    #[error(transparent)]
    Rpc(#[from] crate::rpc::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn backoff_ms(attempts: u32, base: Duration, max: Duration) -> i64 {
    let exponent = attempts.saturating_sub(1).min(30);
    let factor = 2_u128.pow(exponent);
    let raw = base.as_millis().saturating_mul(factor);
    raw.min(max.as_millis()) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::{
        collections::VecDeque,
        io::{BufRead, BufReader, Write},
        os::unix::net::UnixListener,
        path::PathBuf,
        sync::{Arc, Mutex},
        thread,
    };
    use tempfile::TempDir;

    #[test]
    fn worker_registers_schedules_claims_and_completes() {
        let server = MockWorkflowSocket::new(vec![
            json!({"status":"ok","data":{"count":1}}),
            json!({"status":"ok","data":{
                "id":"run-1",
                "name":"send-email",
                "payload":{"to":"a@example.com"},
                "status":"running",
                "attempts":1,
                "max_attempts":3,
                "run_at_ms":1,
                "step_state":{}
            }}),
            json!({"status":"ok","data":{}}),
        ]);
        let mut worker = Worker::new(Client::new(server.path(), "demo")).with_worker_id("w1");
        worker
            .register_with_options(
                "send-email",
                WorkflowOptions {
                    max_attempts: None,
                    schedule: Some("0 9 * * *".to_string()),
                },
                |ctx, payload| {
                    assert_eq!(ctx.run_id, "run-1");
                    assert_eq!(payload["to"], "a@example.com");
                    Ok(())
                },
            )
            .unwrap();

        worker.register_schedules().unwrap();
        assert!(worker.run_once().unwrap());

        let commands = server.commands();
        assert_eq!(commands[0]["command"], "register_schedules");
        assert_eq!(commands[1]["command"], "claim_run");
        assert_eq!(commands[2]["command"], "complete_run");
    }

    #[test]
    fn step_run_persists_result_once() {
        let server = MockWorkflowSocket::new(vec![
            json!({"status":"ok","data":{
                "id":"run-1",
                "name":"job",
                "payload":{},
                "status":"running",
                "attempts":1,
                "max_attempts":3,
                "run_at_ms":1,
                "step_state":{}
            }}),
            json!({"status":"ok","data":{}}),
            json!({"status":"ok","data":{}}),
        ]);
        let mut worker = Worker::new(Client::new(server.path(), "demo")).with_worker_id("w1");
        worker
            .register("job", |ctx, _| {
                let value: String = ctx
                    .step
                    .run("expensive", || Ok("cached".to_string()))
                    .unwrap();
                assert_eq!(value, "cached");
                Ok(())
            })
            .unwrap();

        worker.run_once().unwrap();

        let commands = server.commands();
        assert_eq!(commands[1]["command"], "save_step");
        assert_eq!(commands[1]["step_name"], "expensive");
        assert_eq!(commands[1]["result"], "cached");
    }

    #[test]
    fn bail_maps_to_cancel() {
        let server = MockWorkflowSocket::new(vec![
            json!({"status":"ok","data":{
                "id":"run-1",
                "name":"job",
                "payload":{},
                "status":"running",
                "attempts":1,
                "max_attempts":3,
                "run_at_ms":1,
                "step_state":{}
            }}),
            json!({"status":"ok","data":{}}),
        ]);
        let mut worker = Worker::new(Client::new(server.path(), "demo")).with_worker_id("w1");
        worker
            .register("job", |ctx, _| Err(ctx.bail("not needed")))
            .unwrap();

        worker.run_once().unwrap();

        let commands = server.commands();
        assert_eq!(commands[1]["command"], "cancel_run");
        assert_eq!(commands[1]["reason"], "not needed");
    }

    struct MockWorkflowSocket {
        _dir: TempDir,
        path: PathBuf,
        commands: Arc<Mutex<Vec<Value>>>,
    }

    impl MockWorkflowSocket {
        fn new(responses: Vec<Value>) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("workflow.sock");
            let listener = UnixListener::bind(&path).unwrap();
            let commands = Arc::new(Mutex::new(Vec::new()));
            let thread_commands = Arc::clone(&commands);
            let responses = Arc::new(Mutex::new(VecDeque::from(responses)));
            let thread_responses = Arc::clone(&responses);
            thread::spawn(move || {
                for stream in listener.incoming() {
                    let mut stream = stream.unwrap();
                    let mut line = String::new();
                    {
                        let mut reader = BufReader::new(&stream);
                        reader.read_line(&mut line).unwrap();
                    }
                    let cmd: Value = serde_json::from_str(&line).unwrap();
                    thread_commands.lock().unwrap().push(cmd);
                    let response = thread_responses
                        .lock()
                        .unwrap()
                        .pop_front()
                        .unwrap_or_else(|| json!({"status":"ok","data":{}}));
                    serde_json::to_writer(&mut stream, &response).unwrap();
                    stream.write_all(b"\n").unwrap();
                    if thread_responses.lock().unwrap().is_empty() {
                        break;
                    }
                }
            });
            Self {
                _dir: dir,
                path,
                commands,
            }
        }

        fn path(&self) -> PathBuf {
            self.path.clone()
        }

        fn commands(&self) -> Vec<Value> {
            self.commands.lock().unwrap().clone()
        }
    }
}
