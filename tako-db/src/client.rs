//! A blocking client for statements Tako itself runs against a hosted
//! database, such as channel replay and workflow runs.
//!
//! It reaches the host through the server's local authenticated endpoint, so
//! even the database's own server goes through admission, restore barriers
//! and, for a database on another server, forwarding. Each call is one
//! pipeline request holding one transaction: SQLite's single writer is never
//! held across network round trips.

use std::io::Read;
use std::sync::OnceLock;
use std::time::Duration;

use reqwest::blocking::Client as Http;
use reqwest::header::{AUTHORIZATION, HeaderValue};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::proto::Value as SqlValue;

/// The largest response a call reads.
const RESPONSE_LIMIT: u64 = 32 * 1024 * 1024;

/// One SQL statement with positional arguments. Arguments are JSON scalars:
/// strings, numbers, booleans (stored as 0 or 1) and null.
pub struct Statement {
    pub sql: String,
    pub args: Vec<Value>,
}

impl Statement {
    pub fn new(sql: impl Into<String>, args: Vec<Value>) -> Self {
        Self {
            sql: sql.into(),
            args,
        }
    }
}

/// The rows a statement returned, each a list of JSON scalars.
pub type Rows = Vec<Vec<Value>>;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The database could not answer now: the request failed, timed out, or
    /// was refused while the database is restored, moved or unreachable.
    /// Retrying later can succeed.
    #[error("{0}")]
    Unavailable(String),
    /// The database refused the binding token.
    #[error("{0}")]
    Unauthorized(String),
    /// A statement failed. `code` is SQLite's error name; the message is left
    /// out because it can contain application data.
    #[error("database statement failed ({code})")]
    Statement { code: String },
    /// The request or response was malformed.
    #[error("{0}")]
    Invalid(String),
}

impl ClientError {
    /// Whether the call can succeed when retried, including a statement that
    /// failed only because the database was busy.
    pub fn is_unavailable(&self) -> bool {
        match self {
            Self::Unavailable(_) => true,
            Self::Statement { code } => code.contains("BUSY") || code.contains("LOCKED"),
            Self::Unauthorized(_) | Self::Invalid(_) => false,
        }
    }
}

fn invalid(message: &str) -> ClientError {
    ClientError::Invalid(message.to_string())
}

/// A connection to one database binding: the local endpoint plus the
/// binding's token.
pub struct Client {
    url: String,
    authorization: HeaderValue,
    http: &'static Http,
}

impl Client {
    /// `url` must be a loopback endpoint (`http://127.0.0.1:<port>`): Tako
    /// never sends its own statements anywhere else.
    pub fn new(url: &str, token: &str) -> Result<Self, ClientError> {
        let port = url
            .strip_prefix("http://127.0.0.1:")
            .and_then(|p| p.parse::<u16>().ok())
            .filter(|port| *port != 0)
            .ok_or_else(|| invalid("expected a local database endpoint"))?;
        if token.len() != 64 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(invalid("invalid database binding credential"));
        }
        let mut authorization = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| invalid("invalid database binding credential"))?;
        authorization.set_sensitive(true);
        // One pool per process, not per app or caller.
        static HTTP: OnceLock<Result<Http, reqwest::Error>> = OnceLock::new();
        let http = HTTP
            .get_or_init(|| {
                Http::builder()
                    .no_proxy()
                    .redirect(reqwest::redirect::Policy::none())
                    .retry(reqwest::retry::never())
                    .connect_timeout(Duration::from_secs(2))
                    .timeout(Duration::from_secs(10))
                    .build()
            })
            .as_ref()
            .map_err(|_| invalid("cannot initialize database transport"))?;
        Ok(Self {
            url: format!("http://127.0.0.1:{port}/v3/pipeline"),
            authorization,
            http,
        })
    }

    /// Runs `statements` in one write transaction (`BEGIN IMMEDIATE`) and
    /// returns each statement's rows. A failing statement rolls back all of
    /// them.
    pub fn batch(&self, statements: &[Statement]) -> Result<Vec<Rows>, ClientError> {
        self.transaction("BEGIN IMMEDIATE", statements)
    }

    /// Runs `statements` in one read transaction, which sees one snapshot.
    pub fn read_batch(&self, statements: &[Statement]) -> Result<Vec<Rows>, ClientError> {
        self.transaction("BEGIN", statements)
    }

    fn transaction(&self, begin: &str, statements: &[Statement]) -> Result<Vec<Rows>, ClientError> {
        let mut steps = Vec::with_capacity(statements.len() + 2);
        steps.push(json!({"stmt":{"sql":begin}}));
        for (i, stmt) in statements.iter().enumerate() {
            let args: Vec<_> = stmt
                .args
                .iter()
                .map(sql_argument)
                .collect::<Result<_, _>>()?;
            steps.push(
                json!({"condition":{"type":"ok","step":i},"stmt":{"sql":stmt.sql,"args":args}}),
            );
        }
        // A failed step skips every following step, including COMMIT; `close`
        // rolls back an unfinished transaction without leaking a stream baton.
        steps.push(
            json!({"condition":{"type":"ok","step":statements.len()},"stmt":{"sql":"COMMIT"}}),
        );
        let request =
            json!({"requests":[{"type":"batch","batch":{"steps":steps}},{"type":"close"}]});
        let response = self
            .http
            .post(&self.url)
            .header(AUTHORIZATION, self.authorization.clone())
            .json(&request)
            .send()
            .map_err(|_| ClientError::Unavailable("database request failed".into()))?;
        let status = response.status();
        if !status.is_success() {
            let message = format!("database returned HTTP {}", status.as_u16());
            return Err(match status.as_u16() {
                401 | 403 => ClientError::Unauthorized(message),
                400 | 404 | 413 => ClientError::Invalid(message),
                _ => ClientError::Unavailable(message),
            });
        }
        let mut bytes = Vec::new();
        response
            .take(RESPONSE_LIMIT + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| ClientError::Unavailable("incomplete database response".into()))?;
        if bytes.len() as u64 > RESPONSE_LIMIT {
            return Err(invalid("database response exceeds 32 MiB"));
        }
        let reply: Pipeline =
            serde_json::from_slice(&bytes).map_err(|_| invalid("invalid database response"))?;
        let mut results = reply.results.into_iter();
        let Some(Outcome::Ok {
            response: Reply::Batch { result },
        }) = results.next()
        else {
            return Err(ClientError::Unavailable(
                "database batch unavailable".into(),
            ));
        };
        if !matches!(
            results.next(),
            Some(Outcome::Ok {
                response: Reply::Close
            })
        ) || results.next().is_some()
            || result.step_results.len() != statements.len() + 2
            || result.step_errors.len() != result.step_results.len()
        {
            return Err(invalid("incomplete database transaction"));
        }
        if let Some(failed) = result.step_errors.into_iter().flatten().next() {
            return Err(ClientError::Statement { code: failed.code });
        }
        let mut steps = result.step_results.into_iter();
        if steps.next().flatten().is_none() || steps.next_back().flatten().is_none() {
            return Err(invalid("database transaction did not commit"));
        }
        steps
            .map(|step| {
                step.ok_or_else(|| invalid("database statement skipped"))?
                    .rows
                    .into_iter()
                    .map(|row| row.into_iter().map(sql_scalar).collect())
                    .collect()
            })
            .collect()
    }
}

