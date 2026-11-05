//! Typed access to the authenticated, local Corrosion 1.0 HTTP API.
use std::{collections::BTreeMap, io::Read, time::Duration};

use reqwest::{
    Url,
    blocking::Client,
    header::{AUTHORIZATION, HeaderMap, HeaderValue},
    redirect::Policy,
};
use serde::Serialize;
use serde_json::{Value, json};
use zeroize::Zeroizing;

use crate::{
    Ack, Member, NetworkDescriptor, RegistrySnapshot, Revocation,
    api::{RuntimeError, RuntimeResult},
};

pub const MAX_REGISTRY_ROWS: usize = 10_000;
pub const MAX_REGISTRY_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_RECORD_BYTES: usize = 128 * 1024;
pub const CORROSION_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS tako_network_records (\n\
    kind TEXT NOT NULL, record_id TEXT NOT NULL, digest TEXT NOT NULL, payload TEXT NOT NULL,\n\
    PRIMARY KEY (kind, record_id, digest)\n\
);";

pub struct BearerToken(Zeroizing<String>);

impl BearerToken {
    pub fn new(token: String) -> RuntimeResult<Self> {
        let token = Zeroizing::new(token);
        if token.len() < 16 || token.len() > 4096 || !token.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(RuntimeError::Invalid("Corrosion bearer token"));
        }
        Ok(Self(token))
    }
}

/// Trusted server storage only. Never expose these mutation methods as RPCs.
pub trait RegistryStore {
    fn snapshot(&mut self) -> RuntimeResult<RegistrySnapshot>;
    fn append_member(&mut self, member: &Member) -> RuntimeResult<()>;
    fn append_revocation(&mut self, revocation: &Revocation) -> RuntimeResult<()>;
    fn append_ack(&mut self, ack: &Ack) -> RuntimeResult<()>;
}

pub struct CorrosionStore {
    client: Client,
    endpoint: Url,
    network: NetworkDescriptor,
}

impl CorrosionStore {
    pub fn new(
        endpoint: &str,
        token: BearerToken,
        network: NetworkDescriptor,
    ) -> RuntimeResult<Self> {
        let endpoint =
            Url::parse(endpoint).map_err(|_| RuntimeError::Invalid("Corrosion endpoint"))?;
        let loopback = endpoint
            .host_str()
            .and_then(|host| {
                host.trim_start_matches('[')
                    .trim_end_matches(']')
                    .parse::<std::net::IpAddr>()
                    .ok()
            })
            .is_some_and(|ip| ip.is_loopback());
        if !loopback
            || endpoint.scheme() != "http"
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
            || endpoint.path() != "/"
        {
            return Err(RuntimeError::Invalid(
                "expected a literal loopback Corrosion HTTP origin",
            ));
        }
        let secret = Zeroizing::new(format!("Bearer {}", token.0.as_str()));
        let mut authorization =
            HeaderValue::from_str(&secret).map_err(|_| RuntimeError::Invalid("bearer header"))?;
        authorization.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, authorization);
        let client = Client::builder()
            .default_headers(headers)
            .no_proxy()
            .redirect(Policy::none())
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|_| RuntimeError::Transport)?;
        Ok(Self {
            client,
            endpoint,
            network,
        })
    }

    /// Parent calls this only during locally authorized bootstrap, after schema setup.
    pub fn initialize(&mut self) -> RuntimeResult<()> {
        let rows = self.read_rows()?;
        if !rows.is_empty() {
            snapshot_from_rows(rows, &self.network)?;
            return Ok(());
        }
        self.insert("network", "network", &self.network)?;
        self.snapshot()?;
        Ok(())
    }

    fn post(&self, path: &str, body: &Value) -> RuntimeResult<Vec<u8>> {
        let url = self
            .endpoint
            .join(path)
            .map_err(|_| RuntimeError::Invalid("Corrosion path"))?;
        let response = self
            .client
            .post(url)
            .json(body)
            .send()
            .map_err(|_| RuntimeError::Transport)?;
        if !response.status().is_success() {
            return Err(RuntimeError::Transport);
        }
        if response
            .content_length()
            .is_some_and(|size| size > MAX_REGISTRY_BYTES as u64)
        {
            return Err(RuntimeError::Capacity("Corrosion response"));
        }
        let mut bytes = Vec::new();
        response
            .take(MAX_REGISTRY_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| RuntimeError::Transport)?;
        if bytes.len() > MAX_REGISTRY_BYTES {
            return Err(RuntimeError::Capacity("Corrosion response"));
        }
        Ok(bytes)
    }

    fn read_rows(&self) -> RuntimeResult<Vec<Vec<Value>>> {
        let body = json!([
            "SELECT kind, record_id, digest, payload FROM tako_network_records ORDER BY kind, record_id, digest LIMIT ?",
            [MAX_REGISTRY_ROWS + 1]
        ]);
        parse_query(&self.post("/v1/queries", &body)?)
    }

    fn insert<T: Serialize>(&self, kind: &str, id: &str, payload: &T) -> RuntimeResult<()> {
        let payload = serde_json::to_string(payload)?;
        if payload.len() > MAX_RECORD_BYTES {
            return Err(RuntimeError::Capacity("registry record"));
        }
        let digest = hex::encode(openssl::sha::sha256(payload.as_bytes()));
        let body = json!([[
            "INSERT OR IGNORE INTO tako_network_records (kind, record_id, digest, payload) VALUES (?, ?, ?, ?)",
            [kind, id, digest, payload]
        ]]);
        parse_transaction(&self.post("/v1/transactions", &body)?, 1)
    }
}

