//! Trusted replay statements reach the ordinary database host through its local
//! authenticated endpoint. Even the home server goes through admission and restore barriers.
use crate::ChannelError;
use reqwest::blocking::Client as Http;
use reqwest::header::{AUTHORIZATION, HeaderValue};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::Read;
use std::sync::OnceLock;
use std::time::Duration;
use tako_database::Value as SqlValue;

pub(super) struct Statement {
    pub(super) sql: String,
    pub(super) args: Vec<Value>,
}

impl Statement {
    pub(super) fn new(sql: &str, args: Vec<Value>) -> Self {
        Self {
            sql: sql.into(),
            args,
        }
    }
}

pub(super) struct Client {
    url: String,
    authorization: HeaderValue,
    http: &'static Http,
}

type Rows = Vec<Vec<Value>>;

fn error(message: &str) -> ChannelError {
    ChannelError::Storage(format!("hosted channel replay: {message}"))
}

impl Client {
    pub(super) fn new(url: &str, token: &str) -> Result<Self, ChannelError> {
        let port = url
            .strip_prefix("http://127.0.0.1:")
            .and_then(|p| p.parse::<u16>().ok())
            .filter(|port| *port != 0)
            .ok_or_else(|| error("expected a local database endpoint"))?;
        if token.len() != 64 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(error("invalid database binding credential"));
        }
        let mut authorization = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| error("invalid database binding credential"))?;
        authorization.set_sensitive(true);
        // One pool/runtime per process, not per app or subscriber.
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
            .map_err(|_| error("cannot initialize database transport"))?;
        Ok(Self {
            url: format!("http://127.0.0.1:{port}/v3/pipeline"),
            authorization,
            http,
        })
    }

    pub(super) fn batch(&self, statements: &[Statement]) -> Result<Vec<Rows>, ChannelError> {
        self.transaction("BEGIN IMMEDIATE", statements)
    }

    pub(super) fn read_batch(&self, statements: &[Statement]) -> Result<Vec<Rows>, ChannelError> {
        self.transaction("BEGIN", statements)
    }

    fn transaction(
        &self,
        begin: &str,
        statements: &[Statement],
    ) -> Result<Vec<Rows>, ChannelError> {
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
            .map_err(|_| error("database request failed"))?;
        if !response.status().is_success() {
            return Err(error(&format!(
                "database returned HTTP {}",
                response.status().as_u16()
            )));
        }
        const LIMIT: u64 = 32 * 1024 * 1024;
        let mut bytes = Vec::new();
        response
            .take(LIMIT + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| error("incomplete database response"))?;
        if bytes.len() > LIMIT as usize {
            return Err(error("database response exceeds 32 MiB"));
        }
        let reply: Pipeline =
            serde_json::from_slice(&bytes).map_err(|_| error("invalid database response"))?;
        let mut results = reply.results.into_iter();
        let Some(Outcome::Ok {
            response: Reply::Batch { result },
        }) = results.next()
        else {
            return Err(error("database batch unavailable"));
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
            return Err(error("incomplete database transaction"));
        }
        if let Some(failed) = result.step_errors.into_iter().flatten().next() {
            // Do not copy SQL error messages, which may contain application data, into logs.
            return Err(error(&format!(
                "database statement failed ({})",
                failed.code
            )));
        }
        let mut steps = result.step_results.into_iter();
        if steps.next().flatten().is_none() || steps.next_back().flatten().is_none() {
            return Err(error("database transaction did not commit"));
        }
        steps
            .map(|step| {
                step.ok_or_else(|| error("database statement skipped"))?
                    .rows
                    .into_iter()
                    .map(|row| row.into_iter().map(sql_scalar).collect())
                    .collect()
            })
            .collect()
    }
}

fn sql_argument(value: &Value) -> Result<SqlValue, ChannelError> {
    match value {
        Value::Null => Ok(SqlValue::Null),
        Value::String(s) => Ok(SqlValue::Text(s.clone())),
        Value::Number(n) if n.as_i64().is_some() => Ok(SqlValue::Integer(n.as_i64().unwrap())),
        Value::Number(n) if n.is_f64() => Ok(SqlValue::Float(n.as_f64().unwrap())),
        Value::Bool(b) => Ok(SqlValue::Integer(i64::from(*b))),
        _ => Err(error("expected scalar SQL parameter")),
    }
}

fn sql_scalar(value: SqlValue) -> Result<Value, ChannelError> {
    match value {
        SqlValue::Null => Ok(Value::Null),
        SqlValue::Text(s) => Ok(Value::String(s)),
        SqlValue::Integer(i) => Ok(i.into()),
        SqlValue::Float(f) => Ok(f.into()),
        SqlValue::Blob(_) => Err(error("unexpected blob in replay result")),
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

#[cfg(test)]
mod tests {
    use super::*;
    use tako_database::{
        Binding, DatabaseHost, DatabaseId, Home, HostConfig, generate_token, hash_token,
    };

    #[tokio::test]
    async fn transaction_uses_real_host_rollback_and_revocable_binding() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("database.sqlite");
        DatabaseHost::create_file(&path).unwrap();
        let host = DatabaseHost::new(HostConfig::default());
        let db = DatabaseId::generate();
        host.register(db.clone(), path).unwrap();
        let token = generate_token();
        let scope = "shop/production#channels";
        host.set_bindings(
            scope,
            vec![Binding {
                database: db,
                token_hash: hash_token(&token),
                home: Home::Local,
            }],
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(tako_database::http::serve(
            host.clone(),
            listener,
            tako_database::http::Access::Apps,
        ));
        tokio::task::spawn_blocking(move || {
            let client = Client::new(&url, &token).unwrap();
            let stmt = |sql| Statement::new(sql, vec![]);
            client
                .batch(&[stmt("CREATE TABLE events(id INTEGER PRIMARY KEY)")])
                .unwrap();
            assert!(
                client
                    .batch(&[
                        stmt("INSERT INTO events VALUES(1)"),
                        stmt("INSERT INTO events VALUES(1)")
                    ])
                    .is_err()
            );
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
            host.set_bindings(scope, vec![]);
            assert!(client.read_batch(&[stmt("SELECT id FROM events")]).is_err());
        })
        .await
        .unwrap();
        task.abort();
    }
}
