use super::*;
use std::collections::HashMap;

fn runtime() -> (tempfile::TempDir, ComposeRuntime) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("compose.yaml"), "services: {}\n").unwrap();
    let config = tako_core::compose::ComposeConfig {
        file: "compose.yaml".into(),
        service: "web".into(),
        port: 3000,
        health_path: "/health".into(),
        health_host: None,
    };
    let runtime = ComposeRuntime::new(
        dir.path().into(),
        config,
        HashMap::from([(
            crate::TAKO_APP_DATA_DIR_ENV.into(),
            dir.path().join("data").display().to_string(),
        )]),
    )
    .unwrap();
    (dir, runtime)
}

fn model() -> Value {
    json!({
        "services": {
            "web": {"image":"sha256:web", "build":{"context":"/release/one"}, "restart":"unless-stopped",
                "environment":{"REDIS_URL":"redis://redis:6379"}, "networks":{"default":null},
                "depends_on":{"redis":{"condition":"service_healthy"}}},
            "redis": {"image":"sha256:redis", "restart":"unless-stopped", "networks":{"default":null},
                "volumes":[{"type":"volume","source":"data","target":"/data"}],
                "healthcheck":{"test":["CMD","redis-cli","ping"]}}
        },
        "networks":{"default":{"name":"stable_default"}},
        "volumes":{"data":{"name":"stable_data"}}
    })
}

fn fingerprint(plan: &Plan, name: &str) -> String {
    plan.services
        .iter()
        .find(|s| s.name == name)
        .unwrap()
        .fingerprint
        .clone()
}

#[test]
fn environment_resource_rotation_replaces_only_its_consumers() {
    let (_dir, mut runtime) = runtime();
    runtime.env.insert("APP_TOKEN".into(), "first-value".into());
    for kind in ["secrets", "configs"] {
        let mut input = model();
        input[kind] = json!({"token": {"environment": "APP_TOKEN"}});
        input["services"]["web"][kind] = json!([{ "source": "token" }]);
        let before = Plan::new(input.clone(), &runtime).unwrap();
        runtime
            .env
            .insert("APP_TOKEN".into(), "rotated-value".into());
        let after = Plan::new(input, &runtime).unwrap();
        assert_ne!(fingerprint(&before, "web"), fingerprint(&after, "web"));
        assert_eq!(fingerprint(&before, "redis"), fingerprint(&after, "redis"));
        assert!(
            !serde_json::to_string(&after.model)
                .unwrap()
                .contains("rotated-value")
        );
        runtime.env.insert("APP_TOKEN".into(), "first-value".into());
    }
}

#[test]
fn unchanged_dependency_is_independent_of_web_image_build_and_release() {
    let (_dir, runtime) = runtime();
    let before = Plan::new(model(), &runtime).unwrap();
    let mut next = model();
    next["services"]["web"]["image"] = json!("sha256:next");
    next["services"]["web"]["build"]["context"] = json!("/release/two");
    let after = Plan::new(next, &runtime).unwrap();
    assert_eq!(fingerprint(&before, "redis"), fingerprint(&after, "redis"));
    assert_ne!(fingerprint(&before, "web"), fingerprint(&after, "web"));
    let mut next = model();
    next["services"]["web"]["build"]["context"] = json!("/release/two");
    assert_eq!(
        fingerprint(&before, "web"),
        fingerprint(&Plan::new(next, &runtime).unwrap(), "web")
    );
}

#[test]
fn only_consumers_of_changed_secrets_and_resources_get_new_identities() {
    let (_dir, runtime) = runtime();
    let before = Plan::new(model(), &runtime).unwrap();
    let mut next = model();
    next["services"]["web"]["environment"]["TOKEN"] = json!("private-token");
    let after = Plan::new(next, &runtime).unwrap();
    assert_ne!(fingerprint(&before, "web"), fingerprint(&after, "web"));
    assert_eq!(fingerprint(&before, "redis"), fingerprint(&after, "redis"));
    let mut next = model();
    next["volumes"]["data"]["name"] = json!("another_data");
    let after = Plan::new(next, &runtime).unwrap();
    assert_eq!(fingerprint(&before, "web"), fingerprint(&after, "web"));
    assert_ne!(fingerprint(&before, "redis"), fingerprint(&after, "redis"));
}

