use super::*;
use std::collections::HashMap;

fn runtime() -> (tempfile::TempDir, ComposeRuntime) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("compose.yaml"), "services: {}\n").unwrap();
    let config = tako_core::compose::ComposeConfig {
        file: "compose.yaml".into(),
        services: serde_json::from_value(
            json!({"web":{"envs":{"production":{"route":"web.localhost"}}}}),
        )
        .unwrap(),
    };
    let mut runtime = ComposeRuntime::new(
        dir.path().into(),
        config,
        HashMap::from([(
            crate::TAKO_APP_DATA_DIR_ENV.into(),
            dir.path().join("data").display().to_string(),
        )]),
    )
    .unwrap();
    runtime.ingress_ports.insert("web".into(), 3000);
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

#[test]
fn stopped_apps_preserve_scale_and_container_identity() {
    let (_dir, runtime) = runtime();
    let runtime = runtime.with_replicas(BTreeMap::from([("web".into(), 3)]));
    let active = Plan::new(model(), &runtime).unwrap();
    let paused = runtime
        .clone()
        .with_stopped(false, BTreeSet::from(["web".into()]));
    let plan = Plan::new(model(), &paused).unwrap();
    let web = plan.services.iter().find(|s| s.name == "web").unwrap();
    assert_eq!((web.replicas, web.configured_replicas), (0, 3));
    assert_eq!(fingerprint(&plan, "web"), fingerprint(&active, "web"));
    assert_eq!(
        plan.services
            .iter()
            .find(|s| s.name == "redis")
            .unwrap()
            .replicas,
        1
    );
    let whole = Plan::new(model(), &runtime.with_stopped(true, BTreeSet::new())).unwrap();
    assert!(whole.services.iter().all(|s| s.replicas == 0));
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
fn dependency_conditions_are_part_of_the_immutable_generation() {
    let (_dir, runtime) = runtime();
    let before = Plan::new(model(), &runtime).unwrap();
    let mut changed = model();
    changed["services"]["web"]["depends_on"]["redis"]["condition"] = json!("service_started");
    let after = Plan::new(changed, &runtime).unwrap();
    assert_ne!(fingerprint(&before, "web"), fingerprint(&after, "web"));
    assert_eq!(fingerprint(&before, "redis"), fingerprint(&after, "redis"));
}

#[test]
fn generation_model_preserves_restart_and_aliases_without_dependencies_or_builds() {
    let (_dir, runtime) = runtime();
    let plan = Plan::new(model(), &runtime).unwrap();
    let web = plan.services.iter().find(|s| s.name == "web").unwrap();
    let generated = plan.container_model(web, 0, &runtime).unwrap();
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

#[test]
fn replica_overrides_win_without_changing_container_identity() {
    let (_dir, runtime) = runtime();
    let mut model = model();
    model["services"]["web"]["deploy"] = json!({"replicas": 2});
    model["services"]["worker"] = json!({"image":"sha256:worker", "deploy":{"replicas":3}});
    let defaults = Plan::new(model.clone(), &runtime).unwrap();
    let overridden = runtime
        .clone()
        .with_replicas(BTreeMap::from([("web".into(), 5), ("worker".into(), 0)]));
    let plan = Plan::new(model.clone(), &overridden).unwrap();
    for service in &plan.services {
        assert_eq!(service.fingerprint, fingerprint(&defaults, &service.name));
        assert_eq!(
            service.replicas,
            match service.name.as_str() {
                "web" => 5,
                "worker" => 0,
                _ => 1,
            }
        );
    }
    let reset = Plan::new(model.clone(), &runtime).unwrap();
    assert_eq!(
        reset
            .services
            .iter()
            .find(|s| s.name == "web")
            .unwrap()
            .replicas,
        2
    );
    for (service, count) in [("web", 0), ("web", 129), ("worker", 129), ("redis", 0)] {
        let runtime = runtime
            .clone()
            .with_replicas(BTreeMap::from([(service.into(), count)]));
        assert!(
            Plan::new(model.clone(), &runtime).is_err(),
            "{service}={count}"
        );
    }
}

#[test]
fn native_gateway_changes_generations_and_preserves_stack_dns_aliases() {
    let (_dir, runtime) = runtime();
    let ordinary = Plan::new(model(), &runtime).unwrap();
    let network = crate::NativeNetwork::new("00112233445566778899aabbccddeeff").unwrap();
    let runtime = runtime.with_native_network(network.clone());
    let native = Plan::new(model(), &runtime).unwrap();
    for service in &native.services {
        assert_ne!(fingerprint(&ordinary, &service.name), service.fingerprint);
        let container = native.container_model(service, 0, &runtime).unwrap();
        let definition = &container["services"][service.generation(0)];
        assert_eq!(definition["dns"], json!([network.gateway().to_string()]));
        assert_eq!(
            definition["networks"]["default"]["aliases"],
            json!([service.name])
        );
        assert!(
            definition["networks"]["tako_native_services"]
                .get("aliases")
                .is_none()
        );
        assert_eq!(
            container["networks"]["tako_native_services"],
            json!({"external":true,"name":network.name()})
        );
    }
    let restarted = Plan::new(model(), &runtime).unwrap();
    assert_eq!(fingerprint(&native, "web"), fingerprint(&restarted, "web"));
}

fn sdk_runtime(dir: &std::path::Path, build: &str, secret: &str) -> crate::SdkRuntime {
    crate::SdkRuntime {
        app_name: "shop/production".into(),
        token: "group-token".into(),
        envelope: json!({"token": "group-token", "secrets": {"API_KEY": secret}, "storages": {}, "databases": {}})
            .to_string(),
        build: build.into(),
        environment: "production".into(),
        socket_dir: dir.join("socket"),
        data_dir: dir.join("data"),
        health_host: "shop.tako".into(),
    }
}

fn sdk_group(dir: &std::path::Path) -> ComposeRuntime {
    let config = tako_core::compose::ComposeConfig {
        file: "compose.yaml".into(),
        services: serde_json::from_value(json!({
            "web": {"sdk": true, "envs": {"production": {"route": "web.localhost"}}},
            "worker": {"sdk": true},
            "nginx": {"envs": {"production": {"route": "nginx.localhost"}}}
        }))
        .unwrap(),
    };
    let mut runtime = ComposeRuntime::new(
        dir.into(),
        config,
        HashMap::from([(
            crate::TAKO_APP_DATA_DIR_ENV.into(),
            dir.join("data").display().to_string(),
        )]),
    )
    .unwrap();
    runtime.ingress_ports.insert("web".into(), 3000);
    runtime.ingress_ports.insert("nginx".into(), 80);
    runtime
}

fn sdk_model() -> Value {
    json!({
        "services": {
            "web": {"image":"sha256:web", "environment":{"APP":"web"}},
            "worker": {"image":"sha256:web", "command":["bun","worker.mjs"]},
            "nginx": {"image":"sha256:nginx"},
            "redis": {"image":"sha256:redis"}
        }
    })
}

#[test]
fn sdk_runtime_changes_only_sdk_fingerprints_with_build_and_envelope() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("compose.yaml"), "services: {}\n").unwrap();
    let group = sdk_group(dir.path());
    assert!(
        Plan::new(sdk_model(), &group)
            .err()
            .unwrap()
            .contains("Tako runtime")
    );
    let base = Plan::new(
        sdk_model(),
        &group
            .clone()
            .with_sdk(sdk_runtime(dir.path(), "v1", "first")),
    )
    .unwrap();
    for changed in [
        sdk_runtime(dir.path(), "v2", "first"),
        sdk_runtime(dir.path(), "v1", "rotated"),
    ] {
        let plan = Plan::new(sdk_model(), &group.clone().with_sdk(changed)).unwrap();
        for service in ["web", "worker"] {
            assert_ne!(fingerprint(&plan, service), fingerprint(&base, service));
        }
        for service in ["nginx", "redis"] {
            assert_eq!(fingerprint(&plan, service), fingerprint(&base, service));
        }
    }
    // Plain services keep the identity they had before the SDK runtime existed.
    let mut plain = group.clone();
    plain.config.services.retain(|name, _| name == "nginx");
    let without_sdk = Plan::new(sdk_model(), &plain).unwrap();
    assert_eq!(
        fingerprint(&without_sdk, "redis"),
        fingerprint(&base, "redis")
    );
}