fn sql_argument(value: &Value) -> Result<SqlValue, ClientError> {
    match value {
        Value::Null => Ok(SqlValue::Null),
        Value::String(s) => Ok(SqlValue::Text(s.clone())),
        Value::Number(n) if n.as_i64().is_some() => Ok(SqlValue::Integer(n.as_i64().unwrap())),
        Value::Number(n) if n.is_f64() => Ok(SqlValue::Float(n.as_f64().unwrap())),
        Value::Bool(b) => Ok(SqlValue::Integer(i64::from(*b))),
        _ => Err(invalid("expected scalar SQL parameter")),
    }
}

fn sql_scalar(value: SqlValue) -> Result<Value, ClientError> {
    match value {
        SqlValue::Null => Ok(Value::Null),
        SqlValue::Text(s) => Ok(Value::String(s)),
        SqlValue::Integer(i) => Ok(i.into()),
        SqlValue::Float(f) => Ok(f.into()),
        SqlValue::Blob(_) => Err(invalid("unexpected blob in database result")),
    }
}

#[derive(Deserialize)]
struct Pipeline {
    results: Vec<Outcome>,
}
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Outcome {
    Ok { response: Reply },
    Error,
}
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Reply {
    Batch { result: Batch },
    Close,
}
#[derive(Deserialize)]
struct Batch {
    step_results: Vec<Option<Step>>,
    step_errors: Vec<Option<Failure>>,
}
#[derive(Deserialize)]
struct Step {
    rows: Vec<Vec<SqlValue>>,
}
#[derive(Deserialize)]
struct Failure {
    code: String,
}

#[cfg(all(test, feature = "testing"))]
mod tests {
    use super::*;
    use crate::testing::TestServer;

    #[test]
    fn transaction_uses_real_host_rollback_and_revocable_binding() {
        let server = TestServer::start();
        let database = server.database("shop/production#channels");
        let client = Client::new(&server.url, &database.token).unwrap();
        let stmt = |sql| Statement::new(sql, vec![]);
        client
            .batch(&[stmt("CREATE TABLE events(id INTEGER PRIMARY KEY)")])
            .unwrap();
        let duplicate = client
            .batch(&[
                stmt("INSERT INTO events VALUES(1)"),
                stmt("INSERT INTO events VALUES(1)"),
            ])
            .unwrap_err();
        assert!(
            matches!(&duplicate, ClientError::Statement { code } if code.contains("CONSTRAINT")),
            "{duplicate:?}"
        );
        assert!(!duplicate.is_unavailable());
        assert_eq!(
            client
                .read_batch(&[stmt("SELECT COUNT(*) FROM events")])
                .unwrap()[0][0][0],
            0
        );
        assert_eq!(
            client
                .batch(&[Statement::new(
                    "INSERT INTO events VALUES(?) RETURNING id",
                    vec![i64::MAX.into()]
                )])
                .unwrap()[0][0][0],
            i64::MAX
        );
        server.revoke("shop/production#channels");
        let revoked = client
            .read_batch(&[stmt("SELECT id FROM events")])
            .unwrap_err();
        assert!(
            matches!(revoked, ClientError::Unauthorized(_)),
            "{revoked:?}"
        );
    }

    #[test]
    fn refuses_remote_endpoints_and_malformed_tokens() {
        let token = crate::generate_token();
        assert!(Client::new("http://10.0.0.1:9845", &token).is_err());
        assert!(Client::new("http://127.0.0.1:0", &token).is_err());
        assert!(Client::new("http://127.0.0.1:9845", "short").is_err());
        assert!(Client::new("http://127.0.0.1:9845", &token).is_ok());
    }
}