impl RegistryStore for CorrosionStore {
    fn snapshot(&mut self) -> RuntimeResult<RegistrySnapshot> {
        snapshot_from_rows(self.read_rows()?, &self.network)
    }

    fn append_member(&mut self, member: &Member) -> RuntimeResult<()> {
        member.validate()?;
        let mut snapshot = self.snapshot()?;
        if let Some(previous) = snapshot
            .members
            .iter()
            .find(|m| m.device_id == member.device_id)
        {
            return if previous == member {
                Ok(())
            } else {
                Err(RuntimeError::Conflict("member"))
            };
        }
        check_room(&snapshot)?;
        snapshot.members.push(member.clone());
        snapshot.validate()?;
        self.insert("member", &member.device_id.to_string(), member)?;
        self.snapshot()?;
        Ok(())
    }

    fn append_revocation(&mut self, revocation: &Revocation) -> RuntimeResult<()> {
        let snapshot = self.snapshot()?;
        if let Some(previous) = snapshot.revocations.iter().find(|r| r.id == revocation.id) {
            return if previous == revocation {
                Ok(())
            } else {
                Err(RuntimeError::Conflict("revocation"))
            };
        }
        check_room(&snapshot)?;
        self.insert("revocation", &revocation.id.to_string(), revocation)?;
        self.snapshot()?;
        Ok(())
    }

    fn append_ack(&mut self, ack: &Ack) -> RuntimeResult<()> {
        let snapshot = self.snapshot()?;
        if snapshot.acks.contains(ack) {
            return Ok(());
        }
        check_room(&snapshot)?;
        if !snapshot.revocations.iter().any(|r| r.id == ack.revocation)
            || !snapshot.members.iter().any(|m| {
                m.device_id == ack.server
                    && m.wg_verified
                    && matches!(m.kind, crate::MemberKind::Server(_))
            })
        {
            return Err(RuntimeError::Invalid("ACK target or server"));
        }
        self.insert("ack", &ack_id(ack), ack)?;
        self.snapshot()?;
        Ok(())
    }
}

fn check_room(snapshot: &RegistrySnapshot) -> RuntimeResult<()> {
    if snapshot.members.len() + snapshot.revocations.len() + snapshot.acks.len() + 1
        >= MAX_REGISTRY_ROWS
    {
        return Err(RuntimeError::Capacity(
            "registry rows; historical denials are never pruned",
        ));
    }
    Ok(())
}

fn ack_id(ack: &Ack) -> String {
    format!("{}:{}:{}", ack.revocation, ack.server, ack.boot)
}

fn parse_transaction(bytes: &[u8], expected: usize) -> RuntimeResult<()> {
    let response: Value = serde_json::from_slice(bytes)?;
    let results = response
        .get("results")
        .and_then(Value::as_array)
        .ok_or(RuntimeError::Corrosion("missing transaction results"))?;
    if response.get("error").is_some()
        || results.len() != expected
        || results.iter().any(|r| {
            r.get("error").is_some() || r.get("rows_affected").and_then(Value::as_u64).is_none()
        })
    {
        return Err(RuntimeError::Corrosion(
            "statement failed or incomplete transaction",
        ));
    }
    Ok(())
}

