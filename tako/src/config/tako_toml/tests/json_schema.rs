use super::super::json_schema::{CHECKED_IN_PATH, UPDATE_ENV, generate, toml_to_json};
use super::super::validation::TOP_LEVEL_KEYS;
use super::super::*;
use serde_json::Value;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn checked_in_schema() -> Value {
    let path = repo_root().join(CHECKED_IN_PATH);
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    serde_json::from_str(&content).expect("checked-in schema is JSON")
}

fn schema_errors(content: &str) -> Vec<String> {
    let validator = jsonschema::draft202012::new(&checked_in_schema()).expect("schema compiles");
    let instance = toml_to_json(toml::from_str(content).expect("TOML"));
    validator
        .iter_errors(&instance)
        .map(|error| format!("{}: {error}", error.instance_path()))
        .collect()
}

#[test]
fn tako_toml_json_schema_matches_checked_in_file() {
    let generated = generate();
    let path = repo_root().join(CHECKED_IN_PATH);
    if std::env::var_os(UPDATE_ENV).is_some() {
        let mut rendered = serde_json::to_string_pretty(&generated).unwrap();
        rendered.push('\n');
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, rendered).unwrap();
        return;
    }
    assert!(
        checked_in_schema() == generated,
        "{CHECKED_IN_PATH} is out of date. Regenerate it with:\n  {UPDATE_ENV}=1 cargo test -p tako-cli --bin tako tako_toml_json_schema"
    );
}

#[test]
fn tako_toml_json_schema_lists_every_top_level_key() {
    let generated = generate();
    let mut properties: Vec<&str> = generated["properties"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    let mut keys = TOP_LEVEL_KEYS.to_vec();
    properties.sort_unstable();
    keys.sort_unstable();
    assert_eq!(properties, keys);
    assert_eq!(generated["additionalProperties"], Value::Bool(false));
}

fn repo_tako_tomls(dir: &Path, found: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy();
        if path.is_dir() {
            if !matches!(name.as_ref(), "node_modules" | ".tako" | "dist" | "target") {
                repo_tako_tomls(&path, found);
            }
        } else if name == "tako.toml" {
            found.push(path);
        }
    }
}

#[test]
fn tako_toml_json_schema_accepts_repo_configs() {
    let root = repo_root();
    let mut configs = Vec::new();
    // `presets/` holds preset definitions, not app configs. The `e2e/cli`
    // suite writes its configs inline in the tests.
    for dir in ["examples", "e2e/fixtures"] {
        repo_tako_tomls(&root.join(dir), &mut configs);
    }
    assert!(configs.len() >= 10, "found only {configs:?}");
    for path in configs {
        let content = std::fs::read_to_string(&path).unwrap();
        TakoToml::parse(&content)
            .unwrap_or_else(|error| panic!("{} does not parse: {error}", path.display()));
        let errors = schema_errors(&content);
        assert!(errors.is_empty(), "{}: {errors:?}", path.display());
    }
}

#[test]
fn tako_toml_json_schema_accepts_reference_examples_the_parser_accepts() {
    let docs = std::fs::read_to_string(repo_root().join("website/docs/tako-toml.md")).unwrap();
    let mut blocks = docs.split("```toml\n").skip(1);
    let mut accepted = 0;
    for block in blocks.by_ref() {
        let block = block.split("```").next().unwrap();
        // Many reference snippets are fragments, such as an environment
        // without a route. Only the complete configs the parser accepts count.
        if TakoToml::parse(block).is_ok() {
            accepted += 1;
            let errors = schema_errors(block);
            assert!(errors.is_empty(), "{errors:?}\n{block}");
        }
    }
    assert!(accepted >= 2, "expected the minimal and complete examples");
}

#[test]
fn tako_toml_json_schema_rejects_common_mistakes_the_parser_rejects() {
    let route = "[envs.production]\nroute = \"app.example.com\"\n";
    for (mistake, content) in [
        ("unknown top-level key", format!("nmae = \"app\"\n{route}")),
        (
            "env vars inside an environment",
            "[envs.production]\nroute = \"app.example.com\"\nvars = { A = \"1\" }\n".to_string(),
        ),
        ("wrong type", format!("name = 1\n{route}")),
        ("invalid app name", format!("name = \"My App\"\n{route}")),
        ("unknown runtime", format!("runtime = \"python\"\n{route}")),
        (
            "both route and routes",
            "[envs.production]\nroute = \"a.example.com\"\nroutes = [\"b.example.com\"]\n"
                .to_string(),
        ),
        (
            "deploy environment without a route",
            "[envs.production]\nservers = [\"prod\"]\n".to_string(),
        ),
        ("zero idle timeout", format!("{route}idle_timeout = 0\n")),
        (
            "unknown ssl provider",
            format!("{route}ssl = \"zerossl\"\n"),
        ),
        (
            "unknown build key",
            format!("[build]\ncommand = \"bun run build\"\n{route}"),
        ),
        (
            "build stage without run",
            format!("[[build_stages]]\nname = \"web\"\n{route}"),
        ),
        ("array variable", format!("[vars]\nA = [1]\n{route}")),
        (
            "invalid retention",
            format!("[workflows]\nretention = \"6 months\"\n{route}"),
        ),
        (
            "retention on a server",
            format!("[servers.prod.workflows]\nretention = \"1d\"\n{route}"),
        ),
        (
            "unknown worker key",
            format!("[workflows.email]\nthreads = 2\n{route}"),
        ),
        ("non-table envs", "envs = \"production\"\n".to_string()),
    ] {
        assert!(
            TakoToml::parse(&content).is_err(),
            "parser should reject {mistake}"
        );
        assert!(
            !schema_errors(&content).is_empty(),
            "schema should reject {mistake}"
        );
    }
}