#[test]
fn defaults_protect_data_and_workers_but_allow_explicit_overlap() {
    let (_dir, runtime) = runtime();
    let mut input = model();
    input["services"]["worker"] = json!({"image":"sha256:worker"});
    let plan = Plan::new(input.clone(), &runtime).unwrap();
    let orders: BTreeMap<_, _> = plan
        .services
        .iter()
        .map(|s| (s.name.as_str(), s.order))
        .collect();
    assert_eq!(orders["web"], UpdateOrder::StartFirst);
    assert_eq!(orders["redis"], UpdateOrder::StopFirst);
    assert_eq!(orders["worker"], UpdateOrder::StopFirst);
    input["services"]["web"]["volumes"] =
        json!([{"type":"volume","source":"data","target":"/data"}]);
    input["services"]["worker"]["deploy"] = json!({"update_config":{"order":"start-first"}});
    let plan = Plan::new(input, &runtime).unwrap();
    assert_eq!(
        plan.services
            .iter()
            .find(|s| s.name == "web")
            .unwrap()
            .order,
        UpdateOrder::StopFirst
    );
    assert_eq!(
        plan.services
            .iter()
            .find(|s| s.name == "worker")
            .unwrap()
            .order,
        UpdateOrder::StartFirst
    );
}

#[test]
fn policy_and_replica_changes_preserve_existing_container_identity() {
    let (_dir, runtime) = runtime();
    let before = Plan::new(model(), &runtime).unwrap();
    let mut next = model();
    next["services"]["redis"]["deploy"] =
        json!({"replicas":3,"update_config":{"order":"start-first","monitor":"10s"}});
    let after = Plan::new(next, &runtime).unwrap();
    assert_eq!(fingerprint(&before, "redis"), fingerprint(&after, "redis"));
    assert_eq!(
        after
            .services
            .iter()
            .find(|s| s.name == "redis")
            .unwrap()
            .replicas,
        3
    );
}

#[test]
fn dependencies_are_ordered_and_completed_jobs_are_recognized() {
    let (_dir, runtime) = runtime();
    let mut input = model();
    input["services"]["web"]["depends_on"]["migrate"] =
        json!({"condition":"service_completed_successfully"});
    input["services"]["migrate"] =
        json!({"image":"sha256:job","depends_on":{"redis":{"condition":"service_healthy"}}});
    let plan = Plan::new(input, &runtime).unwrap();
    assert_eq!(
        plan.services
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>(),
        ["redis", "migrate", "web"]
    );
    assert!(plan.services[1].completed);
}

#[test]
fn invalid_graph_and_unsupported_semantics_fail_before_container_changes() {
    let (_dir, runtime) = runtime();
    let mut input = model();
    input["services"]["redis"]["depends_on"] = json!({"web":{"condition":"service_started"}});
    assert!(Plan::new(input, &runtime).err().unwrap().contains("cycle"));
    let mut input = model();
    input["services"]["redis"]["scale"] = json!(0);
    assert!(
        Plan::new(input, &runtime)
            .err()
            .unwrap()
            .contains("zero replicas")
    );
    let mut input = model();
    input["services"]["web"]["deploy"] = json!({"update_config":{"parallelism":2}});
    assert!(
        Plan::new(input, &runtime)
            .err()
            .unwrap()
            .contains("order and monitor")
    );
    let mut input = model();
    input["services"]["redis"]["volumes"][0]
        .as_object_mut()
        .unwrap()
        .remove("source");
    assert!(
        Plan::new(input, &runtime)
            .err()
            .unwrap()
            .contains("anonymous volumes")
    );
}

