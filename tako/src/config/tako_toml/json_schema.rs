//! JSON Schema for `tako.toml`, published at <https://tako.sh/schemas/tako-toml.json>.
//!
//! Sections that the parser deserializes with Serde derive their schema from
//! the Rust types. Sections the parser reads by hand (top-level keys, `[vars]`,
//! `[workflows]`, `[servers]`) are written out here. The schema may be looser
//! than full validation, but it must accept every config the parser accepts:
//! in tests, every successful `TakoToml::parse` also validates its input
//! against this schema.
//!
//! The checked-in copy lives in `website/public/schemas/tako-toml.json`. The
//! drift test compares parsed JSON, so `bun run fmt` may reformat the file.
//! Regenerate it with
//! `TAKO_UPDATE_SCHEMA=1 cargo test -p tako-cli --bin tako tako_toml_json_schema`.

use super::schema::*;
use schemars::{JsonSchema, SchemaGenerator, generate::SchemaSettings};
use serde_json::{Value, json};
use std::sync::OnceLock;

pub(super) const CHECKED_IN_PATH: &str = "website/public/schemas/tako-toml.json";
pub(super) const UPDATE_ENV: &str = "TAKO_UPDATE_SCHEMA";

const NAME_PATTERN: &str = "^[a-z]([a-z0-9-]{0,61}[a-z0-9])?$";
const NON_BLANK_PATTERN: &str = "\\S";

