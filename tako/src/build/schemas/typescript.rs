use super::model::{COMMENT, Manifest, invalid, pascal};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
};

pub fn generate(manifests: &[Manifest]) -> io::Result<String> {
    let mut output = format!("// {COMMENT}\n\n");
    let mut used = BTreeSet::new();
    for doc in manifests {
        doc.validate()?;
        let names: BTreeMap<_, _> = doc
            .definitions
            .keys()
            .map(|key| {
                (
                    key.clone(),
                    format!("_{}{}", pascal(&doc.tako.app), pascal(key)),
                )
            })
            .collect();
        for (key, name) in &names {
            if !used.insert(name.clone()) {
                return Err(invalid(format!("generated name collision: {name}")));
            }
            output.push_str(&format!(
                "type {name} = {};\n",
                render(doc, &doc.definitions[key], &names, 0)?
            ));
        }
        for primitive in &doc.tako.primitives {
            let name = format!(
                "{}{}{}",
                pascal(&doc.tako.app),
                pascal(primitive.name()),
                primitive.kind()
            );
            if !used.insert(name.clone()) {
                return Err(invalid(format!("generated name collision: {name}")));
            }
            output.push_str(&format!(
                "export type {name} = {{ readonly kind: {}; readonly address: {};",
                quote(&primitive.kind().to_lowercase()),
                quote(&format!("{}.{}", doc.tako.app, primitive.name()))
            ));
            for (field, schema) in primitive.fields() {
                output.push_str(&format!(" {field}: {};", render(doc, schema, &names, 0)?));
            }
            output.push_str(" };\n");
        }
    }
    if manifests.iter().all(|doc| doc.tako.primitives.is_empty()) {
        output.push_str("export {};\n");
    }
    Ok(output)
}

