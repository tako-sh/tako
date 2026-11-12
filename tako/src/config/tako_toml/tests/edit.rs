use super::super::*;

fn directive_count(content: &str) -> usize {
    content
        .lines()
        .filter(|line| line.starts_with("#:schema"))
        .count()
}

#[test]
fn edits_keep_the_schema_directive_once_at_the_top() {
    let temp = tempfile::TempDir::new().unwrap();
    let path = temp.path().join("tako.toml");
    std::fs::write(
        &path,
        format!(
            "{TAKO_TOML_SCHEMA_DIRECTIVE}\n# Tako configuration\nname = \"demo\"\n\n[envs.production]\nroute = \"demo.example.com\"\n"
        ),
    )
    .unwrap();

    TakoToml::upsert_server_env_in_file(&path, "prod", "production").unwrap();
    TakoToml::upsert_server_env_in_file(&path, "prod-b", "production").unwrap();
    TakoToml::upsert_storage_binding_in_file(&path, "production", "uploads", "local", None)
        .unwrap();

    let content = std::fs::read_to_string(&path).unwrap();
    assert_eq!(content.lines().next(), Some(TAKO_TOML_SCHEMA_DIRECTIVE));
    assert_eq!(directive_count(&content), 1, "{content}");
    let parsed = TakoToml::parse(&content).unwrap();
    assert_eq!(parsed.envs["production"].servers, ["prod", "prod-b"]);
}

#[test]
fn edits_add_the_schema_directive_only_to_new_files() {
    let temp = tempfile::TempDir::new().unwrap();
    let new_path = temp.path().join("new/tako.toml");
    TakoToml::upsert_server_env_in_file(&new_path, "prod", "production").unwrap();
    let content = std::fs::read_to_string(&new_path).unwrap();
    assert_eq!(content.lines().next(), Some(TAKO_TOML_SCHEMA_DIRECTIVE));

    let existing_path = temp.path().join("tako.toml");
    std::fs::write(&existing_path, "name = \"demo\"\n").unwrap();
    TakoToml::upsert_server_env_in_file(&existing_path, "prod", "production").unwrap();
    let content = std::fs::read_to_string(&existing_path).unwrap();
    assert_eq!(directive_count(&content), 0, "{content}");
}

#[test]
fn schema_directive_names_the_published_schema() {
    assert_eq!(
        TAKO_TOML_SCHEMA_DIRECTIVE,
        format!("#:schema {TAKO_TOML_SCHEMA_URL}")
    );
}
