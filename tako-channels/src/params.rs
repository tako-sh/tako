use percent_encoding::percent_decode_str;
use serde_json::{Map, Number, Value};

#[derive(Debug, thiserror::Error)]
pub enum ParamsError {
    #[error("invalid schema: {0}")]
    InvalidSchema(String),
    #[error("invalid params: {0}")]
    Invalid(String),
}

/// Coerce a query string into a JSON object that matches the schema's expected
/// scalar property types, then validate it against JSON Schema draft 2020-12.
pub fn validate_query(schema: &Value, query: &str) -> Result<Value, ParamsError> {
    let mut obj = Map::new();

    if !query.is_empty() {
        for pair in query.split('&') {
            let mut parts = pair.splitn(2, '=');
            let key = decode_query_component(parts.next().unwrap_or_default(), "key")?;
            let raw = decode_query_component(parts.next().unwrap_or_default(), "value")?;
            obj.insert(key.clone(), coerce_property(schema, &key, &raw));
        }
    }

    reject_undeclared(schema, &obj)?;
    let value = Value::Object(obj);
    let validator = jsonschema::draft202012::new(schema)
        .map_err(|error| ParamsError::InvalidSchema(error.to_string()))?;

    if let Err(error) = validator.validate(&value) {
        return Err(ParamsError::Invalid(error.to_string()));
    }

    Ok(value)
}

/// The params part of a channel route's query. `last_message_id` (the
/// WebSocket replay cursor) and `from` (the replay start) are never params.
pub fn channel_params_query(query: Option<&str>) -> String {
    let Some(query) = query else {
        return String::new();
    };

    query
        .split('&')
        .filter(|pair| {
            let key = pair.split_once('=').map_or(*pair, |(key, _)| key);
            // Compare decoded keys: an encoded spelling must not become a
            // param. Undecodable keys stay, so validation rejects them.
            decode_query_component(key, "key").map_or(true, |key| {
                !crate::stream::RESERVED_PARAM_NAMES.contains(&key.as_str())
            })
        })
        .filter(|pair| !pair.is_empty())
        .collect::<Vec<_>>()
        .join("&")
}

/// Validate already typed params, such as those of a server-side publish,
/// against the same schema that query params are checked with.
pub fn validate_value(schema: &Value, params: &Value) -> Result<(), ParamsError> {
    if let Some(params) = params.as_object() {
        reject_undeclared(schema, params)?;
    }
    let validator = jsonschema::draft202012::new(schema)
        .map_err(|error| ParamsError::InvalidSchema(error.to_string()))?;
    validator
        .validate(params)
        .map_err(|error| ParamsError::Invalid(error.to_string()))
}

/// Params select streams, so each value of a param the schema does not
/// declare would start a new one. Only a schema that explicitly sets
/// `additionalProperties: true` accepts params outside its `properties`,
/// whatever JSON Schema's own default; a schema without properties accepts none.
fn reject_undeclared(schema: &Value, params: &Map<String, Value>) -> Result<(), ParamsError> {
    if schema.get("additionalProperties") == Some(&Value::Bool(true)) {
        return Ok(());
    }
    let declared = schema.get("properties").and_then(Value::as_object);
    match params
        .keys()
        .find(|key| !declared.is_some_and(|declared| declared.contains_key(*key)))
    {
        Some(key) => Err(ParamsError::Invalid(format!(
            "unknown param '{key}': the channel's params schema does not declare it"
        ))),
        None => Ok(()),
    }
}

fn decode_query_component(value: &str, label: &str) -> Result<String, ParamsError> {
    let plus_decoded = value.replace('+', " ");
    percent_decode_str(&plus_decoded)
        .decode_utf8()
        .map_err(|_| ParamsError::Invalid(format!("invalid {label} encoding")))
        .map(|value| value.into_owned())
}

fn coerce_property(schema: &Value, key: &str, raw: &str) -> Value {
    match property_type(schema, key) {
        Some("integer") => raw
            .parse::<i64>()
            .map(Value::from)
            .unwrap_or_else(|_| Value::String(raw.to_string())),
        Some("number") => raw
            .parse::<f64>()
            .ok()
            .and_then(Number::from_f64)
            .map(Value::Number)
            .unwrap_or_else(|| Value::String(raw.to_string())),
        Some("boolean") => match raw {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => Value::String(raw.to_string()),
        },
        _ => Value::String(raw.to_string()),
    }
}

