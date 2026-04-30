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

    let value = Value::Object(obj);
    let validator = jsonschema::draft202012::new(schema)
        .map_err(|error| ParamsError::InvalidSchema(error.to_string()))?;

    if let Err(error) = validator.validate(&value) {
        return Err(ParamsError::Invalid(error.to_string()));
    }

    Ok(value)
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
    fn decodes_percent_and_plus_encoded_values() {
        let value = validate_query(&schema(), "roomId=room+one%2Ftwo").unwrap();
        assert_eq!(value["roomId"], "room one/two");
    }
}