fn parse_query(bytes: &[u8]) -> RuntimeResult<Vec<Vec<Value>>> {
    if bytes.len() > MAX_REGISTRY_BYTES {
        return Err(RuntimeError::Capacity("Corrosion response"));
    }
    let mut columns = false;
    let mut ended = false;
    let mut rows = Vec::new();
    for line in bytes
        .split(|b| *b == b'\n')
        .filter(|l| !l.iter().all(u8::is_ascii_whitespace))
    {
        if ended {
            return Err(RuntimeError::Corrosion("data after eoq"));
        }
        let event: Value = serde_json::from_slice(line)?;
        let fields = event
            .as_object()
            .ok_or(RuntimeError::Corrosion("query event"))?;
        if fields.len() != 1 {
            return Err(RuntimeError::Corrosion("query event fields"));
        }
        if let Some(names) = event.get("columns") {
            if columns || names != &json!(["kind", "record_id", "digest", "payload"]) {
                return Err(RuntimeError::Corrosion("query columns"));
            }
            columns = true;
        } else if let Some(row) = event.get("row").and_then(Value::as_array) {
            if !columns || row.len() != 2 || row[0].as_u64().is_none() {
                return Err(RuntimeError::Corrosion("query row framing"));
            }
            let values = row[1]
                .as_array()
                .filter(|v| v.len() == 4)
                .ok_or(RuntimeError::Corrosion("query row columns"))?;
            rows.push(values.clone());
            if rows.len() > MAX_REGISTRY_ROWS {
                return Err(RuntimeError::Capacity("registry rows"));
            }
        } else if event.get("eoq").is_some_and(Value::is_object) && columns {
            ended = true;
        } else {
            return Err(RuntimeError::Corrosion("query failed or unknown event"));
        }
    }
    if !ended {
        return Err(RuntimeError::Corrosion("incomplete query"));
    }
    Ok(rows)
}

