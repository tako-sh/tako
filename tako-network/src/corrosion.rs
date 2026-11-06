//! Typed access to the authenticated, local Corrosion 1.0 HTTP API.
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    time::Duration,
};

use reqwest::{
    Url,
    blocking::Client,
    header::{AUTHORIZATION, HeaderMap, HeaderValue},
    redirect::Policy,
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use zeroize::Zeroizing;

use crate::{
    Ack, Member, NetworkDescriptor, RegistrySnapshot, Revocation,
    api::{RuntimeError, RuntimeResult},
};

pub const MAX_REGISTRY_ROWS: usize = 10_000;
pub const MAX_REGISTRY_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_RECORD_BYTES: usize = 128 * 1024;
// Snapshots run several times per agent tick, so page size sets the number of
// sequential loopback round-trips. Every response and the whole projection are
// already bounded by MAX_REGISTRY_BYTES, so a page cannot exceed that cap unless
// the registry itself is over capacity, which fails closed either way.
const PAGE_ROWS: usize = 256;
const MAX_ADMISSION_ROWS: usize = MAX_REGISTRY_ROWS / 2;
const MAX_ADMISSION_BYTES: usize = MAX_REGISTRY_BYTES / 2;
// A denial captures every known server, including those admitted on other replicas.
const MAX_REVOCATION_BYTES: usize = MAX_REGISTRY_ROWS * 67 + 1024;

#[cfg(test)]
type TestTransport = fn(&str, &Value) -> RuntimeResult<Vec<u8>>;
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
/// Membership and denial records are immutable. ACKs are replaceable boot observations.
/// Snapshot capacity errors must fail closed at the caller, never fall back to old grants.
/// Mutations use `Capacity("registry admissions")` or `Capacity("registry record")`
/// only for candidate rejection before publication. Other capacity errors mean
/// unreadable registry state, including when a mutation encounters them internally.
pub trait RegistryStore {
    fn snapshot(&mut self) -> RuntimeResult<RegistrySnapshot>;
    fn append_member(&mut self, member: &Member) -> RuntimeResult<()>;
    fn append_revocation(&mut self, revocation: &Revocation) -> RuntimeResult<()>;
    fn append_ack(&mut self, ack: &Ack) -> RuntimeResult<()>;
}

pub struct CorrosionStore {
    #[cfg(test)]
    pub(crate) transport: Option<TestTransport>,
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
            #[cfg(test)]
            transport: None,
            client,
            endpoint,
            network,
        })
    }

    /// Parent calls this only during locally authorized bootstrap, after schema setup.
    pub fn initialize(&mut self) -> RuntimeResult<()> {
        let rows = self.read_rows()?;
        if rows.last.is_some() {
            rows.finish()?;
            return Ok(());
        }
        self.insert("network", "network", &self.network)?;
        self.snapshot()?;
        Ok(())
    }

    pub(crate) fn post(&self, path: &str, body: &Value) -> RuntimeResult<Vec<u8>> {
        #[cfg(test)]
        if let Some(post) = self.transport {
            return post(path, body);
        }
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

    fn read_rows(&self) -> RuntimeResult<RecordProjection> {
        let mut projection = RecordProjection::new(&self.network);
        let high = parse_query(&self.post("/v1/queries", &json!([
            "SELECT kind, record_id, digest, payload FROM tako_network_records ORDER BY kind DESC, record_id DESC, digest DESC LIMIT 1", []
        ]))?)?;
        let Some(high) = high.first() else {
            return Ok(projection);
        };
        let upper = record_key(high)?;
        let mut cursor = (String::new(), String::new(), String::new());
        loop {
            let page = parse_query(&self.post("/v1/queries", &json!([
                "SELECT kind, record_id, digest, payload FROM tako_network_records WHERE (kind, record_id, digest) > (?, ?, ?) AND (kind, record_id, digest) <= (?, ?, ?) ORDER BY kind, record_id, digest LIMIT ?",
                [cursor.0, cursor.1, cursor.2, upper.0, upper.1, upper.2, PAGE_ROWS]
            ]))?)?;
            if page.is_empty() {
                break;
            }
            if page.len() > PAGE_ROWS {
                return Err(RuntimeError::Corrosion("page size"));
            }
            for row in page {
                let key = record_key(&row)?;
                if key <= cursor || key > upper {
                    return Err(RuntimeError::Corrosion("page order"));
                }
                cursor = key;
                projection.push(row)?;
            }
        }
        Ok(projection)
    }

    fn record<T: DeserializeOwned>(&self, kind: &str, id: &str) -> RuntimeResult<Option<T>> {
        let rows = parse_query(&self.post("/v1/queries", &json!([
            "SELECT kind, record_id, digest, payload FROM tako_network_records WHERE kind=? AND record_id=? ORDER BY digest LIMIT 2",
            [kind, id]
        ]))?)?;
        if rows.len() > 1 {
            return Err(RuntimeError::Conflict("record ID"));
        }
        rows.first()
            .map(|row| {
                let (found_kind, found_id, _) = record_key(row)?;
                if found_kind != kind || found_id != id {
                    return Err(RuntimeError::Corrosion("record key"));
                }
                Ok(serde_json::from_str(record_payload(row)?)?)
            })
            .transpose()
    }

    fn require_network(&self) -> RuntimeResult<()> {
        if self
            .record::<NetworkDescriptor>("network", "network")?
            .as_ref()
            != Some(&self.network)
        {
            return Err(RuntimeError::Conflict("network descriptor"));
        }
        Ok(())
    }

    fn insert<T: Serialize>(&self, kind: &str, id: &str, payload: &T) -> RuntimeResult<()> {
        let payload = serde_json::to_string(payload)?;
        if payload.len() > record_limit(kind) {
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
        self.read_rows()?.finish()
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
        snapshot.members.push(member.clone());
        if snapshot.members.len() + snapshot.revocations.len() + 1 > MAX_ADMISSION_ROWS
            || bounded_json_size(&snapshot)? > MAX_ADMISSION_BYTES
        {
            return Err(RuntimeError::Capacity("registry admissions"));
        }
        snapshot.validate()?;
        self.insert("member", &member.device_id.to_string(), member)?;
        self.snapshot()?;
        Ok(())
    }

    fn append_revocation(&mut self, revocation: &Revocation) -> RuntimeResult<()> {
        self.require_network()?;
        if let Some(previous) =
            self.record::<Revocation>("revocation", &revocation.id.to_string())?
        {
            if previous != *revocation {
                return Err(RuntimeError::Conflict("revocation"));
            }
            return Ok(());
        }
        self.insert("revocation", &revocation.id.to_string(), revocation)?;
        if self
            .record::<Revocation>("revocation", &revocation.id.to_string())?
            .as_ref()
            != Some(revocation)
        {
            return Err(RuntimeError::Corrosion("revocation absent after write"));
        }
        Ok(())
    }

    fn append_ack(&mut self, ack: &Ack) -> RuntimeResult<()> {
        self.require_network()?;
        let revocation = self.record::<Revocation>("revocation", &ack.revocation.to_string())?;
        let server = self.record::<Member>("member", &ack.server.to_string())?;
        if let Some(server) = &server {
            server.validate()?;
            if server.network_id != self.network.id {
                return Err(RuntimeError::Conflict("member network"));
            }
        }
        if !revocation.is_some_and(|r| r.id == ack.revocation)
            || !server.is_some_and(|m| {
                m.device_id == ack.server
                    && m.wg_verified
                    && matches!(m.kind, crate::MemberKind::Server(_))
            })
        {
            return Err(RuntimeError::Invalid("ACK target or server"));
        }
        let payload = serde_json::to_string(ack)?;
        let digest = hex::encode(openssl::sha::sha256(payload.as_bytes()));
        let id = ack_id(ack);
        if self
            .record::<Ack>("ack", &id)?
            .is_some_and(|previous| previous != *ack)
        {
            return Err(RuntimeError::Conflict("ACK ID"));
        }
        let prefix = format!("{}:{}:", ack.revocation, ack.server);
        // Only boot-specific observations are replaceable. Denials remain immutable.
        let body = json!([
            [
                "INSERT INTO tako_network_records (kind, record_id, digest, payload) SELECT 'ack', ?, ?, ? WHERE NOT EXISTS (SELECT 1 FROM tako_network_records WHERE kind='ack' AND record_id=?)",
                [id, digest, payload, id]
            ],
            [
                "DELETE FROM tako_network_records WHERE kind='ack' AND substr(record_id, 1, length(?))=? AND record_id<>?",
                [prefix, prefix, id]
            ]
        ]);
        parse_transaction(&self.post("/v1/transactions", &body)?, 2)
    }
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

fn record_key(row: &[Value]) -> RuntimeResult<(String, String, String)> {
    if row.len() != 4 {
        return Err(RuntimeError::Corrosion("record width"));
    }
    let text = |i: usize| {
        row[i]
            .as_str()
            .map(str::to_owned)
            .ok_or(RuntimeError::Corrosion("non-text record"))
    };
    Ok((text(0)?, text(1)?, text(2)?))
}

fn record_limit(kind: &str) -> usize {
    if kind == "revocation" {
        MAX_REVOCATION_BYTES
    } else {
        MAX_RECORD_BYTES
    }
}

fn record_payload(row: &[Value]) -> RuntimeResult<&str> {
    let payload = row
        .get(3)
        .and_then(Value::as_str)
        .ok_or(RuntimeError::Corrosion("non-text record"))?;
    if payload.len() > record_limit(row[0].as_str().unwrap_or_default()) {
        return Err(RuntimeError::Capacity("registry stored record"));
    }
    if row[2].as_str() != Some(&hex::encode(openssl::sha::sha256(payload.as_bytes()))) {
        return Err(RuntimeError::Corrosion("payload digest mismatch"));
    }
    Ok(payload)
}

struct RecordProjection {
    snapshot: RegistrySnapshot,
    acks: BTreeMap<(crate::RevocationId, crate::DeviceId), Ack>,
    last: Option<(String, String, String)>,
    bytes: usize,
    network_seen: bool,
}

impl RecordProjection {
    fn new(pinned: &NetworkDescriptor) -> Self {
        Self {
            snapshot: RegistrySnapshot {
                network: pinned.clone(),
                members: vec![],
                revocations: vec![],
                acks: vec![],
            },
            acks: BTreeMap::new(),
            last: None,
            bytes: 0,
            network_seen: false,
        }
    }

    fn push(&mut self, row: Vec<Value>) -> RuntimeResult<()> {
        let (kind, id, digest) = record_key(&row)?;
        let payload = record_payload(&row)?;
        if let Some((old_kind, old_id, old_digest)) = &self.last
            && old_kind == &kind
            && old_id == &id
        {
            if old_digest != &digest {
                return Err(RuntimeError::Conflict("record ID"));
            }
            return Ok(());
        }
        self.last = Some((kind.clone(), id.clone(), digest));
        match kind.as_str() {
            "network" => {
                let network: NetworkDescriptor = serde_json::from_str(payload)?;
                if id != "network" || network != self.snapshot.network {
                    return Err(RuntimeError::Conflict("network descriptor"));
                }
                self.bytes += bounded_json_size(&network)?;
                self.network_seen = true;
            }
            "member" => {
                let member: Member = serde_json::from_str(payload)?;
                if member.device_id.to_string() != id {
                    return Err(RuntimeError::Corrosion("member ID"));
                }
                self.bytes += bounded_json_size(&member)?;
                self.snapshot.members.push(member);
            }
            "revocation" => {
                let revocation: Revocation = serde_json::from_str(payload)?;
                if revocation.id.to_string() != id {
                    return Err(RuntimeError::Corrosion("revocation ID"));
                }
                self.bytes += bounded_json_size(&revocation)?;
                self.snapshot.revocations.push(revocation);
            }
            "ack" => {
                let ack: Ack = serde_json::from_str(payload)?;
                if ack_id(&ack) != id {
                    return Err(RuntimeError::Corrosion("ACK ID"));
                }
                // Concurrent boots may both survive replication. Keeping one exact
                // boot can report pending, but cannot invent current enforcement.
                let key = (ack.revocation.clone(), ack.server.clone());
                if self.acks.len() < MAX_REGISTRY_ROWS || self.acks.contains_key(&key) {
                    self.acks.insert(key, ack);
                }
            }
            _ => return Err(RuntimeError::Corrosion("unknown record kind")),
        }
        if self.bytes > MAX_REGISTRY_BYTES
            || self.snapshot.members.len() + self.snapshot.revocations.len() + 1 > MAX_REGISTRY_ROWS
        {
            return Err(RuntimeError::Capacity("registry projection"));
        }
        Ok(())
    }

    fn finish(mut self) -> RuntimeResult<RegistrySnapshot> {
        if !self.network_seen {
            return Err(RuntimeError::Corrosion("missing pinned network descriptor"));
        }
        self.snapshot.acks = self.acks.into_values().collect();
        bound_projection(&mut self.snapshot)?;
        self.snapshot.validate()?;
        Ok(self.snapshot)
    }
}

pub(crate) fn bounded_json_size(value: &impl Serialize) -> RuntimeResult<usize> {
    struct Counter {
        bytes: usize,
        exceeded: bool,
    }
    impl Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > MAX_REGISTRY_BYTES - self.bytes {
                self.exceeded = true;
                return Err(std::io::Error::other("registry bytes"));
            }
            self.bytes += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter {
        bytes: 0,
        exceeded: false,
    };
    let result = serde_json::to_writer(&mut counter, value);
    if counter.exceeded {
        return Err(RuntimeError::Capacity("registry bytes"));
    }
    result?;
    Ok(counter.bytes)
}

/// ACKs are evidence of progress, never authority. Omitting one only reports pending.
pub(crate) fn bound_projection(snapshot: &mut RegistrySnapshot) -> RuntimeResult<()> {
    let acks = std::mem::take(&mut snapshot.acks);
    let rows = snapshot.members.len() + snapshot.revocations.len() + 1;
    if rows > MAX_REGISTRY_ROWS {
        return Err(RuntimeError::Capacity("registry projection"));
    }
    let mut bytes = bounded_json_size(snapshot)?;
    for ack in acks {
        if rows + snapshot.acks.len() >= MAX_REGISTRY_ROWS {
            break;
        }
        let size = serde_json::to_vec(&ack)?.len() + usize::from(!snapshot.acks.is_empty());
        if bytes + size <= MAX_REGISTRY_BYTES {
            bytes += size;
            snapshot.acks.push(ack);
        }
    }
    Ok(())
}

#[cfg(test)]
fn snapshot_from_rows(
    mut rows: Vec<Vec<Value>>,
    pinned: &NetworkDescriptor,
) -> RuntimeResult<RegistrySnapshot> {
    rows.sort_by_key(|row| record_key(row).unwrap_or_default());
    let mut projection = RecordProjection::new(pinned);
    for row in rows {
        projection.push(row)?;
    }
    projection.finish()
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
