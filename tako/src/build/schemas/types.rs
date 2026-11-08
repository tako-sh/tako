use super::model::{Manifest, Primitive, invalid, pascal};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
};

#[derive(Clone, Debug)]
pub enum Type {
    Any,
    Never,
    Null,
    Bool,
    Number,
    Integer,
    String,
    Named(String),
    Array(Box<Type>),
    Nullable(Box<Type>),
}
#[derive(Clone, Debug)]
pub struct Field {
    pub wire: String,
    pub ty: Type,
    pub optional: bool,
}
#[derive(Clone, Debug)]
pub enum Definition {
    Alias(Type),
    Object {
        fields: Vec<Field>,
        additional: Option<Type>,
    },
    Enum(Vec<Value>),
    Union(Vec<Type>),
    Tuple(Vec<Type>),
}
#[derive(Clone, Debug)]
pub struct Binding {
    pub app: String,
    pub name: String,
    pub kind: &'static str,
    pub symbol: String,
    pub messages: Vec<(String, Type)>,
}
#[derive(Default)]
pub struct Graph {
    pub definitions: BTreeMap<String, Definition>,
    pub bindings: Vec<Binding>,
    names: BTreeMap<String, String>,
    refs: BTreeMap<String, String>,
}

impl Graph {
    pub fn compile(manifests: &[Manifest]) -> io::Result<Self> {
        let mut graph = Self::default();
        for manifest in manifests {
            manifest.validate()?;
            for primitive in &manifest.tako.primitives {
                let symbol = format!(
                    "{}{}{}",
                    pascal(&manifest.tako.app),
                    pascal(primitive.name()),
                    primitive.kind()
                );
                let origin = format!(
                    "{}:{}:{}",
                    manifest.tako.app,
                    primitive.kind(),
                    primitive.name()
                );
                let fields = primitive.fields();
                graph.define(
                    manifest,
                    &fields[0].1.clone(),
                    &format!("{symbol}{}", pascal(fields[0].0)),
                    &format!("{origin}/{}", fields[0].0),
                    0,
                )?;
                graph.define(
                    manifest,
                    &fields[1].1.clone(),
                    &format!("{symbol}{}", pascal(fields[1].0)),
                    &format!("{origin}/{}", fields[1].0),
                    0,
                )?;
                let mut messages = Vec::new();
                if let Primitive::Channel {
                    messages: schema, ..
                } = primitive
                {
                    let schema = manifest.dereference(schema)?;
                    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
                        for (name, payload) in properties {
                            let ty = graph.define(
                                manifest,
                                payload,
                                &format!("{symbol}{}Data", pascal(name)),
                                &format!("{origin}/message/{name}"),
                                0,
                            )?;
                            messages.push((name.clone(), ty));
                        }
                    }
                }
                graph.bindings.push(Binding {
                    app: manifest.tako.app.clone(),
                    name: primitive.name().into(),
                    kind: primitive.kind(),
                    symbol,
                    messages,
                });
            }
        }
        Ok(graph)
    }

    fn define(
        &mut self,
        doc: &Manifest,
        schema: &Value,
        name: &str,
        origin: &str,
        depth: usize,
    ) -> io::Result<Type> {
        if let Some(prior) = self.names.get(name) {
            if prior != origin {
                return Err(invalid(format!(
                    "generated name collision: {name} ({prior}, {origin})"
                )));
            }
            return Ok(Type::Named(name.into()));
        }
        if self.definitions.len() >= 4096 {
            return Err(invalid("schema defines too many types"));
        }
        self.names.insert(name.into(), origin.into());
        self.definitions
            .insert(name.into(), Definition::Alias(Type::Never));
        let definition = self.definition(doc, schema, name, origin, depth + 1)?;
        self.definitions.insert(name.into(), definition);
        Ok(Type::Named(name.into()))
    }

    fn ty(
        &mut self,
        doc: &Manifest,
        schema: &Value,
        name: &str,
        origin: &str,
        depth: usize,
    ) -> io::Result<Type> {
        if depth > 128 {
            return Err(invalid("schema nesting exceeds 128 levels"));
        }
        if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
            let key = format!("{}:{reference}", doc.tako.app);
            if let Some(name) = self.refs.get(&key) {
                return Ok(Type::Named(name.clone()));
            }
            let target = doc.reference(reference)?;
            let ref_name = format!(
                "{}Schema{}",
                pascal(&doc.tako.app),
                pascal(reference.strip_prefix("#/$defs/").unwrap())
            );
            self.refs.insert(key.clone(), ref_name.clone());
            return self.define(doc, target, &ref_name, &key, depth + 1);
        }
        match self.definition(doc, schema, name, origin, depth + 1)? {
            Definition::Alias(ty) => Ok(ty),
            _ => self.define(doc, schema, name, origin, depth + 1),
        }
    }

    fn definition(
        &mut self,
        doc: &Manifest,
        schema: &Value,
        name: &str,
        origin: &str,
        depth: usize,
    ) -> io::Result<Definition> {
        if depth > 128 {
            return Err(invalid("schema nesting exceeds 128 levels"));
        }
        if schema == &Value::Bool(true) || schema.as_object().is_some_and(|m| m.is_empty()) {
            return Ok(Definition::Alias(Type::Any));
        }
        if schema == &Value::Bool(false) {
            return Ok(Definition::Alias(Type::Never));
        }
        let object = schema.as_object().ok_or_else(|| {
            invalid(format!(
                "{origin}: expected a JSON Schema object or boolean"
            ))
        })?;
        for unsupported in [
            "not",
            "if",
            "then",
            "else",
            "patternProperties",
            "dependentSchemas",
            "unevaluatedProperties",
            "$dynamicRef",
        ] {
            if object.contains_key(unsupported) {
                return Err(invalid(format!(
                    "{origin}: unsupported schema keyword {unsupported}"
                )));
            }
        }
        if object.contains_key("$ref") {
            return Ok(Definition::Alias(self.ty(
                doc,
                schema,
                name,
                origin,
                depth + 1,
            )?));
        }
        if let Some(constant) = object.get("const") {
            return Self::enumeration(vec![constant.clone()], origin);
        }
        if let Some(values) = object.get("enum").and_then(Value::as_array) {
            return Self::enumeration(values.clone(), origin);
        }
        if let Some(parts) = object.get("allOf").and_then(Value::as_array) {
            let merged = merge_objects(doc, parts, depth)?;
            return self.definition(doc, &merged, name, origin, depth + 1);
        }
        if let Some(parts) = object
            .get("anyOf")
            .or_else(|| object.get("oneOf"))
            .and_then(Value::as_array)
        {
            return self.union(doc, parts, name, origin, depth);
        }
        if let Some(kinds) = object.get("type").and_then(Value::as_array) {
            let parts: Vec<_> = kinds
                .iter()
                .map(|kind| {
                    let mut v = schema.clone();
                    v["type"] = kind.clone();
                    v
                })
                .collect();
            return self.union(doc, &parts, name, origin, depth);
        }
        let kind = object.get("type").and_then(Value::as_str);
        let alias = match kind {
            Some("null") => Some(Type::Null),
            Some("boolean") => Some(Type::Bool),
            Some("string") => Some(Type::String),
            Some("number") => Some(Type::Number),
            Some("integer") => {
                if object
                    .get("format")
                    .and_then(Value::as_str)
                    .is_some_and(|s| ["int64", "uint64", "int128", "uint128"].contains(&s))
                    && !(object
                        .get("minimum")
                        .and_then(Value::as_f64)
                        .is_some_and(|n| n >= -9_007_199_254_740_991.0)
                        && object
                            .get("maximum")
                            .and_then(Value::as_f64)
                            .is_some_and(|n| n <= 9_007_199_254_740_991.0))
                {
                    return Err(invalid(format!(
                        "{origin}: 64-bit integers must use decimal strings on the wire, or explicit JavaScript-safe bounds"
                    )));
                }
                Some(Type::Integer)
            }
            _ => None,
        };
        if let Some(ty) = alias {
            return Ok(Definition::Alias(ty));
        }
        if kind == Some("array") {
            if let Some(items) = object.get("prefixItems").and_then(Value::as_array) {
                let min = object.get("minItems").and_then(Value::as_u64).unwrap_or(0) as usize;
                let max = object
                    .get("maxItems")
                    .and_then(Value::as_u64)
                    .map(|n| n as usize);
                if min != items.len() || max != Some(items.len()) {
                    return Err(invalid(format!(
                        "{origin}: native bindings require fixed-length tuples; use a homogeneous array or an object for optional/rest tuple elements"
                    )));
                }
                let items = items
                    .iter()
                    .enumerate()
                    .map(|(i, s)| {
                        self.ty(
                            doc,
                            s,
                            &format!("{name}Item{i}"),
                            &format!("{origin}/{i}"),
                            depth + 1,
                        )
                    })
                    .collect::<io::Result<_>>()?;
                return Ok(Definition::Tuple(items));
            }
            let any = Value::Bool(true);
            return Ok(Definition::Alias(Type::Array(Box::new(self.ty(
                doc,
                object.get("items").unwrap_or(&any),
                &format!("{name}Item"),
                &format!("{origin}/items"),
                depth + 1,
            )?))));
        }
        if kind == Some("object") || object.contains_key("properties") {
            let mut fields = Vec::new();
            let required: BTreeSet<_> = object
                .get("required")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            if let Some(properties) = object.get("properties").and_then(Value::as_object) {
                for (wire, schema) in properties {
                    fields.push(Field {
                        wire: wire.clone(),
                        ty: self.ty(
                            doc,
                            schema,
                            &format!("{name}{}", pascal(wire)),
                            &format!("{origin}/{wire}"),
                            depth + 1,
                        )?,
                        optional: !required.contains(wire.as_str()),
                    });
                }
            }
            if required
                .iter()
                .any(|key| !fields.iter().any(|f| f.wire == *key))
            {
                return Err(invalid(format!(
                    "{origin}: required property has no schema"
                )));
            }
            let additional = match object.get("additionalProperties") {
                Some(Value::Bool(false)) => None,
                None | Some(Value::Bool(true)) => Some(Type::Any),
                Some(schema) => Some(self.ty(
                    doc,
                    schema,
                    &format!("{name}Additional"),
                    &format!("{origin}/additional"),
                    depth + 1,
                )?),
            };
            return Ok(Definition::Object { fields, additional });
        }
        if kind.is_none() {
            return Ok(Definition::Alias(Type::Any));
        }
        Err(invalid(format!("{origin}: unsupported schema type")))
    }

    fn enumeration(values: Vec<Value>, origin: &str) -> io::Result<Definition> {
        if values.is_empty() || values.iter().any(|v| v.is_object() || v.is_array()) {
            return Err(invalid(format!(
                "{origin}: enums must contain JSON scalars"
            )));
        }
        Ok(Definition::Enum(values))
    }
    fn union(
        &mut self,
        doc: &Manifest,
        parts: &[Value],
        name: &str,
        origin: &str,
        depth: usize,
    ) -> io::Result<Definition> {
        if parts.is_empty() {
            return Err(invalid("schema union cannot be empty"));
        }
        let non_null: Vec<_> = parts
            .iter()
            .filter(|p| p.get("type").and_then(Value::as_str) != Some("null"))
            .collect();
        if non_null.len() == 1 && parts.len() == 2 {
            return Ok(Definition::Alias(Type::Nullable(Box::new(self.ty(
                doc,
                non_null[0],
                &format!("{name}Value"),
                &format!("{origin}/value"),
                depth + 1,
            )?))));
        }
        Ok(Definition::Union(
            parts
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    self.ty(
                        doc,
                        s,
                        &format!("{name}Variant{i}"),
                        &format!("{origin}/variant{i}"),
                        depth + 1,
                    )
                })
                .collect::<io::Result<_>>()?,
        ))
    }

    pub fn alias_cycle(&self, current: &str, value: &Type) -> bool {
        fn walk(graph: &Graph, current: &str, value: &Type, seen: &mut BTreeSet<String>) -> bool {
            match value {
                Type::Named(name) if name == current => true,
                Type::Named(name) if seen.insert(name.clone()) => match &graph.definitions[name] {
                    Definition::Alias(next) => walk(graph, current, next, seen),
                    _ => false,
                },
                Type::Array(next) | Type::Nullable(next) => walk(graph, current, next, seen),
                _ => false,
            }
        }
        walk(self, current, value, &mut BTreeSet::new())
    }

    /// Vec/Map already provide indirection. Directly recursive fields need a Box/pointer.
    pub fn recursive(&self, current: &str, target: &str) -> bool {
        fn ty(graph: &Graph, current: &str, t: &Type, seen: &mut BTreeSet<String>) -> bool {
            match t {
                Type::Named(name) => walk(graph, current, name, seen),
                Type::Nullable(t) => ty(graph, current, t, seen),
                _ => false,
            }
        }
        fn walk(graph: &Graph, current: &str, target: &str, seen: &mut BTreeSet<String>) -> bool {
            if current == target {
                return true;
            }
            if !seen.insert(target.into()) {
                return false;
            }
            match &graph.definitions[target] {
                Definition::Alias(t) => ty(graph, current, t, seen),
                Definition::Object { fields, .. } => {
                    fields.iter().any(|f| ty(graph, current, &f.ty, seen))
                }
                Definition::Union(ts) | Definition::Tuple(ts) => {
                    ts.iter().any(|t| ty(graph, current, t, seen))
                }
                Definition::Enum(_) => false,
            }
        }
        walk(self, current, target, &mut BTreeSet::new())
    }
}