fn quote(s: &str) -> String {
    serde_json::to_string(s).unwrap()
}
fn render(
    doc: &Manifest,
    value: &Value,
    names: &BTreeMap<String, String>,
    depth: usize,
) -> io::Result<String> {
    if depth > 128 {
        return Err(invalid("schema nesting exceeds 128 levels"));
    }
    if value == &Value::Bool(true) {
        return Ok("unknown".into());
    }
    if value == &Value::Bool(false) {
        return Ok("never".into());
    }
    let object = value
        .as_object()
        .ok_or_else(|| invalid("expected a JSON Schema object or boolean"))?;
    if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
        doc.reference(reference)?;
        let key = reference
            .strip_prefix("#/$defs/")
            .unwrap()
            .replace("~1", "/")
            .replace("~0", "~");
        return Ok(names[&key].clone());
    }
    for key in [
        "not",
        "if",
        "then",
        "else",
        "patternProperties",
        "dependentSchemas",
        "unevaluatedProperties",
        "$dynamicRef",
    ] {
        if object.contains_key(key) {
            return Err(invalid(format!("unsupported schema keyword {key}")));
        }
    }
    if let Some(constant) = object.get("const") {
        return literal(constant);
    }
    if let Some(values) = object.get("enum").and_then(Value::as_array) {
        if values.is_empty() {
            return Err(invalid("enum cannot be empty"));
        }
        return Ok(format!(
            "({})",
            values
                .iter()
                .map(literal)
                .collect::<io::Result<Vec<_>>>()?
                .join(" | ")
        ));
    }
    for (key, operator) in [("anyOf", " | "), ("oneOf", " | "), ("allOf", " & ")] {
        if let Some(parts) = object.get(key).and_then(Value::as_array) {
            if parts.is_empty() {
                return Err(invalid("schema union/intersection cannot be empty"));
            }
            return Ok(format!(
                "({})",
                parts
                    .iter()
                    .map(|v| render(doc, v, names, depth + 1))
                    .collect::<io::Result<Vec<_>>>()?
                    .join(operator)
            ));
        }
    }
    if let Some(kinds) = object.get("type").and_then(Value::as_array) {
        return render(
            doc,
            &serde_json::json!({"anyOf": kinds.iter().map(|kind| { let mut s=value.clone(); s["type"]=kind.clone(); s }).collect::<Vec<_>>()}),
            names,
            depth + 1,
        );
    }
    let kind = object.get("type").and_then(Value::as_str);
    Ok(match kind {
        Some("null") => "null".into(),
        Some("boolean") => "boolean".into(),
        Some("number" | "integer") => {
            if object
                .get("format")
                .and_then(Value::as_str)
                .is_some_and(|s| ["int64", "uint64", "int128", "uint128"].contains(&s))
                && !(object
                    .get("minimum")
                    .and_then(Value::as_f64)
                    .is_some_and(|v| v >= -9_007_199_254_740_991.0)
                    && object
                        .get("maximum")
                        .and_then(Value::as_f64)
                        .is_some_and(|v| v <= 9_007_199_254_740_991.0))
            {
                return Err(invalid(
                    "64-bit integers must use decimal strings or JavaScript-safe bounds",
                ));
            }
            "number".into()
        }
        Some("string") => {
            if let Some(template) = object.get("x-tako-template") {
                let texts = template
                    .get("texts")
                    .and_then(Value::as_array)
                    .ok_or_else(|| invalid("invalid template texts"))?;
                let types = template
                    .get("types")
                    .and_then(Value::as_array)
                    .ok_or_else(|| invalid("invalid template types"))?;
                if texts.len() != types.len() + 1 {
                    return Err(invalid("invalid template lengths"));
                }
                let mut result = "`".to_string();
                for (i, text) in texts.iter().enumerate() {
                    result.push_str(
                        &text
                            .as_str()
                            .ok_or_else(|| invalid("invalid template text"))?
                            .replace('\\', "\\\\")
                            .replace('`', "\\`")
                            .replace("${", "\\${"),
                    );
                    if let Some(ty) = types.get(i) {
                        let resolved = doc.dereference(ty)?;
                        if !matches!(
                            resolved.get("type").and_then(Value::as_str),
                            Some("string" | "number" | "integer" | "boolean")
                        ) && resolved.get("const").is_none()
                            && resolved.get("enum").is_none()
                        {
                            return Err(invalid("unsupported template placeholder"));
                        }
                        result.push_str(&format!("${{{}}}", render(doc, ty, names, depth + 1)?));
                    }
                }
                result.push('`');
                result
            } else {
                "string".into()
            }
        }
        Some("array") => {
            if let Some(prefix) = object.get("prefixItems").and_then(Value::as_array) {
                let min = object.get("minItems").and_then(Value::as_u64).unwrap_or(0) as usize;
                let mut fields = prefix
                    .iter()
                    .enumerate()
                    .map(|(i, v)| {
                        Ok(format!(
                            "({}){}",
                            render(doc, v, names, depth + 1)?,
                            if i >= min { "?" } else { "" }
                        ))
                    })
                    .collect::<io::Result<Vec<_>>>()?;
                if object.get("items") != Some(&Value::Bool(false))
                    && object.get("maxItems").and_then(Value::as_u64) != Some(prefix.len() as u64)
                {
                    fields.push(format!(
                        "...({})[]",
                        render(
                            doc,
                            object.get("items").unwrap_or(&Value::Bool(true)),
                            names,
                            depth + 1
                        )?
                    ));
                }
                format!(
                    "{}[{}]",
                    if object.get("readOnly") == Some(&Value::Bool(true)) {
                        "readonly "
                    } else {
                        ""
                    },
                    fields.join(", ")
                )
            } else {
                format!(
                    "{}<{}>",
                    if object.get("readOnly") == Some(&Value::Bool(true)) {
                        "ReadonlyArray"
                    } else {
                        "Array"
                    },
                    render(
                        doc,
                        object.get("items").unwrap_or(&Value::Bool(true)),
                        names,
                        depth + 1
                    )?
                )
            }
        }
        Some("object") | None if kind == Some("object") || object.contains_key("properties") => {
            let mut fields = Vec::new();
            let required: BTreeSet<_> = object
                .get("required")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            if let Some(properties) = object.get("properties").and_then(Value::as_object) {
                for (key, schema) in properties {
                    fields.push(format!(
                        "{}{}: {};",
                        quote(key),
                        if required.contains(key.as_str()) {
                            ""
                        } else {
                            "?"
                        },
                        render(doc, schema, names, depth + 1)?
                    ));
                }
            }
            match object.get("additionalProperties") {
                Some(Value::Bool(false)) => {}
                None | Some(Value::Bool(true)) => fields.push("[key: string]: unknown;".into()),
                Some(schema) => fields.push(format!(
                    "[key: string]: {};",
                    render(doc, schema, names, depth + 1)?
                )),
            }
            format!("{{ {} }}", fields.join(" "))
        }
        None => "unknown".into(),
        _ => return Err(invalid("unsupported JSON Schema type")),
    })
}
fn literal(value: &Value) -> io::Result<String> {
    if value.is_object() || value.is_array() {
        return Err(invalid("type literals must be JSON scalars"));
    }
    Ok(value.to_string())
}
