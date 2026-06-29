use super::*;

#[test]
fn resolve_dev_preset_ref_uses_build_adapter_override_when_preset_is_missing() {
    let temp = TempDir::new().unwrap();
    std::fs::write(temp.path().join("package.json"), r#"{"name":"demo"}"#).unwrap();
    let cfg = TakoToml {
        runtime: Some("node".to_string()),
        ..Default::default()
    };

    assert_eq!(resolve_dev_preset_ref(temp.path(), &cfg).unwrap(), "node");
}

#[test]
fn resolve_dev_preset_ref_qualifies_runtime_local_alias() {
    let temp = TempDir::new().unwrap();
    let cfg = TakoToml {
        runtime: Some("bun".to_string()),
        preset: Some("tanstack-start".to_string()),
        ..Default::default()
    };

    assert_eq!(
        resolve_dev_preset_ref(temp.path(), &cfg).unwrap(),
        "javascript/tanstack-start"
    );
}

#[test]
fn resolve_dev_preset_ref_errors_when_runtime_is_unknown_for_local_alias() {
    let temp = TempDir::new().unwrap();
    let cfg = TakoToml {
        preset: Some("tanstack-start".to_string()),
        ..Default::default()
    };

    let err = resolve_dev_preset_ref(temp.path(), &cfg).unwrap_err();
    assert!(err.contains("Cannot resolve preset"));
}

#[test]
fn resolve_dev_preset_ref_rejects_unknown_build_adapter_override() {
    let temp = TempDir::new().unwrap();
    let cfg = TakoToml {
        runtime: Some("python".to_string()),
        ..Default::default()
    };

    let err = resolve_dev_preset_ref(temp.path(), &cfg).unwrap_err();
    assert!(err.contains("Invalid runtime"));
}

#[test]
fn resolve_effective_dev_build_adapter_uses_preset_group_when_detection_is_unknown() {
    let temp = TempDir::new().unwrap();
    let cfg = TakoToml::default();

    let adapter = resolve_effective_dev_build_adapter(temp.path(), &cfg, "bun").unwrap();
    assert_eq!(adapter, BuildAdapter::Bun);
}

#[test]
fn resolve_dev_run_command_uses_sdk_entrypoint_for_bun() {
    let preset = parse_and_validate_preset(
        r#"
main = "src/index.ts"
"#,
        "bun",
    )
    .unwrap();

    let pd = Path::new("/project");
    let cmd = resolve_dev_run_command(
        &TakoToml::default(),
        &preset,
        "src/index.ts",
        BuildAdapter::Bun,
        false,
        pd,
        None,
    )
    .expect("runtime default dev command");

    assert_eq!(cmd[0], "bun");
    assert!(cmd.iter().any(|a| a.contains("entrypoints/bun-server.mjs")));
    assert!(cmd.last().unwrap().ends_with("src/index.ts"));
}

#[test]
fn resolve_dev_run_command_uses_sdk_entrypoint_for_node() {
    let preset = parse_and_validate_preset(
        r#"
main = "dist/server/tako-entry.mjs"
"#,
        "tanstack-start",
    )
    .unwrap();

    let pd = Path::new("/project");
    let cmd = resolve_dev_run_command(
        &TakoToml::default(),
        &preset,
        "src/index.ts",
        BuildAdapter::Node,
        true,
        pd,
        None,
    )
    .expect("runtime default dev command");

    assert_eq!(cmd[0], "node");
    assert!(
        cmd.iter()
            .any(|a| a.contains("entrypoints/node-server.mjs"))
    );
    assert!(cmd.last().unwrap().ends_with("src/index.ts"));
}

#[test]
fn resolve_dev_run_command_preset_dev_overrides_runtime_default() {
    let mut preset = parse_and_validate_preset(
        r#"
main = "src/index.ts"
"#,
        "vite",
    )
    .unwrap();
    preset.dev = vec!["vite".to_string(), "dev".to_string()];

    let pd = Path::new("/project");
    let cmd = resolve_dev_run_command(
        &TakoToml::default(),
        &preset,
        "src/index.ts",
        BuildAdapter::Bun,
        true,
        pd,
        None,
    )
    .expect("preset dev command");

    assert_eq!(cmd, vec!["vite", "dev"]);
}

#[test]
fn tanstack_start_bun_dev_resolves_to_bunx_bun_vite_dev_end_to_end() {
    let _lock = crate::paths::test_tako_home_env_lock();
    let previous = std::env::var_os("TAKO_HOME");
    let home = TempDir::new().unwrap();
    unsafe {
        std::env::set_var("TAKO_HOME", home.path());
    }

    let project = TempDir::new().unwrap();
    std::fs::write(project.path().join("bun.lock"), "").unwrap();
    std::fs::write(project.path().join("package.json"), r#"{"name":"demo"}"#).unwrap();

    let cfg = TakoToml {
        runtime: Some("bun".to_string()),
        preset: Some("tanstack-start".to_string()),
        ..Default::default()
    };

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let preset_ref = resolve_dev_preset_ref(project.path(), &cfg).unwrap();
    let (preset, _src) = runtime
        .block_on(crate::build::load_dev_build_preset(
            project.path(),
            &preset_ref,
        ))
        .unwrap();

    let adapter = resolve_effective_dev_build_adapter(project.path(), &cfg, &preset_ref).unwrap();

    let cmd = resolve_dev_run_command(
        &cfg,
        &preset,
        "src/index.ts",
        adapter,
        true,
        project.path(),
        None,
    )
    .unwrap();

    match previous {
        Some(value) => unsafe { std::env::set_var("TAKO_HOME", value) },
        None => unsafe { std::env::remove_var("TAKO_HOME") },
    }

    assert_eq!(adapter, BuildAdapter::Bun);
    assert_eq!(preset_ref, "javascript/tanstack-start");
    assert_eq!(cmd, vec!["bun", "--bun", "./node_modules/.bin/vite", "dev"]);
}

#[test]
fn resolve_dev_run_command_uses_preset_runtime_override_for_bun() {
    let preset = parse_and_validate_preset(
        r#"
main = "src/index.ts"
dev = ["vite", "dev"]

[bun]
dev = ["bunx", "--bun", "vite", "dev"]
"#,
        "tanstack-start",
    )
    .unwrap();

    let pd = Path::new("/project");
    let cmd = resolve_dev_run_command(
        &TakoToml::default(),
        &preset,
        "src/index.ts",
        BuildAdapter::Bun,
        true,
        pd,
        None,
    )
    .expect("preset runtime override command");

    assert_eq!(cmd, vec!["bunx", "--bun", "vite", "dev"]);
}

#[test]
fn resolve_dev_run_command_falls_back_to_preset_dev_when_runtime_override_missing() {
    let preset = parse_and_validate_preset(
        r#"
main = "src/index.ts"
dev = ["vite", "dev"]

[bun]
dev = ["bunx", "--bun", "vite", "dev"]
"#,
        "tanstack-start",
    )
    .unwrap();

    let pd = Path::new("/project");
    let cmd = resolve_dev_run_command(
        &TakoToml::default(),
        &preset,
        "src/index.ts",
        BuildAdapter::Node,
        true,
        pd,
        None,
    )
    .expect("preset default dev command for node");

    assert_eq!(cmd, vec!["vite", "dev"]);
}

#[test]
fn resolve_dev_run_command_config_dev_beats_runtime_override() {
    let preset = parse_and_validate_preset(
        r#"
main = "src/index.ts"
dev = ["vite", "dev"]

[bun]
dev = ["bunx", "--bun", "vite", "dev"]
"#,
        "tanstack-start",
    )
    .unwrap();

    let cfg = TakoToml {
        dev: vec!["custom".to_string(), "cmd".to_string()],
        ..Default::default()
    };

    let pd = Path::new("/project");
    let cmd = resolve_dev_run_command(
        &cfg,
        &preset,
        "src/index.ts",
        BuildAdapter::Bun,
        true,
        pd,
        None,
    )
    .expect("config dev command");

    assert_eq!(cmd, vec!["custom", "cmd"]);
}

#[test]
fn resolve_dev_run_command_config_dev_overrides_preset() {
    let mut preset = parse_and_validate_preset(
        r#"
main = "src/index.ts"
"#,
        "vite",
    )
    .unwrap();
    preset.dev = vec!["vite".to_string(), "dev".to_string()];

    let cfg = TakoToml {
        dev: vec!["custom".to_string(), "cmd".to_string()],
        ..Default::default()
    };

    let pd = Path::new("/project");
    let cmd = resolve_dev_run_command(
        &cfg,
        &preset,
        "src/index.ts",
        BuildAdapter::Bun,
        true,
        pd,
        None,
    )
    .expect("config dev command");

    assert_eq!(cmd, vec!["custom", "cmd"]);
}

#[test]
fn resolve_dev_run_command_cli_command_beats_config_and_preset() {
    let preset = parse_and_validate_preset(
        r#"
main = "src/index.ts"
dev = ["vite", "dev"]

[bun]
dev = ["bunx", "--bun", "vite", "dev"]
"#,
        "tanstack-start",
    )
    .unwrap();

    let cfg = TakoToml {
        dev: vec!["custom".to_string(), "cmd".to_string()],
        ..Default::default()
    };
    let override_cmd = vec![
        "npm".to_string(),
        "run".to_string(),
        "dev".to_string(),
        "--".to_string(),
        "--host".to_string(),
        "127.0.0.1".to_string(),
    ];

    let pd = Path::new("/project");
    let cmd = resolve_dev_run_command(
        &cfg,
        &preset,
        "src/index.ts",
        BuildAdapter::Bun,
        true,
        pd,
        Some(&override_cmd),
    )
    .expect("cli dev command");

    assert_eq!(cmd, vec!["npm", "run", "dev", "--", "--host", "127.0.0.1"]);
}