fn merge_objects(doc: &Manifest, parts: &[Value], depth: usize) -> io::Result<Value> {
    if depth > 128 || parts.is_empty() {
        return Err(invalid("invalid schema intersection"));
    }
    let mut properties = serde_json::Map::new();
    let mut required = BTreeSet::new();
    let mut closed: Option<BTreeSet<String>> = None;
    for part in parts {
        let part = doc.dereference(part)?;
        let part = if let Some(nested) = part.get("allOf").and_then(Value::as_array) {
            merge_objects(doc, nested, depth + 1)?
        } else {
            part.clone()
        };
        if part.get("type").and_then(Value::as_str) != Some("object") {
            return Err(invalid("allOf bindings currently require object schemas"));
        }
        let fields = part
            .get("properties")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        if part.get("additionalProperties") == Some(&Value::Bool(false)) {
            let keys: BTreeSet<String> = fields.keys().cloned().collect();
            closed = Some(closed.map_or(keys.clone(), |prior| {
                prior.intersection(&keys).cloned().collect()
            }));
        } else if part
            .get("additionalProperties")
            .is_some_and(|v| !v.is_boolean())
        {
            return Err(invalid(
                "allOf with typed additional properties requires an explicit object schema",
            ));
        }
        for (key, value) in fields {
            if let Some(previous) = properties.get(&key)
                && previous != &value
            {
                return Err(invalid("conflicting property schemas in allOf"));
            }
            properties.insert(key, value);
        }
        if let Some(keys) = part.get("required").and_then(Value::as_array) {
            required.extend(keys.iter().filter_map(Value::as_str).map(str::to_owned));
        }
    }
    if closed
        .as_ref()
        .is_some_and(|keys| properties.keys().any(|key| !keys.contains(key)))
    {
        return Err(invalid("allOf adds properties to a closed object"));
    }
    Ok(
        json!({"type":"object", "properties":properties, "required":required, "additionalProperties":closed.is_none()}),
    )
}