fn property_type<'a>(schema: &'a Value, key: &str) -> Option<&'a str> {
    schema.get("properties")?.get(key)?.get("type")?.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "roomId": { "type": "string", "minLength": 1 },
                "limit": { "type": "integer" },
                "pinned": { "type": "boolean" }
            },
            "required": ["roomId"]
        })
    }

    #[test]
    fn accepts_valid_query() {
        let value = validate_query(&schema(), "roomId=room-9&limit=10&pinned=true").unwrap();
        assert_eq!(value["roomId"], "room-9");
        assert_eq!(value["limit"], 10);
        assert_eq!(value["pinned"], true);
    }

    #[test]
    fn rejects_missing_required() {
        let error = validate_query(&schema(), "limit=10").unwrap_err();
        assert!(matches!(error, ParamsError::Invalid(_)), "got {error:?}");
    }

    #[test]
    fn rejects_wrong_type() {
        let error = validate_query(&schema(), "roomId=room-9&limit=ten").unwrap_err();
        assert!(matches!(error, ParamsError::Invalid(_)), "got {error:?}");
    }

    #[test]
    fn rejects_empty_string_when_schema_requires_min_length() {
        let error = validate_query(&schema(), "roomId=").unwrap_err();
        assert!(matches!(error, ParamsError::Invalid(_)), "got {error:?}");
    }

    #[test]
    fn empty_query_with_empty_schema() {
        let schema = json!({ "type": "object" });
        let value = validate_query(&schema, "").unwrap();
        assert_eq!(value, json!({}));
    }

    #[test]
    fn typed_params_use_the_query_schema_without_coercion() {
        validate_value(&schema(), &json!({"roomId": "room-9", "limit": 10})).unwrap();
        for params in [
            json!({"limit": 10}),
            json!({"roomId": "room-9", "limit": "10"}),
            json!({"roomId": ""}),
        ] {
            let error = validate_value(&schema(), &params).unwrap_err();
            assert!(matches!(error, ParamsError::Invalid(_)), "got {error:?}");
        }
    }

    #[test]
    fn undeclared_params_are_rejected_unless_the_schema_allows_them() {
        // Each distinct value of an undeclared param would start a stream.
        for schema in [
            schema(),
            json!({"type": "object", "properties": {"roomId": {"type": "string"}}}),
        ] {
            let error = validate_query(&schema, "roomId=a&junk=1").unwrap_err();
            assert!(
                matches!(&error, ParamsError::Invalid(message) if message.contains("'junk'")),
                "got {error:?}"
            );
            let error = validate_value(&schema, &json!({"roomId": "a", "junk": 1})).unwrap_err();
            assert!(matches!(error, ParamsError::Invalid(_)), "got {error:?}");
        }
        // A channel without a params schema accepts no params.
        for schema in [
            json!({"type": "object"}),
            json!({"type": "object", "properties": {}}),
        ] {
            assert_eq!(validate_query(&schema, "").unwrap(), json!({}));
            assert!(validate_query(&schema, "junk=1").is_err());
            assert!(validate_value(&schema, &json!({"junk": 1})).is_err());
        }
        let open = json!({"type": "object", "additionalProperties": true});
        assert_eq!(validate_query(&open, "any=1").unwrap(), json!({"any": "1"}));
        validate_value(&open, &json!({"any": 1})).unwrap();
        // Reserved names are not params and keep working.
        let query = channel_params_query(Some("roomId=a&from=start&last_message_id=x"));
        assert_eq!(
            validate_query(&schema(), &query).unwrap(),
            json!({"roomId": "a"})
        );
    }

    #[test]
    fn query_and_json_numbers_bind_the_same_stream() {
        // A subscriber's query value is parsed as text; a publisher's arrives
        // as a JSON number. Both must yield the same f64, or the two name
        // different streams.
        let schema = json!({"type": "object", "properties": {"x": {"type": "number"}}});
        let mut cases: Vec<String> = [
            "0.1",
            "1e23",
            "2.2250738585072011e-308",
            "2.2250738585072014e-308",
            "4.9406564584124654e-324",
            "1.7976931348623157e308",
            "0.30000000000000004",
            "123456789.12345678",
            "8.98846567431158e307",
        ]
        .map(String::from)
        .to_vec();
        // Deterministic pseudo-random 17 to 20 digit decimals over a wide
        // exponent range: the inputs where fast float parsing rounds wrong.
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..20_000 {
            let digits = 17 + (next() % 4) as usize;
            let mantissa: String = (0..digits)
                .map(|_| char::from(b'0' + (next() % 10) as u8))
                .collect();
            let exponent = (next() % 600) as i64 - 300;
            cases.push(format!("{}.{}e{exponent}", &mantissa[..1], &mantissa[1..]));
        }
        for raw in cases {
            let queried = validate_query(&schema, &format!("x={raw}")).unwrap();
            let published = json!({ "x": serde_json::from_str::<Value>(&raw).unwrap() });
            assert_eq!(
                crate::ChannelStream::new("c", &queried),
                crate::ChannelStream::new("c", &published),
                "{raw}: query {queried} vs JSON {published}"
            );
        }
    }

    #[test]
    fn decodes_percent_and_plus_encoded_values() {
        let value = validate_query(&schema(), "roomId=room+one%2Ftwo").unwrap();
        assert_eq!(value["roomId"], "room one/two");
    }
}
