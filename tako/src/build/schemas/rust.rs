use super::{
    model::{COMMENT, Manifest, invalid, pascal, snake},
    types::{Definition, Graph, Type},
};
use std::{collections::BTreeSet, io};

pub fn generate(manifests: &[Manifest]) -> io::Result<String> {
    let graph = Graph::compile(manifests)?;
    let mut out = format!(
        "// {COMMENT}\n// Include this module with #[path = \"../tako_imports.rs\"] mod tako_imports;\n#![allow(dead_code)]\n\n"
    );
    for (name, definition) in &graph.definitions {
        match definition {
            Definition::Alias(ty) => {
                if graph.alias_cycle(name, ty) {
                    return Err(invalid(
                        "recursive type aliases require an object or union boundary",
                    ));
                }
                out.push_str(&format!(
                    "pub type {name} = {};\n\n",
                    render(&graph, "", ty)
                ));
            }
            Definition::Object { fields, additional } => {
                out.push_str("#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]\n");
                if additional.is_none() {
                    out.push_str("#[serde(deny_unknown_fields)]\n");
                }
                out.push_str(&format!("pub struct {name} {{\n"));
                let mut names = BTreeSet::from(["additional_properties".to_string()]);
                for field in fields {
                    let field_name = snake(&field.wire);
                    if !names.insert(field_name.clone()) {
                        return Err(invalid(format!(
                            "field name collision in {name}: {}",
                            field.wire
                        )));
                    }
                    let ty = render(&graph, name, &field.ty);
                    out.push_str(&format!("    #[serde(rename = {:?})]\n", field.wire));
                    if field.optional {
                        out.push_str("    #[serde(default, skip_serializing_if = \"tako::network::Optional::is_missing\")]\n");
                        out.push_str(&format!(
                            "    pub {field_name}: tako::network::Optional<{ty}>,\n"
                        ));
                    } else {
                        // Serde normally accepts an absent Option as None. This field
                        // is nullable but required, so missing must still be an error.
                        out.push_str(
                            "    #[serde(deserialize_with = \"tako::network::required\")]\n",
                        );
                        out.push_str(&format!("    pub {field_name}: {ty},\n"));
                    }
                }
                if let Some(ty) = additional {
                    out.push_str(&format!("    #[serde(flatten)]\n    pub additional_properties: std::collections::BTreeMap<String, {}>,\n",render(&graph,name,ty)));
                }
                out.push_str("}\n\n");
            }
            Definition::Union(types) => {
                out.push_str(&format!("#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]\n#[serde(untagged)]\npub enum {name} {{\n"));
                for (i, ty) in types.iter().enumerate() {
                    out.push_str(&format!("    Variant{i}({}),\n", render(&graph, name, ty)));
                }
                out.push_str("}\n\n");
            }
            Definition::Tuple(types) => {
                // A newtype tuple keeps fixed tuples as JSON arrays, including [] and [x].
                let inner = if types.is_empty() {
                    "[serde_json::Value; 0]".into()
                } else {
                    format!(
                        "({},)",
                        types
                            .iter()
                            .map(|t| render(&graph, name, t))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                out.push_str(&format!("#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]\n#[serde(transparent)]\npub struct {name}(pub {inner});\n\n"));
            }
            Definition::Enum(values) => {
                let mut names = BTreeSet::new();
                let variants: Vec<_> = values
                    .iter()
                    .enumerate()
                    .map(|(i, v)| {
                        let mut candidate = v
                            .as_str()
                            .map(pascal)
                            .unwrap_or_else(|| format!("Variant{i}"));
                        if candidate == "Self" {
                            candidate = "ValueSelf".into();
                        }
                        while !names.insert(candidate.clone()) {
                            candidate.push_str(&format!("_{i}"));
                        }
                        candidate
                    })
                    .collect();
                out.push_str(&format!(
                    "#[derive(Debug, Clone, PartialEq, Eq)]\npub enum {name} {{ {} }}\n",
                    variants.join(", ")
                ));
                out.push_str(&format!("impl serde::Serialize for {name} {{\n    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {{\n        let text = match self {{\n"));
                for (variant, value) in variants.iter().zip(values) {
                    out.push_str(&format!(
                        "            Self::{variant} => {:?},\n",
                        value.to_string()
                    ));
                }
                out.push_str("        };\n        let value: serde_json::Value = serde_json::from_str(text).map_err(serde::ser::Error::custom)?;\n        serde::Serialize::serialize(&value, serializer)\n    }\n}\n");
                out.push_str(&format!("impl<'de> serde::Deserialize<'de> for {name} {{\n    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {{\n        let value = <serde_json::Value as serde::Deserialize>::deserialize(deserializer)?;\n"));
                for (variant, value) in variants.iter().zip(values) {
                    // JSON Schema compares numbers by value, so 1, 1.0, and 1e0
                    // must select the same variant. Only use float comparison
                    // in the portable range; larger integers must stay exact.
                    let comparison = match value.as_f64() {
                        Some(number) if number.abs() <= 9_007_199_254_740_991.0 => {
                            format!("value.as_f64() == Some({number:?})")
                        }
                        _ => format!(
                            "value == serde_json::from_str::<serde_json::Value>({:?}).map_err(serde::de::Error::custom)?",
                            value.to_string()
                        ),
                    };
                    out.push_str(&format!(
                        "        if {comparison} {{ return Ok(Self::{variant}); }}\n"
                    ));
                }
                out.push_str(
                    "        Err(serde::de::Error::custom(\"invalid enum value\"))\n    }\n}\n\n",
                );
            }
        }
    }
    for binding in &graph.bindings {
        out.push_str(&format!(
            "/// {} in app {}.\npub const {}_ADDRESS: &str = {:?};\n",
            binding.kind,
            binding.app,
            snake(&binding.symbol).to_uppercase(),
            format!("{}.{}", binding.app, binding.name)
        ));
        for (message, _) in &binding.messages {
            out.push_str(&format!(
                "pub const {}_{}_MESSAGE: &str = {:?};\n",
                snake(&binding.symbol).to_uppercase(),
                snake(message).to_uppercase(),
                message
            ));
        }
    }
    Ok(out)
}

fn render(graph: &Graph, current: &str, ty: &Type) -> String {
    match ty {
        Type::Any => "serde_json::Value".into(),
        Type::Never => "tako::network::Never".into(),
        Type::Null => "()".into(),
        Type::Bool => "bool".into(),
        Type::Number => "f64".into(),
        Type::Integer => "i64".into(),
        Type::String => "String".into(),
        Type::Named(name) => {
            if graph.recursive(current, name) {
                format!("Box<{name}>")
            } else {
                name.clone()
            }
        }
        Type::Array(ty) => format!("Vec<{}>", render(graph, "", ty)),
        Type::Nullable(ty) => format!("Option<{}>", render(graph, current, ty)),
    }
}
