//! Wire types of the SQL over HTTP protocol (Hrana over HTTP, JSON encoding).
//!
//! Version 2 (`/v2/pipeline`) is what `@libsql/client` speaks; version 3
//! (`/v3/pipeline`, `/v3/cursor`) adds `get_autocommit`, the
//! `is_autocommit` condition, and streaming cursors, and is what the Turso
//! serverless drivers speak. Both share these types.

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use serde::de::{self, Deserializer};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};

/// Protocol version selected by the request path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Version {
    V2,
    V3,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PipelineRequest {
    #[serde(default)]
    pub baton: Option<String>,
    pub requests: Vec<StreamRequest>,
}

#[derive(Debug, Serialize)]
pub struct PipelineResponse {
    pub baton: Option<String>,
    pub base_url: Option<String>,
    pub results: Vec<StreamResult>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CursorRequest {
    #[serde(default)]
    pub baton: Option<String>,
    pub batch: Batch,
}

#[derive(Debug, Serialize)]
pub struct CursorResponse {
    pub baton: Option<String>,
    pub base_url: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamRequest {
    Close,
    Execute {
        stmt: Stmt,
    },
    Batch {
        batch: Batch,
    },
    Sequence {
        #[serde(default)]
        sql: Option<String>,
        #[serde(default)]
        sql_id: Option<i32>,
    },
    Describe {
        #[serde(default)]
        sql: Option<String>,
        #[serde(default)]
        sql_id: Option<i32>,
    },
    StoreSql {
        sql_id: i32,
        sql: String,
    },
    CloseSql {
        sql_id: i32,
    },
    GetAutocommit,
}

impl StreamRequest {
    /// Request types that version 2 does not define.
    pub fn requires_v3(&self) -> bool {
        match self {
            StreamRequest::GetAutocommit => true,
            StreamRequest::Batch { batch } => batch.uses_v3_conditions(),
            _ => false,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamResult {
    Ok { response: StreamResponse },
    Error { error: Error },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamResponse {
    Close,
    Execute { result: StmtResult },
    Batch { result: BatchResult },
    Sequence,
    Describe { result: DescribeResult },
    StoreSql,
    CloseSql,
    GetAutocommit { is_autocommit: bool },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stmt {
    #[serde(default)]
    pub sql: Option<String>,
    #[serde(default)]
    pub sql_id: Option<i32>,
    #[serde(default)]
    pub args: Vec<Value>,
    #[serde(default)]
    pub named_args: Vec<NamedArg>,
    #[serde(default)]
    pub want_rows: Option<bool>,
    /// Sent by some libSQL clients; replication is not supported, so it is
    /// accepted and ignored.
    #[serde(default)]
    pub replication_index: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NamedArg {
    pub name: String,
    pub value: Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Batch {
    pub steps: Vec<BatchStep>,
    /// See [`Stmt::replication_index`].
    #[serde(default)]
    pub replication_index: Option<serde_json::Value>,
}

impl Batch {
    fn uses_v3_conditions(&self) -> bool {
        self.steps
            .iter()
            .filter_map(|step| step.condition.as_ref())
            .any(BatchCond::uses_is_autocommit)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchStep {
    #[serde(default)]
    pub condition: Option<BatchCond>,
    pub stmt: Stmt,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BatchCond {
    Ok { step: u32 },
    Error { step: u32 },
    Not { cond: Box<BatchCond> },
    And { conds: Vec<BatchCond> },
    Or { conds: Vec<BatchCond> },
    IsAutocommit,
}

impl BatchCond {
    fn uses_is_autocommit(&self) -> bool {
        match self {
            BatchCond::IsAutocommit => true,
            BatchCond::Not { cond } => cond.uses_is_autocommit(),
            BatchCond::And { conds } | BatchCond::Or { conds } => {
                conds.iter().any(BatchCond::uses_is_autocommit)
            }
            BatchCond::Ok { .. } | BatchCond::Error { .. } => false,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct BatchResult {
    pub step_results: Vec<Option<StmtResult>>,
    pub step_errors: Vec<Option<Error>>,
}

#[derive(Debug, Serialize)]
pub struct StmtResult {
    pub cols: Vec<Col>,
    pub rows: Vec<Vec<Value>>,
    pub affected_row_count: u64,
    #[serde(serialize_with = "serialize_opt_i64_string")]
    pub last_insert_rowid: Option<i64>,
    pub rows_read: u64,
    pub rows_written: u64,
    pub query_duration_ms: f64,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Col {
    pub name: Option<String>,
    pub decltype: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DescribeResult {
    pub params: Vec<DescribeParam>,
    pub cols: Vec<Col>,
    pub is_explain: bool,
    pub is_readonly: bool,
}

#[derive(Debug, Serialize)]
pub struct DescribeParam {
    pub name: Option<String>,
}

/// An in-band error: an SQL failure reported inside a `200 OK` response.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Error {
    pub message: String,
    pub code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extended_code: Option<String>,
}

impl Error {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: code.to_owned(),
            extended_code: None,
        }
    }
}

/// One line of a cursor response body after the first.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CursorEntry {
    StepBegin {
        step: u32,
        cols: Vec<Col>,
    },
    Row {
        row: Vec<Value>,
    },
    StepEnd {
        affected_row_count: u64,
        #[serde(serialize_with = "serialize_opt_i64_string")]
        last_insert_rowid: Option<i64>,
    },
    StepError {
        step: u32,
        error: Error,
    },
    Error {
        error: Error,
    },
    ReplicationIndex {
        replication_index: Option<u64>,
    },
}

/// An SQL value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Integer(i64),
    Float(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl Serialize for Value {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(2))?;
        match self {
            Value::Null => map.serialize_entry("type", "null")?,
            Value::Integer(value) => {
                map.serialize_entry("type", "integer")?;
                map.serialize_entry("value", &value.to_string())?;
            }
            Value::Float(value) => {
                map.serialize_entry("type", "float")?;
                if value.is_finite() {
                    map.serialize_entry("value", value)?;
                } else {
                    map.serialize_entry("value", &Option::<f64>::None)?;
                }
            }
            Value::Text(value) => {
                map.serialize_entry("type", "text")?;
                map.serialize_entry("value", value)?;
            }
            Value::Blob(value) => {
                map.serialize_entry("type", "blob")?;
                map.serialize_entry("base64", &STANDARD_NO_PAD.encode(value))?;
            }
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for Value {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        enum Wire {
            Null,
            Integer { value: IntegerWire },
            Float { value: Option<f64> },
            Text { value: String },
            Blob { base64: String },
        }

        #[derive(Deserialize)]
        #[serde(untagged)]
        enum IntegerWire {
            String(String),
            Number(i64),
        }

        Ok(match Wire::deserialize(deserializer)? {
            Wire::Null => Value::Null,
            Wire::Integer {
                value: IntegerWire::Number(value),
            } => Value::Integer(value),
            Wire::Integer {
                value: IntegerWire::String(value),
            } => Value::Integer(
                value
                    .parse()
                    .map_err(|_| de::Error::custom("integer value is not a 64-bit integer"))?,
            ),
            Wire::Float { value: Some(value) } => Value::Float(value),
            Wire::Float { value: None } => Value::Float(f64::NAN),
            Wire::Text { value } => Value::Text(value),
            Wire::Blob { base64 } => {
                Value::Blob(decode_base64(&base64).map_err(de::Error::custom)?)
            }
        })
    }
}

/// Accepts standard base64 with or without padding.
fn decode_base64(input: &str) -> Result<Vec<u8>, &'static str> {
    let trimmed = input.trim_end_matches('=');
    STANDARD_NO_PAD
        .decode(trimmed)
        .or_else(|_| STANDARD.decode(input))
        .map_err(|_| "blob value is not valid base64")
}

fn serialize_opt_i64_string<S: Serializer>(
    value: &Option<i64>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match value {
        Some(value) => serializer.serialize_str(&value.to_string()),
        None => serializer.serialize_none(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn values_round_trip_through_the_wire_encoding() {
        let values = vec![
            Value::Null,
            Value::Integer(i64::MAX),
            Value::Integer(i64::MIN),
            Value::Float(1.5),
            Value::Text("héllo".into()),
            Value::Blob(vec![0xde, 0xad, 0xbe, 0xef]),
        ];
        let encoded = serde_json::to_value(&values).unwrap();
        assert_eq!(
            encoded,
            json!([
                {"type": "null"},
                {"type": "integer", "value": "9223372036854775807"},
                {"type": "integer", "value": "-9223372036854775808"},
                {"type": "float", "value": 1.5},
                {"type": "text", "value": "héllo"},
                {"type": "blob", "base64": "3q2+7w"},
            ])
        );
        let decoded: Vec<Value> = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, values);
    }

    #[test]
    fn non_finite_floats_encode_as_null() {
        let encoded = serde_json::to_value(Value::Float(f64::INFINITY)).unwrap();
        assert_eq!(encoded, json!({"type": "float", "value": null}));
        let decoded: Value = serde_json::from_value(encoded).unwrap();
        assert!(matches!(decoded, Value::Float(value) if value.is_nan()));
    }

    #[test]
    fn decoding_accepts_padded_blobs_and_numeric_integers() {
        let padded: Value =
            serde_json::from_value(json!({"type": "blob", "base64": "AQ=="})).unwrap();
        assert_eq!(padded, Value::Blob(vec![1]));
        let number: Value = serde_json::from_value(json!({"type": "integer", "value": 7})).unwrap();
        assert_eq!(number, Value::Integer(7));
        assert!(
            serde_json::from_value::<Value>(json!({"type": "integer", "value": "1.5"})).is_err()
        );
        assert!(serde_json::from_value::<Value>(json!({"type": "blob", "base64": "!"})).is_err());
    }

    #[test]
    fn statement_results_encode_rowids_as_strings() {
        let result = StmtResult {
            cols: vec![],
            rows: vec![],
            affected_row_count: 1,
            last_insert_rowid: Some(42),
            rows_read: 0,
            rows_written: 1,
            query_duration_ms: 0.0,
        };
        assert_eq!(
            serde_json::to_value(result).unwrap()["last_insert_rowid"],
            json!("42")
        );
    }

    #[test]
    fn version_three_requests_are_detected() {
        let get: StreamRequest = serde_json::from_value(json!({"type": "get_autocommit"})).unwrap();
        assert!(get.requires_v3());
        let batch: StreamRequest = serde_json::from_value(json!({
            "type": "batch",
            "batch": {"steps": [
                {"stmt": {"sql": "SELECT 1"}},
                {"condition": {"type": "not", "cond": {"type": "is_autocommit"}}, "stmt": {"sql": "COMMIT"}}
            ]}
        }))
        .unwrap();
        assert!(batch.requires_v3());
        let plain: StreamRequest =
            serde_json::from_value(json!({"type": "execute", "stmt": {"sql": "SELECT 1"}}))
                .unwrap();
        assert!(!plain.requires_v3());
    }
}