#[test]
fn sdk_services_receive_the_runtime_and_plain_services_only_the_marker() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("compose.yaml"), "services: {}\n").unwrap();
    std::fs::create_dir(dir.path().join("data")).unwrap();
    let runtime = sdk_group(dir.path()).with_sdk(sdk_runtime(dir.path(), "v1", "first"));
    // The envelope reaches Docker on stdin, never the Compose client.
    assert!(
        runtime
            .env
            .values()
            .all(|value| !value.contains("group-token"))
    );
    let plan = Plan::new(sdk_model(), &runtime).unwrap();
    let definition = |name: &str| {
        let service = plan.services.iter().find(|s| s.name == name).unwrap();
        let model = plan.container_model(service, 0, &runtime).unwrap();
        model["services"][service.generation(0)].clone()
    };
    let web = definition("web");
    let environment = &web["environment"];
    assert_eq!(environment["APP"], "web");
    assert_eq!(environment["TAKO_APP_NAME"], "shop/production");
    assert_eq!(
        environment["TAKO_INTERNAL_SOCKET"],
        "/run/tako/internal.sock"
    );
    assert_eq!(environment["TAKO_DATA_DIR"], "/tako/data");
    assert_eq!(environment["TAKO_BUILD"], "v1");
    assert_eq!(environment["ENV"], "production");
    assert_eq!(environment["HOST"], "0.0.0.0");
    assert_eq!(environment["PORT"], "3000");
    assert_eq!(environment["TAKO_COMPOSE_SERVICE"], "web");
    let bootstrap: Value =
        serde_json::from_str(environment["TAKO_BOOTSTRAP_DATA"].as_str().unwrap()).unwrap();
    assert_eq!(bootstrap["token"], "group-token");
    let mounts = web["volumes"].as_array().unwrap();
    let mount = |target: &str| mounts.iter().find(|m| m["target"] == target).unwrap();
    assert_eq!(mount("/run/tako")["read_only"], true);
    assert_eq!(
        mount("/run/tako")["source"],
        json!(dir.path().join("socket"))
    );
    assert_eq!(
        mount("/tako/data")["source"],
        json!(dir.path().join("data"))
    );
    assert!(web["group_add"].as_array().is_some_and(|g| g.len() == 1));

    // Workers have no HTTP contract.
    let worker = definition("worker");
    assert_eq!(worker["environment"]["TAKO_APP_NAME"], "shop/production");
    assert!(worker["environment"].get("HOST").is_none());
    assert!(worker["environment"].get("PORT").is_none());

    let nginx = definition("nginx");
    assert_eq!(
        nginx["environment"],
        json!({"TAKO_COMPOSE_SERVICE": "nginx"})
    );
    assert!(nginx.get("volumes").is_none());
    let redis = definition("redis");
    assert!(redis.get("environment").is_none());
}