/// Generate the `tako.toml` JSON Schema.
pub(super) fn generate() -> Value {
    let mut generator = SchemaSettings::draft2020_12()
        .for_deserialize()
        .into_generator();
    let compose = reference::<tako_core::compose::ComposeConfig>(&mut generator);
    let container_options = reference::<tako_core::container::ContainerOptions>(&mut generator);
    let build = reference::<BuildConfig>(&mut generator);
    let build_stage = reference::<BuildStage>(&mut generator);
    let images = reference::<tako_images::ImagesConfig>(&mut generator);
    let env = reference::<EnvConfig>(&mut generator);
    let storage = reference::<StorageResourceConfig>(&mut generator);
    let worker = reference::<WorkflowWorkerConfig>(&mut generator);
    let mut definitions = generator.take_definitions(true);

    let env_definition = definitions
        .get_mut("EnvConfig")
        .and_then(Value::as_object_mut)
        .expect("EnvConfig definition");
    env_definition.insert(
        "description".into(),
        "Deploy environment from `[envs.<name>]`. Use either `route` or `routes`, not both. Put environment variables in `[vars.<name>]`.".into(),
    );
    env_definition.insert("not".into(), json!({ "required": ["route", "routes"] }));
    definitions.insert(
        "WorkflowWorkerConfig".into(),
        merge(
            definitions["WorkflowWorkerConfig"].clone(),
            json!({ "description": "Workflow worker group from `[workflows.<group>]` or `[servers.<server>.workflows.<group>]`." }),
        ),
    );

    let worker_count = json!({ "type": "integer", "minimum": 0, "maximum": u32::MAX });
    let base_workflow_properties = json!({
        "workers": merge(worker_count.clone(), json!({ "description": "Number of always-on worker processes. `0` means scale-to-zero. Parsed, but does not yet control production supervision." })),
        "concurrency": merge(worker_count, json!({ "description": "Max parallel task slots per worker. Parsed, but does not yet control production supervision." })),
    });
    let mut app_workflow_properties = base_workflow_properties.clone();
    app_workflow_properties["retention"] = json!({
        "type": "string",
        "pattern": "^(forever|\\+?[0-9]+[smhd])$",
        "default": "184d",
        "description": "When finished workflow runs and their saved steps become eligible for cleanup: a positive whole number followed by `s`, `m`, `h`, or `d`, such as `\"30d\"`, or `\"forever\"`. Defaults to `\"184d\"`.",
        "examples": ["184d", "30d", "1h", "forever"],
    });
    definitions.insert(
        "Workflows".into(),
        json!({
            "type": "object",
            "description": "Workflow settings. Other keys are named worker groups, selected by `worker: \"name\"` in workflow definitions.",
            "properties": app_workflow_properties,
            "propertyNames": { "pattern": NAME_PATTERN },
            "additionalProperties": worker,
        }),
    );
    definitions.insert(
        "ServerWorkflows".into(),
        json!({
            "type": "object",
            "description": "Per-server workflow overrides. Other keys are named worker groups. `retention` belongs in the app's `[workflows]` section.",
            "properties": base_workflow_properties,
            "propertyNames": { "pattern": NAME_PATTERN },
            "additionalProperties": worker,
        }),
    );
    definitions.insert(
        "VarValue".into(),
        json!({
            "type": ["string", "number", "boolean"],
            "description": "Converted to a process-env string. TOML datetimes are also accepted.",
        }),
    );

    let string_array = |description: &str| json!({ "type": "array", "items": { "type": "string" }, "description": description });
    let properties = json!({
        "name": {
            "type": "string",
            "pattern": NAME_PATTERN,
            "description": "Stable app identity. Lowercase letters, numbers, and hyphens; starts with a lowercase letter, ends with a letter or number, at most 63 characters. If omitted, Tako derives the name from the config file's directory. Renaming creates a separate deployed app.",
        },
        "schemas": merge(string_array("External `tako-schema.json` files or HTTP(S) URLs. File paths are relative to `tako.toml`. `tako gen` compiles them into native types."), json!({ "default": [] })),
        "runtime": {
            "type": "string",
            "pattern": "^\\s*(bun|node|go)\\s*(@[\\s\\S]*)?$",
            "description": "Runtime adapter, optionally pinned with `@version`, such as `bun@1.2.3`, `node`, or `go`.",
            "examples": ["bun", "node", "go", "bun@1.2.3"],
        },
        "package_manager": {
            "type": "string",
            "description": "JS package manager override: `bun`, `npm`, `pnpm`, or `yarn`. Detected from `package.json` or lockfiles when omitted.",
            "examples": ["bun", "npm", "pnpm", "yarn"],
        },
        "preset": {
            "type": "string",
            "pattern": "^[^:/]*\\S[^:/]*$",
            "description": "Runtime-local preset alias such as `vite`, `tanstack-start`, or `nextjs`. Provides `main` and `assets` defaults.",
            "examples": ["vite", "tanstack-start", "nextjs"],
        },
        "main": {
            "type": "string",
            "pattern": NON_BLANK_PATTERN,
            "description": "Runtime entrypoint override. May be a file path or module specifier.",
        },
        "app_root": {
            "type": "string",
            "description": "JS channels, presence, stores, and workflows root, relative to `tako.toml`. Defaults to `src`; use `.` for root-level files.",
            "default": DEFAULT_JS_APP_ROOT,
        },
        "dev": string_array("Custom `tako dev` command, such as `[\"vite\", \"dev\"]`. Overrides preset and runtime defaults unless a CLI command is passed."),
        "start": {
            "type": "array",
            "items": { "type": "string", "pattern": NON_BLANK_PATTERN },
            "minItems": 1,
            "description": "Native deploy start command for prebuilt artifacts. An exact `{main}` argument is replaced with the resolved entrypoint.",
        },
        "assets": string_array("Additional asset directories merged into deployed `public/`, relative to the project root."),
        "container": {
            "type": "string",
            "description": "Container file path for container releases, relative to `tako.toml`. `container`, `image`, and `[compose]` are mutually exclusive.",
        },
        "image": {
            "type": "string",
            "description": "Prebuilt container image reference, such as `registry/name:tag`. `container`, `image`, and `[compose]` are mutually exclusive.",
        },
        "compose": merge(compose, json!({ "description": "Docker Compose release: one Compose project on one server, with one HTTP service exposed by Tako." })),
        "container_options": merge(container_options, json!({ "description": "Container release options. Require `container` or `image`." })),
        "release": {
            "type": "string",
            "description": "One-shot command run on the leader server before the rolling update, such as `bun run db:migrate`. Can be overridden per environment.",
        },
        "build": merge(build, json!({ "description": "Simple build. Cannot be combined with `[[build_stages]]` when `run`, `include`, or `exclude` is set." })),
        "build_stages": {
            "type": "array",
            "items": build_stage,
            "description": "Multi-stage build, run in order. Mutually exclusive with `[build].run`.",
        },
        "workflows": { "$ref": "#/$defs/Workflows" },
        "images": merge(images, json!({ "description": "Public image optimizer allowlists. Each array replaces its default and must be non-empty." })),
        "vars": {
            "type": "object",
            "description": "Environment variables for every environment. A nested table `[vars.<env>]` overrides them for one environment.",
            "additionalProperties": {
                "anyOf": [
                    { "$ref": "#/$defs/VarValue" },
                    {
                        "type": "object",
                        "description": "Variables for one environment, merged on top of `[vars]`.",
                        "additionalProperties": { "$ref": "#/$defs/VarValue" },
                    },
                ],
            },
        },
        "envs": {
            "type": "object",
            "description": "Environments. Deploy environments must define `route` or `routes`. `development` is reserved for `tako dev`.",
            "properties": {
                "development": merge(env.clone(), json!({ "description": "Local development with `tako dev`. Routes are optional and default to `<name>.test`." })),
            },
            "additionalProperties": {
                "allOf": [
                    env,
                    {
                        "anyOf": [{ "required": ["route"] }, { "required": ["routes"] }],
                    },
                ],
            },
        },
        "storages": {
            "type": "object",
            "description": "S3-compatible storage resources, bound to environments with `[envs.<env>].storages`. `local` is built in and cannot be declared.",
            "propertyNames": { "pattern": "^[a-z0-9_-]+$", "not": { "const": "local" } },
            "additionalProperties": storage,
        },
        "servers": {
            "type": "object",
            "description": "Per-server overrides from `[servers.<server>]`. Server names follow the app-name rules.",
            "propertyNames": { "pattern": NAME_PATTERN },
            "additionalProperties": {
                "type": "object",
                "properties": { "workflows": { "$ref": "#/$defs/ServerWorkflows" } },
                "additionalProperties": false,
            },
        },
    });
    let mut schema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": TAKO_TOML_SCHEMA_URL,
        "$comment": "Generated from the Tako CLI's tako.toml parser types. Do not edit by hand.",
        "title": "tako.toml",
        "description": "Tako app configuration. Reference: https://tako.sh/docs/tako-toml. This schema can be looser than the CLI's validation; `tako` remains the authority.",
        "type": "object",
        "properties": properties,
        "additionalProperties": false,
        "$defs": definitions,
    });
    clean(&mut schema);
    schema
}