#[test]
fn generation_model_preserves_restart_and_aliases_without_dependencies_or_builds() {
    let (_dir, runtime) = runtime();
    let plan = Plan::new(model(), &runtime).unwrap();
    let web = plan.services.iter().find(|s| s.name == "web").unwrap();
    let generated = plan.container_model(web, 0, &runtime);
    let service = &generated["services"][web.generation(0)];
    assert_eq!(generated["services"].as_object().unwrap().len(), 1);
    assert_eq!(service["restart"], "unless-stopped");
    assert_eq!(service["networks"]["default"]["aliases"], json!(["web"]));
    assert!(service.get("depends_on").is_none());
    assert!(service.get("build").is_none());
    assert_eq!(service["ports"][0]["published"], "0");
    assert_eq!(generated["volumes"]["data"]["name"], "stable_data");
}

#[test]
fn mounted_file_content_changes_are_part_of_identity() {
    let (dir, runtime) = runtime();
    let file = dir.path().join("config.txt");
    std::fs::write(&file, "first").unwrap();
    let mut input = model();
    input["services"]["web"]["configs"] = json!([{"source":"settings","target":"/config"}]);
    input["configs"] = json!({"settings":{"file":file}});
    let before = Plan::new(input.clone(), &runtime).unwrap();
    std::fs::write(&file, "second").unwrap();
    let after = Plan::new(input, &runtime).unwrap();
    assert_ne!(fingerprint(&before, "web"), fingerprint(&after, "web"));
    assert_eq!(fingerprint(&before, "redis"), fingerprint(&after, "redis"));
}

#[test]
fn mounted_executable_mode_changes_replace_only_the_consumer() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, runtime) = runtime();
    let scripts = dir.path().join("scripts");
    std::fs::create_dir(&scripts).unwrap();
    let script = scripts.join("maintenance.sh");
    std::fs::write(&script, "#!/bin/sh\necho maintenance\n").unwrap();
    let mut input = model();
    input["services"]["web"]["volumes"] = json!([
        {"type":"bind", "source":scripts, "target":"/scripts", "read_only":true}
    ]);
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o644)).unwrap();
    let before = Plan::new(input.clone(), &runtime).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let after = Plan::new(input.clone(), &runtime).unwrap();
    assert_ne!(fingerprint(&before, "web"), fingerprint(&after, "web"));
    assert_eq!(fingerprint(&before, "redis"), fingerprint(&after, "redis"));
    // Capture normalizes any executable source file to 0755.
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let normalized = Plan::new(input, &runtime).unwrap();
    assert_eq!(fingerprint(&after, "web"), fingerprint(&normalized, "web"));
}

#[test]
fn identical_mounted_inputs_keep_identity_across_release_directories() {
    let (_dir, runtime) = runtime();
    let mut fingerprints = Vec::new();
    for version in ["v1", "v2", "v3"] {
        let root = runtime.directory.join(version);
        std::fs::create_dir(&root).unwrap();
        let file = root.join("settings.txt");
        std::fs::write(&file, if version == "v3" { "changed" } else { "same" }).unwrap();
        let mut release = runtime.clone();
        release.directory = root;
        let mut input = model();
        input["services"]["redis"]["volumes"] = json!([
            {"type":"bind", "source":file, "target":"/settings", "read_only":true}
        ]);
        input["services"]["redis"]["configs"] = json!([{"source":"settings"}]);
        input["services"]["redis"]["secrets"] = json!([{"source":"token"}]);
        input["configs"] = json!({"settings":{"file":file}});
        input["secrets"] = json!({"token":{"file":file}});
        let plan = Plan::new(input, &release).unwrap();
        assert_eq!(plan.model["configs"]["settings"]["file"], json!(file));
        assert_eq!(
            plan.model["services"]["redis"]["volumes"][0]["source"],
            json!(file)
        );
        fingerprints.push(fingerprint(&plan, "redis"));
    }
    assert_eq!(fingerprints[0], fingerprints[1]);
    assert_ne!(fingerprints[1], fingerprints[2]);
}

#[test]
fn monitor_duration_accepts_compound_values_and_rejects_unbounded_waits() {
    assert_eq!(
        parse_duration(&json!("1m2.5s")).unwrap(),
        Duration::from_millis(62500)
    );
    assert_eq!(parse_duration(&json!("0s")).unwrap(), Duration::ZERO);
    assert!(parse_duration(&json!("1h")).is_err());
    assert!(parse_duration(&json!("NaNs")).is_err());
}