fn snapshot_from_rows(
    rows: Vec<Vec<Value>>,
    pinned: &NetworkDescriptor,
) -> RuntimeResult<RegistrySnapshot> {
    let mut snapshot = RegistrySnapshot {
        network: pinned.clone(),
        members: vec![],
        revocations: vec![],
        acks: vec![],
    };
    let mut seen = BTreeMap::new();
    let mut network_seen = false;
    for row in rows {
        let fields = row
            .iter()
            .map(|v| v.as_str().ok_or(RuntimeError::Corrosion("non-text record")))
            .collect::<RuntimeResult<Vec<_>>>()?;
        let [kind, id, digest, payload] = fields.as_slice() else {
            return Err(RuntimeError::Corrosion("record width"));
        };
        if payload.len() > MAX_RECORD_BYTES {
            return Err(RuntimeError::Capacity("registry record"));
        }
        if hex::encode(openssl::sha::sha256(payload.as_bytes())) != *digest {
            return Err(RuntimeError::Corrosion("payload digest mismatch"));
        }
        if let Some(previous) = seen.insert((kind.to_string(), id.to_string()), payload.to_string())
        {
            if previous != *payload {
                return Err(RuntimeError::Conflict("record ID"));
            }
            continue;
        }
        match *kind {
            "network" => {
                let network: NetworkDescriptor = serde_json::from_str(payload)?;
                if *id != "network" || &network != pinned {
                    return Err(RuntimeError::Conflict("network descriptor"));
                }
                network_seen = true;
            }
            "member" => {
                let member: Member = serde_json::from_str(payload)?;
                if member.device_id.to_string() != *id {
                    return Err(RuntimeError::Corrosion("member ID"));
                }
                snapshot.members.push(member);
            }
            "revocation" => {
                let revocation: Revocation = serde_json::from_str(payload)?;
                if revocation.id.to_string() != *id {
                    return Err(RuntimeError::Corrosion("revocation ID"));
                }
                snapshot.revocations.push(revocation);
            }
            "ack" => {
                let ack: Ack = serde_json::from_str(payload)?;
                if ack_id(&ack) != *id {
                    return Err(RuntimeError::Corrosion("ACK ID"));
                }
                snapshot.acks.push(ack);
            }
            _ => return Err(RuntimeError::Corrosion("unknown record kind")),
        }
    }
    if !network_seen {
        return Err(RuntimeError::Corrosion("missing pinned network descriptor"));
    }
    snapshot.validate()?;
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DeviceKind, LocalDeviceKeys, NetworkId};

    fn network() -> NetworkDescriptor {
        let id = NetworkId::random().unwrap();
        NetworkDescriptor {
            initial_admin: LocalDeviceKeys::generate()
                .unwrap()
                .device_id(&id, DeviceKind::Laptop)
                .unwrap(),
            id,
        }
    }
    fn row(kind: &str, id: &str, value: &impl Serialize) -> Vec<Value> {
        let payload = serde_json::to_string(value).unwrap();
        vec![
            json!(kind),
            json!(id),
            json!(hex::encode(openssl::sha::sha256(payload.as_bytes()))),
            json!(payload),
        ]
    }

    #[test]
    fn statement_errors_and_partial_results_fail_even_with_http_success() {
        parse_transaction(br#"{"results":[{"rows_affected":1,"time":0.1}]}"#, 1).unwrap();
        for bad in [
            r#"{"results":[{"error":"bad"}]}"#,
            r#"{"results":[]}"#,
            r#"{"results":[{"rows_affected":1},{"error":"partial"}]}"#,
            r#"{"error":"bad","results":[{"rows_affected":1}]}"#,
        ] {
            assert!(parse_transaction(bad.as_bytes(), 1).is_err());
        }
    }

    #[test]
    fn ndjson_requires_columns_rows_and_terminal_eoq() {
        let prefix = "{\"columns\":[\"kind\",\"record_id\",\"digest\",\"payload\"]}\n";
        assert_eq!(
            parse_query(
                format!("{prefix}{{\"row\":[1,[\"x\",\"a\",\"b\",\"c\"]]}}\n{{\"eoq\":{{}}}}\n")
                    .as_bytes()
            )
            .unwrap()
            .len(),
            1
        );
        for suffix in [
            "",
            "{\"error\":\"bad\"}\n{\"eoq\":{}}",
            "{\"row\":[1,[]]}\n{\"eoq\":{}}",
            "{\"eoq\":{}}\n{\"eoq\":{}}",
        ] {
            assert!(parse_query(format!("{prefix}{suffix}").as_bytes()).is_err());
        }
        assert!(parse_query(b"{\"eoq\":{}}").is_err());
    }

    #[test]
    fn descriptor_is_pinned_and_conflicting_payloads_cannot_be_hidden() {
        let network = network();
        let first = row("network", "network", &network);
        snapshot_from_rows(vec![first.clone(), first.clone()], &network).unwrap();
        assert!(snapshot_from_rows(vec![], &network).is_err());
        let other = self::network();
        assert!(snapshot_from_rows(vec![row("network", "network", &other)], &network).is_err());
        assert!(
            snapshot_from_rows(vec![first, row("network", "network", &other)], &network).is_err()
        );
    }

    #[test]
    fn sql_keeps_conflicts_and_identical_deliveries_are_idempotent() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch(CORROSION_SCHEMA).unwrap();
        for payload in ["one", "one", "two"] {
            db.execute(
                "INSERT OR IGNORE INTO tako_network_records VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![
                    "member",
                    "same-id",
                    hex::encode(openssl::sha::sha256(payload.as_bytes())),
                    payload
                ],
            )
            .unwrap();
        }
        assert_eq!(
            db.query_row("SELECT count(*) FROM tako_network_records", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            2
        );
    }

    #[test]
    fn bearer_and_origin_validation_rejects_remote_or_ambiguous_endpoints() {
        assert!(BearerToken::new("short".into()).is_err());
        assert!(BearerToken::new("not-a-token\nsecret".into()).is_err());
        for endpoint in [
            "http://example.com:8080",
            "http://localhost:8080",
            "https://127.0.0.1:8080",
            "http://127.0.0.1:8080/api",
            "http://user:pass@127.0.0.1:8080",
            "http://127.0.0.1:8080/?secret=token",
        ] {
            let token = BearerToken::new("fixture-token-123456789".into()).unwrap();
            assert!(CorrosionStore::new(endpoint, token, network()).is_err());
        }
        let store = CorrosionStore::new(
            "http://127.0.0.1:8080",
            BearerToken::new("fixture-token-123456789".into()).unwrap(),
            network(),
        )
        .unwrap();
        assert_eq!(store.endpoint.as_str(), "http://127.0.0.1:8080/");
    }
}