/// Validate `content` against the generated schema, panicking with the errors.
/// `TakoToml::parse` calls this in tests for every config it accepts.
pub(super) fn assert_accepts(content: &str) {
    static VALIDATOR: OnceLock<jsonschema::Validator> = OnceLock::new();
    let validator = VALIDATOR.get_or_init(|| {
        jsonschema::draft202012::new(&generate()).expect("tako.toml JSON Schema compiles")
    });
    let raw: toml::Value = toml::from_str(content).expect("accepted config is TOML");
    let instance = toml_to_json(raw);
    let errors: Vec<String> = validator
        .iter_errors(&instance)
        .map(|error| format!("{}: {error}", error.instance_path()))
        .collect();
    assert!(
        errors.is_empty(),
        "the tako.toml JSON Schema rejects a config the parser accepts:\n{}\n--- config ---\n{content}",
        errors.join("\n")
    );
}

/// Convert TOML to the JSON data model editors validate: datetimes become strings.
pub(super) fn toml_to_json(value: toml::Value) -> Value {
    match value {
        toml::Value::String(value) => Value::String(value),
        toml::Value::Integer(value) => Value::from(value),
        toml::Value::Float(value) => Value::from(value),
        toml::Value::Boolean(value) => Value::Bool(value),
        toml::Value::Datetime(value) => Value::String(value.to_string()),
        toml::Value::Array(values) => Value::Array(values.into_iter().map(toml_to_json).collect()),
        toml::Value::Table(table) => Value::Object(
            table
                .into_iter()
                .map(|(key, value)| (key, toml_to_json(value)))
                .collect(),
        ),
    }
}

fn reference<T: JsonSchema>(generator: &mut SchemaGenerator) -> Value {
    generator.subschema_for::<T>().to_value()
}

fn merge(mut base: Value, extra: Value) -> Value {
    if let (Some(base), Value::Object(extra)) = (base.as_object_mut(), extra) {
        base.extend(extra);
    }
    base
}

/// TOML has no null, so drop the null alternatives schemars adds for `Option`.
fn clean(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for child in object.values_mut() {
                clean(child);
            }
            if object.get("default").is_some_and(Value::is_null) {
                object.remove("default");
            }
            if let Some(Value::Array(types)) = object.get_mut("type") {
                types.retain(|kind| kind != "null");
                if types.len() == 1 {
                    let kind = types.remove(0);
                    object.insert("type".into(), kind);
                }
            }
            if let Some(Value::Array(variants)) = object.get_mut("anyOf") {
                variants.retain(|variant| variant != &json!({ "type": "null" }));
                if variants.len() == 1
                    && let Value::Object(variant) = variants.remove(0)
                {
                    object.remove("anyOf");
                    for (key, child) in variant {
                        object.entry(key).or_insert(child);
                    }
                }
            }
        }
        Value::Array(values) => values.iter_mut().for_each(clean),
        _ => {}
    }
}
