use super::{
    model::{COMMENT, Manifest, invalid, pascal},
    types::{Definition, Graph, Type},
};
use serde_json::Value;
use std::{collections::BTreeSet, io};

pub fn generate(manifests: &[Manifest], package: &str) -> io::Result<String> {
    if package.is_empty()
        || !package
            .bytes()
            .enumerate()
            .all(|(i, c)| c == b'_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit()))
    {
        return Err(invalid("invalid Go package name"));
    }
    let graph = Graph::compile(manifests)?;
    let mut out = format!("// {COMMENT}\npackage {package}\n\n");
    if graph.definitions.is_empty() {
        return Ok(out);
    }
    out.push_str(
        "import (\n\"bytes\"\n\"encoding/json\"\n\"fmt\"\n\"reflect\"\n\"tako.sh\"\n)\n\n",
    );
    out.push_str(r#"// Helpers preserve JSON presence, required properties, and enum values.
var _ = tako.Null{}
func takoSchemaNull(data []byte) bool { return bytes.Equal(bytes.TrimSpace(data), []byte("null")) }
func takoSchemaEnum(data []byte, allowed ...string) bool {
    var value any
    if json.Unmarshal(data, &value) != nil { return false }
    for _, text := range allowed { var candidate any; if json.Unmarshal([]byte(text), &candidate) == nil && reflect.DeepEqual(value, candidate) { return true } }
    return false
}
func takoSchemaObject(data []byte) (map[string]json.RawMessage, error) {
    if takoSchemaNull(data) { return nil, fmt.Errorf("expected object, got null") }
    var fields map[string]json.RawMessage
    if err := json.Unmarshal(data, &fields); err != nil { return nil, err }
    return fields, nil
}

"#);
    let mut globals: BTreeSet<String> = graph.definitions.keys().cloned().collect();
    for (name, definition) in &graph.definitions {
        match definition {
            Definition::Alias(ty) => {
                if graph.alias_cycle(name, ty) {
                    return Err(invalid(
                        "recursive type aliases require an object or union boundary",
                    ));
                }
                out.push_str(&format!("type {name} = {}\n\n", render(&graph, "", ty)));
            }
            Definition::Object { fields, additional } => {
                out.push_str(&format!("type {name} struct {{\n"));
                let mut used = BTreeSet::from([
                    "AdditionalProperties".to_string(),
                    "MarshalJSON".into(),
                    "UnmarshalJSON".into(),
                ]);
                for field in fields {
                    let field_name = pascal(&field.wire);
                    if !used.insert(field_name.clone()) {
                        return Err(invalid(format!(
                            "field name collision in {name}: {}",
                            field.wire
                        )));
                    }
                    let ty = render(&graph, name, &field.ty);
                    out.push_str(&format!(
                        "    {field_name} {}\n",
                        if field.optional {
                            format!("tako.Optional[{ty}]")
                        } else {
                            ty
                        }
                    ));
                }
                if let Some(ty) = additional {
                    out.push_str(&format!(
                        "    AdditionalProperties map[string]{}\n",
                        render(&graph, "", ty)
                    ));
                }
                out.push_str("}\n");
                out.push_str(&format!("func (v {name}) MarshalJSON() ([]byte, error) {{\n    fields := map[string]json.RawMessage{{}}\n"));
                if additional.is_some() {
                    out.push_str("    for key, value := range v.AdditionalProperties { data, err := json.Marshal(value); if err != nil { return nil, err }; fields[key] = data }\n");
                }
                for field in fields {
                    let field_name = pascal(&field.wire);
                    let key = quote(&field.wire);
                    out.push_str(&format!("    delete(fields, {key})\n"));
                    if field.optional {
                        out.push_str(&format!("    if v.{field_name}.Set {{\n"));
                    } else {
                        out.push_str("    {\n");
                    }
                    out.push_str(&format!("        data, err := json.Marshal(v.{field_name}{})\n        if err != nil {{ return nil, err }}\n",if field.optional {".Value"}else{""}));
                    if !nullable(&graph, &field.ty, &mut BTreeSet::new()) {
                        out.push_str(&format!("        if takoSchemaNull(data) {{ return nil, fmt.Errorf(\"null is not allowed for %s\", {key}) }}\n"));
                    }
                    out.push_str(&format!("        fields[{key}] = data\n    }}\n"));
                }
                out.push_str("    return json.Marshal(fields)\n}\n");
                out.push_str(&format!("func (v *{name}) UnmarshalJSON(data []byte) error {{\n    fields, err := takoSchemaObject(data)\n    if err != nil {{ return err }}\n    var next {name}\n"));
                for field in fields {
                    let field_name = pascal(&field.wire);
                    let key = quote(&field.wire);
                    out.push_str(&format!("    if data, ok := fields[{key}]; ok {{\n"));
                    if !nullable(&graph, &field.ty, &mut BTreeSet::new()) {
                        out.push_str(&format!("        if takoSchemaNull(data) {{ return fmt.Errorf(\"null is not allowed for %s\", {key}) }}\n"));
                    }
                    out.push_str(&format!("        if err := json.Unmarshal(data, &next.{field_name}{}); err != nil {{ return fmt.Errorf(\"%s: %w\", {key}, err) }}\n",if field.optional {".Value"}else{""}));
                    if field.optional {
                        out.push_str(&format!("        next.{field_name}.Set = true\n"));
                    }
                    out.push_str(&format!("        delete(fields, {key})\n    }}"));
                    if !field.optional {
                        out.push_str(&format!(
                            " else {{ return fmt.Errorf(\"missing required property %s\", {key}) }}"
                        ));
                    }
                    out.push('\n');
                }
                if let Some(ty) = additional {
                    let ty = render(&graph, "", ty);
                    out.push_str(&format!("    next.AdditionalProperties = make(map[string]{ty}, len(fields))\n    for key, data := range fields {{ var value {ty}; if err := json.Unmarshal(data, &value); err != nil {{ return err }}; next.AdditionalProperties[key] = value }}\n"));
                } else {
                    out.push_str(
                        "    if len(fields) != 0 { return fmt.Errorf(\"unknown properties\") }\n",
                    );
                }
                out.push_str("    *v = next\n    return nil\n}\n\n");
            }
            Definition::Tuple(types) => {
                out.push_str(&format!("type {name} struct {{\n"));
                for (i, ty) in types.iter().enumerate() {
                    out.push_str(&format!("    Item{i} {}\n", render(&graph, name, ty)));
                }
                out.push_str(&format!("}}\nfunc (v {name}) MarshalJSON() ([]byte, error) {{ return json.Marshal([]any{{{}}}) }}\n",(0..types.len()).map(|i|format!("v.Item{i}")).collect::<Vec<_>>().join(", ")));
                out.push_str(&format!("func (v *{name}) UnmarshalJSON(data []byte) error {{\n    if takoSchemaNull(data) {{ return fmt.Errorf(\"expected tuple\") }}\n    var items []json.RawMessage\n    if err := json.Unmarshal(data, &items); err != nil {{ return err }}\n    if len(items) != {} {{ return fmt.Errorf(\"incorrect tuple length\") }}\n    var next {name}\n",types.len()));
                for (i, ty) in types.iter().enumerate() {
                    if !nullable(&graph, ty, &mut BTreeSet::new()) {
                        out.push_str(&format!("    if takoSchemaNull(items[{i}]) {{ return fmt.Errorf(\"null tuple item\") }}\n"));
                    }
                    out.push_str(&format!("    if err := json.Unmarshal(items[{i}], &next.Item{i}); err != nil {{ return err }}\n"));
                }
                out.push_str("    *v = next\n    return nil\n}\n\n");
            }
            Definition::Union(types) => {
                out.push_str(&format!("type {name} struct {{\n"));
                for (i, ty) in types.iter().enumerate() {
                    out.push_str(&format!("    Variant{i} *{}\n", render(&graph, "", ty)));
                }
                out.push_str(&format!("}}\nfunc (v {name}) MarshalJSON() ([]byte, error) {{\n    count := 0\n    var value any\n"));
                for i in 0..types.len() {
                    out.push_str(&format!(
                        "    if v.Variant{i} != nil {{ count++; value = *v.Variant{i} }}\n"
                    ));
                }
                out.push_str("    if count != 1 { return nil, fmt.Errorf(\"select exactly one union variant\") }\n    return json.Marshal(value)\n}\n");
                out.push_str(&format!(
                    "func (v *{name}) UnmarshalJSON(data []byte) error {{\n"
                ));
                for (i, ty) in types.iter().enumerate() {
                    out.push_str(&format!("    {{ var candidate {}; if {}json.Unmarshal(data, &candidate) == nil {{ *v = {name}{{Variant{i}: &candidate}}; return nil }} }}\n",render(&graph,"",ty),if nullable(&graph,ty,&mut BTreeSet::new()){ "" }else{"!takoSchemaNull(data) && "}));
                }
                out.push_str("    return fmt.Errorf(\"no matching union variant\")\n}\n\n");
            }
            Definition::Enum(values) => {
                let primitive = if values.iter().all(Value::is_string) {
                    Some("string")
                } else if values.iter().all(Value::is_number) {
                    Some("float64")
                } else if values.iter().all(Value::is_boolean) {
                    Some("bool")
                } else {
                    None
                };
                let allowed = values
                    .iter()
                    .map(|v| quote(&v.to_string()))
                    .collect::<Vec<_>>()
                    .join(", ");
                if let Some(primitive) = primitive {
                    out.push_str(&format!("type {name} {primitive}\nconst (\n"));
                    let mut used = BTreeSet::new();
                    for (i, v) in values.iter().enumerate() {
                        let mut candidate = v
                            .as_str()
                            .map(pascal)
                            .unwrap_or_else(|| format!("Variant{i}"));
                        while !used.insert(candidate.clone()) {
                            candidate.push_str(&format!("Value{i}"));
                        }
                        let variant = candidate;
                        if !globals.insert(format!("{name}{variant}")) {
                            return Err(invalid(
                                "generated Go enum constant conflicts with a type",
                            ));
                        }
                        out.push_str(&format!("    {name}{variant} {name} = {v}\n"));
                    }
                    out.push_str(&format!(")\nfunc (v {name}) MarshalJSON() ([]byte, error) {{\n    data, err := json.Marshal({primitive}(v))\n    if err != nil {{ return nil, err }}\n    if !takoSchemaEnum(data, {allowed}) {{ return nil, fmt.Errorf(\"invalid enum value\") }}\n    return data, nil\n}}\nfunc (v *{name}) UnmarshalJSON(data []byte) error {{\n    if !takoSchemaEnum(data, {allowed}) {{ return fmt.Errorf(\"invalid enum value\") }}\n    var value {primitive}\n    if err := json.Unmarshal(data, &value); err != nil {{ return err }}\n    *v = {name}(value)\n    return nil\n}}\n\n"));
                } else {
                    out.push_str(&format!("type {name} struct {{ Value json.RawMessage }}\nfunc (v {name}) MarshalJSON() ([]byte, error) {{ if !takoSchemaEnum(v.Value, {allowed}) {{ return nil, fmt.Errorf(\"invalid enum value\") }}; return v.Value, nil }}\nfunc (v *{name}) UnmarshalJSON(data []byte) error {{ if !takoSchemaEnum(data, {allowed}) {{ return fmt.Errorf(\"invalid enum value\") }}; v.Value = append(v.Value[:0], data...); return nil }}\n\n"));
                }
            }
        }
    }
    for binding in &graph.bindings {
        out.push_str(&format!(
            "const {}Address = {}\n",
            binding.symbol,
            quote(&format!("{}.{}", binding.app, binding.name))
        ));
        for (message, _) in &binding.messages {
            out.push_str(&format!(
                "const {}{}Message = {}\n",
                binding.symbol,
                pascal(message),
                quote(message)
            ));
        }
    }
    Ok(out)
}
fn quote(value: &str) -> String {
    serde_json::to_string(value).unwrap()
}
fn render(graph: &Graph, current: &str, ty: &Type) -> String {
    match ty {
        Type::Any => "json.RawMessage".into(),
        Type::Never => "tako.Never".into(),
        Type::Null => "tako.Null".into(),
        Type::Bool => "bool".into(),
        Type::Number => "float64".into(),
        Type::Integer => "int64".into(),
        Type::String => "string".into(),
        Type::Named(name) => {
            if graph.recursive(current, name) {
                format!("*{name}")
            } else {
                name.clone()
            }
        }
        Type::Array(ty) => format!("tako.Array[{}]", render(graph, "", ty)),
        Type::Nullable(ty) => format!("*{}", render(graph, current, ty)),
    }
}
fn nullable(graph: &Graph, ty: &Type, seen: &mut BTreeSet<String>) -> bool {
    match ty {
        Type::Any | Type::Null | Type::Nullable(_) => true,
        Type::Named(name) if seen.insert(name.clone()) => match graph.definitions.get(name) {
            Some(Definition::Alias(t)) => nullable(graph, t, seen),
            Some(Definition::Union(types)) => types.iter().any(|t| nullable(graph, t, seen)),
            Some(Definition::Enum(values)) => values.iter().any(Value::is_null),
            _ => false,
        },
        _ => false,
    }
}
