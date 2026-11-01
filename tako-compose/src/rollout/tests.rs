use super::*;
use std::collections::HashMap;
use std::process::Stdio;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Fixture {
    dir: tempfile::TempDir,
    config: tako_core::compose::ComposeConfig,
    env: HashMap<String, String>,
    runtimes: Vec<ComposeRuntime>,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = tako_core::compose::ComposeConfig {
            file: "compose.yaml".into(),
            service: "web".into(),
            port: 8080,
            health_path: "/".into(),
            health_host: None,
        };
        let env = HashMap::from([
            (
                crate::TAKO_APP_DATA_DIR_ENV.into(),
                dir.path().join("data/app").display().to_string(),
            ),
            ("DB_PASSWORD".into(), "fixture-only".into()),
            ("LITERAL".into(), "literal$VALUE/${UNKNOWN}/$$".into()),
        ]);
        Self {
            dir,
            config,
            env,
            runtimes: Vec::new(),
        }
    }

    async fn release(&mut self, version: &str, yaml: &str) -> ComposeRuntime {
        let runtime = self.unprepared_release(version, yaml);
        runtime.prepare(version).await.unwrap();
        runtime
    }

    fn unprepared_release(&mut self, version: &str, yaml: &str) -> ComposeRuntime {
        let dir = self.dir.path().join(version);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("compose.yaml"), yaml).unwrap();
        std::fs::write(dir.join("Dockerfile"), format!("FROM docker.io/library/busybox:1.37\nRUN mkdir -p /www && echo {version} > /www/index.html\nCMD [\"sh\",\"-c\",\"echo web-ready; exec httpd -f -p 8080 -h /www\"]\n")).unwrap();
        let runtime = ComposeRuntime::new(dir, self.config.clone(), self.env.clone()).unwrap();
        self.runtimes.push(runtime.clone());
        runtime
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtimes.first()
            && let Ok((runtime, _)) = runtime.prepared()
        {
            let _ = runtime
                .command(false)
                .into_std()
                .args(["down", "--volumes", "--remove-orphans"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

fn service<'a>(containers: &'a [ServiceContainer], name: &str) -> &'a ServiceContainer {
    containers.iter().find(|c| c.service == name).unwrap()
}

async fn exec(container: &ServiceContainer, args: &[&str]) -> String {
    let mut command = docker_command(false);
    command.args(["exec", &container.id]).args(args);
    let child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let output = crate::process_output::capture(
        child,
        Some(Duration::from_secs(30)),
        crate::process_output::StdoutMode::Complete,
    )
    .await
    .unwrap();
    assert!(
        output.status.is_some_and(|status| status.success()),
        "exec {args:?} in {}: status {:?}\nstdout: {}\nstderr: {}",
        container.service,
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

async fn http(container: &ServiceContainer) -> String {
    let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", container.port.unwrap()))
        .await
        .unwrap();
    socket
        .write_all(b"GET / HTTP/1.0\r\nHost: fixture\r\n\r\n")
        .await
        .unwrap();
    let mut response = String::new();
    tokio::time::timeout(Duration::from_secs(5), socket.read_to_string(&mut response))
        .await
        .unwrap()
        .unwrap();
    assert!(
        response.starts_with("HTTP/1.0 200") || response.starts_with("HTTP/1.1 200"),
        "{response}"
    );
    response
}

#[tokio::test]
#[ignore = "requires Docker Compose and shared Docker build paths; run with the Docker integration harness"]
async fn compose_model_preserves_large_environment_values() {
    let mut fixture = Fixture::new();
    let value = "x".repeat(96 * 1024);
    let model = serde_json::json!({"services": {"web": {
        "image": "docker.io/library/busybox:1.37",
        "environment": {"LARGE_VALUE": value}
    }}});
    let runtime = fixture.unprepared_release("large", &model.to_string());
    std::fs::write(
        runtime.directory.join(".env"),
        "COMPOSE_FILE=/opt/tako/secret.key\nCOMPOSE_ENV_FILES=/opt/tako/secret.key\n",
    )
    .unwrap();
    let (snapshot, _guard) = runtime.snapshot().unwrap();
    let resolved = snapshot.model().await.unwrap();
    assert_eq!(
        resolved["services"]["web"]["environment"]["LARGE_VALUE"],
        value
    );
}

#[tokio::test]
#[ignore = "requires Docker Compose and shared Docker build paths; run with the Docker integration harness"]
async fn unchanged_non_root_service_keeps_its_private_mount_across_releases() {
    use std::os::unix::fs::PermissionsExt;
    let mut fixture = Fixture::new();
    let mut model: serde_json::Value = serde_yaml_ng::from_str(SMALL_STACK).unwrap();
    model["services"]["reader"] = serde_json::json!({
        "image":"docker.io/library/busybox:1.37", "user":"12345:12345",
        "command":["sh", "-c", "cat /input/settings.txt; sleep 3600"],
        "volumes":["./input:/input:ro"], "deploy":{"update_config":{"monitor":"0s"}}
    });
    let yaml = model.to_string();
    let mut original = None;
    let script = "#!/bin/sh\necho stable-input\n";
    for (version, mode) in [
        ("mount-v1", 0o644),
        ("mount-v2", 0o644),
        ("mount-v3", 0o755),
    ] {
        let runtime = fixture.unprepared_release(version, &yaml);
        std::fs::create_dir(runtime.directory.join("input")).unwrap();
        let path = runtime.directory.join("input/settings.txt");
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        runtime.prepare(version).await.unwrap();
        let mut rollout = runtime.begin_rollout().await.unwrap();
        rollout.apply().await.unwrap();
        let reader = service(rollout.containers(), "reader");
        assert_eq!(exec(reader, &["cat", "/input/settings.txt"]).await, script);
        assert_eq!(
            exec(reader, &["stat", "-c", "%a", "/input/settings.txt"])
                .await
                .trim(),
            format!("{mode:o}")
        );
        if let Some(id) = &original {
            if mode == 0o755 {
                assert_ne!(&reader.id, id);
                assert_eq!(
                    exec(reader, &["/input/settings.txt"]).await.trim(),
                    "stable-input"
                );
            } else {
                assert_eq!(&reader.id, id);
            }
        } else {
            original = Some(reader.id.clone());
        }
        rollout.finish().await.unwrap();
    }
}

#[tokio::test]
#[ignore = "requires Docker Compose and shared Docker build paths; run with the Docker integration harness"]
async fn rollout_uses_private_prepared_inputs_after_release_tampering() {
    let mut fixture = Fixture::new();
    let mut model: Value = serde_yaml_ng::from_str(SMALL_STACK).unwrap();
    model["secrets"] = serde_json::json!({"token": {"environment": "DB_PASSWORD"}});
    model["configs"] = serde_json::json!({"token_config": {"environment": "DB_PASSWORD"}});
    model["services"]["web"]["secrets"] = serde_json::json!(["token"]);
    model["services"]["web"]["configs"] =
        serde_json::json!([{"source":"token_config", "target":"/token-config"}]);
    let yaml = model.to_string();
    let runtime = fixture.release("immutable", &yaml).await;
    std::fs::write(
        runtime.directory.join("compose.yaml"),
        "services:\n  web:\n    image: busybox\n    privileged: true\n    pid: host\n",
    )
    .unwrap();
    std::fs::write(
        runtime.directory.join(".tako-compose-images.json"),
        "{\"web\":\"unprepared-image\"}",
    )
    .unwrap();
    std::fs::write(runtime.directory.join("Dockerfile"), "FROM scratch\n").unwrap();
    let mut rollout = runtime.begin_rollout().await.unwrap();
    rollout.apply().await.unwrap();
    assert!(
        http(rollout.ingress().unwrap())
            .await
            .ends_with("immutable\n")
    );
    assert!(
        rollout
            .containers()
            .iter()
            .any(|container| container.service == "redis")
    );
    let mut inspect = docker_command(false);
    inspect.args([
        "inspect",
        "--format",
        "{{.HostConfig.Privileged}}:{{.HostConfig.PidMode}}",
        &rollout.ingress().unwrap().id,
    ]);
    assert_eq!(
        String::from_utf8(
            engine_output(inspect, Duration::from_secs(10))
                .await
                .unwrap()
        )
        .unwrap()
        .trim(),
        "false:"
    );
    let redis_id = service(rollout.containers(), "redis").id.clone();
    let web_id = rollout.ingress().unwrap().id.clone();
    assert_eq!(
        exec(
            rollout.ingress().unwrap(),
            &["cat", "/run/secrets/token", "/token-config"]
        )
        .await,
        "fixture-onlyfixture-only"
    );
    rollout.finish().await.unwrap();

    // Secret changes still interpolate the captured source, without rebuilding
    // images or rereading app-writable files.
    let mut changed = runtime.clone();
    changed
        .env
        .insert("DB_PASSWORD".into(), "rotated-fixture-only".into());
    let mut rotated = changed.begin_rollout().await.unwrap();
    rotated.apply().await.unwrap();
    assert_ne!(rotated.ingress().unwrap().id, web_id);
    assert_eq!(
        exec(
            rotated.ingress().unwrap(),
            &["cat", "/run/secrets/token", "/token-config"]
        )
        .await,
        "rotated-fixture-onlyrotated-fixture-only"
    );
    assert_eq!(service(rotated.containers(), "redis").id, redis_id);
    rotated.finish().await.unwrap();
    fn assert_private_files(path: &std::path::Path) {
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                assert_private_files(&path);
            } else {
                let bytes = std::fs::read(path).unwrap();
                assert!(
                    !bytes
                        .windows(b"rotated-fixture-only".len())
                        .any(|v| v == b"rotated-fixture-only")
                );
            }
        }
    }
    assert_private_files(&runtime.preparation_root().unwrap());
}

#[tokio::test]
#[ignore = "requires Docker Compose and shared Docker build paths; run with the Docker integration harness"]
async fn rollback_restarts_renamed_dependencies_before_waiting_for_consumers() {
    let mut fixture = Fixture::new();
    let model = serde_json::json!({
        "services": {
            "redis": {
                "image": "docker.io/library/redis:7-alpine",
                "volumes": ["state:/data"],
                "healthcheck": {"test": ["CMD", "redis-cli", "ping"], "interval":"1s", "timeout":"1s", "retries":3},
                "deploy": {"update_config": {"monitor":"0s"}}
            },
            "web": {
                "image": "docker.io/library/redis:7-alpine", "tmpfs": ["/data"],
                "command": ["sh", "-c", "while true; do sleep 1; done"],
                "depends_on": {"redis": {"condition":"service_healthy"}},
                "healthcheck": {"test": ["CMD", "redis-cli", "-h", "redis", "ping"], "interval":"1s", "timeout":"1s", "retries":3},
                "deploy": {"update_config": {"order":"stop-first", "monitor":"0s"}}
            }
        },
        "volumes": {"state": {}}
    });
    let runtime = fixture.release("before-rename", &model.to_string()).await;
    let mut before = runtime.begin_rollout().await.unwrap();
    before.apply().await.unwrap();
    before.finish().await.unwrap();
    let web = before.ingress().unwrap().clone();
    let redis = service(before.containers(), "redis").clone();
    exec(&redis, &["redis-cli", "SET", "marker", "preserved"]).await;

    let mut next = model;
    next["services"]["cache"] = next["services"]
        .as_object_mut()
        .unwrap()
        .remove("redis")
        .unwrap();
    next["services"]["web"]["depends_on"] =
        serde_json::json!({"cache": {"condition":"service_healthy"}});
    next["services"]["web"]["command"] = serde_json::json!(["sh", "-c", "exit 1"]);
    let changed = fixture.release("after-rename", &next.to_string()).await;
    let mut pending = changed.begin_rollout().await.unwrap();
    assert!(pending.apply().await.is_err());
    assert!(
        !inspect_container(&redis.id, false)
            .await
            .unwrap()
            .unwrap()
            .running()
    );
    assert!(
        !inspect_container(&web.id, false)
            .await
            .unwrap()
            .unwrap()
            .running()
    );
    tokio::time::timeout(Duration::from_secs(30), pending.rollback())
        .await
        .unwrap()
        .unwrap();
    assert!(
        inspect_container(&web.id, false)
            .await
            .unwrap()
            .unwrap()
            .healthy()
    );
    assert_eq!(
        exec(&web, &["redis-cli", "-h", "redis", "GET", "marker"])
            .await
            .trim(),
        "preserved"
    );
}

#[tokio::test]
#[ignore = "requires Docker Compose and shared Docker build paths; run with the Docker integration harness"]
async fn build_inputs_are_local_copied_and_validated_before_building() {
    let mut fixture = Fixture::new();
    let runtime = fixture.unprepared_release(
        "build-inputs",
        r#"
services:
  web:
    build:
      context: .
      additional_contexts:
        extra: ./extra
      secrets: [build_token]
secrets:
  build_token:
    file: ./build-token
"#,
    );
    std::fs::create_dir(runtime.directory.join("extra")).unwrap();
    std::fs::write(runtime.directory.join("extra/marker"), "safe-context\n").unwrap();
    std::fs::write(runtime.directory.join("build-token"), "safe-build-token").unwrap();
    std::fs::write(runtime.directory.join("Dockerfile"), "FROM docker.io/library/busybox:1.37\nCOPY --from=extra marker /www/index.html\nRUN --mount=type=secret,id=build_token test \"$(cat /run/secrets/build_token)\" = safe-build-token\nCMD [\"httpd\",\"-f\",\"-p\",\"8080\",\"-h\",\"/www\"]\n").unwrap();
    runtime.prepare("build-inputs").await.unwrap();
    std::fs::write(
        runtime.directory.join("extra/marker"),
        "changed-after-build\n",
    )
    .unwrap();
    let mut rollout = runtime.begin_rollout().await.unwrap();
    rollout.apply().await.unwrap();
    assert!(
        http(rollout.ingress().unwrap())
            .await
            .ends_with("safe-context\n")
    );
    rollout.finish().await.unwrap();
}

#[tokio::test]
#[ignore = "requires Docker Compose and shared Docker build paths; run with the Docker integration harness"]
async fn production_rollouts_preserve_dependencies_and_recover_interrupted_generations() {
    let mut fixture = Fixture::new();
    let yaml = include_str!("../../../e2e/fixtures/compose/web-worker/compose.yaml").replace(
        "    build: .",
        "    build: .\n    restart: unless-stopped\n    environment:\n      LITERAL: ${LITERAL}",
    );
    let v1 = fixture.release("v1", &yaml).await;
    let mut first = v1.begin_rollout().await.unwrap();
    first.apply().await.unwrap();
    first.finish().await.unwrap();
    let initial = first.containers().to_vec();
    // An already passing Docker health check must not bypass an explicitly
    // requested observation window (dependency readiness uses a zero window).
    let monitoring_started = Instant::now();
    wait_ready(service(&initial, "web"), None, Duration::from_secs(1))
        .await
        .unwrap();
    assert!(monitoring_started.elapsed() >= Duration::from_secs(1));
    assert!(http(service(&initial, "web")).await.ends_with("v1\n"));
    assert_eq!(
        exec(service(&initial, "web"), &["printenv", "LITERAL"])
            .await
            .trim(),
        "literal$VALUE/${UNKNOWN}/$$"
    );
    // This IPv4 fixture uses Docker's service alias, independently of the CI
    // host's search domains or upstream IPv6 DNS configuration.
    assert_eq!(
        exec(
            service(&initial, "web"),
            &["nslookup", "-type=A", "redis", "127.0.0.11"]
        )
        .await
        .matches("redis")
        .count()
            > 0,
        true
    );
    let mut inspect = docker_command(false);
    inspect.args([
        "inspect",
        "--format",
        "{{.HostConfig.RestartPolicy.Name}}",
        &service(&initial, "web").id,
    ]);
    assert_eq!(
        String::from_utf8_lossy(
            &engine_output(inspect, Duration::from_secs(10))
                .await
                .unwrap()
        )
        .trim(),
        "unless-stopped"
    );
    exec(
        service(&initial, "redis"),
        &["redis-cli", "SET", "marker", "retained"],
    )
    .await;
    exec(
        service(&initial, "db"),
        &[
            "psql",
            "-U",
            "app",
            "-c",
            "CREATE TABLE marker (value TEXT); INSERT INTO marker VALUES ('retained')",
        ],
    )
    .await;
    // Database-native backup/restore remains separate from image rollback.
    exec(service(&initial,"db"), &["sh","-c","pg_dump -U app --no-owner --no-acl > /tmp/marker.sql && createdb -U app restored && psql -U app -d restored -v ON_ERROR_STOP=1 < /tmp/marker.sql"]).await;
    assert_eq!(
        exec(
            service(&initial, "db"),
            &[
                "psql",
                "-U",
                "app",
                "-d",
                "restored",
                "-Atc",
                "SELECT value FROM marker"
            ]
        )
        .await
        .trim(),
        "retained"
    );

    // Log clients are observers, not the owners of any container.
    let mut logs = v1.logs(first.containers()).await.unwrap();
    let mut stdout = logs.stdout.take().unwrap();
    let mut buffer = vec![0; 4096];
    let n = tokio::time::timeout(Duration::from_secs(10), stdout.read(&mut buffer))
        .await
        .unwrap()
        .unwrap();
    assert!(n > 0);
    logs.kill().await.unwrap();
    let mut adopted = v1.begin_rollout().await.unwrap();
    assert!(adopted.changes().is_empty());
    adopted.apply().await.unwrap();
    adopted.finish().await.unwrap();
    assert_eq!(adopted.ingress().unwrap().id, service(&initial, "web").id);
    assert_eq!(
        adopted.ingress().unwrap().port,
        service(&initial, "web").port
    );

    let v2 = fixture.release("v2", &yaml).await;
    let mut pending = v2.begin_rollout().await.unwrap();
    assert_eq!(
        pending.changes(),
        vec![("web".into(), UpdateOrder::StartFirst)]
    );
    pending.apply().await.unwrap();
    let candidate_id = pending.ingress().unwrap().id.clone();
    assert!(http(pending.ingress().unwrap()).await.ends_with("v2\n"));
    assert!(http(service(&initial, "web")).await.ends_with("v1\n"));
    for name in ["db", "redis", "worker"] {
        assert_eq!(
            service(pending.containers(), name).id,
            service(&initial, name).id
        );
    }
    // Termination before commit: the committed v1 reconciler adopts its own
    // healthy generation and removes only the abandoned candidate.
    drop(pending);
    let mut recovered = v1.begin_rollout().await.unwrap();
    recovered.apply().await.unwrap();
    recovered.finish().await.unwrap();
    assert_eq!(recovered.ingress().unwrap().id, service(&initial, "web").id);
    assert!(
        inspect_container(&candidate_id, false)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        exec(
            service(recovered.containers(), "redis"),
            &["redis-cli", "GET", "marker"]
        )
        .await
        .trim(),
        "retained"
    );

    // Termination after commit: reconciling v2 adopts its already-running web
    // container rather than recreating it, then removes v1.
    let mut committed = v2.begin_rollout().await.unwrap();
    committed.apply().await.unwrap();
    let committed_id = committed.ingress().unwrap().id.clone();
    drop(committed);
    let mut restarted = v2.begin_rollout().await.unwrap();
    restarted.apply().await.unwrap();
    restarted.finish().await.unwrap();
    assert_eq!(restarted.ingress().unwrap().id, committed_id);
    assert!(
        inspect_container(&service(&initial, "web").id, false)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        service(restarted.containers(), "redis").id,
        service(&initial, "redis").id
    );

    let bad = fixture
        .release(
            "bad",
            &yaml.replace("wget -q -O /dev/null http://127.0.0.1:8080/", "exit 1"),
        )
        .await;
    let mut failed = bad.begin_rollout().await.unwrap();
    assert!(failed.apply().await.is_err());
    assert!(http(restarted.ingress().unwrap()).await.ends_with("v2\n"));
    failed.rollback().await.unwrap();
    assert_eq!(v2.containers().await.unwrap().len(), 4);

    // A failed Redis change uses stop-first, then restores the exact previous
    // container and its named volume. The HTTP container remains untouched.
    let bad_data = fixture
        .release(
            "bad-data",
            &yaml.replace(
                "[redis-server, --appendonly, \"yes\"]",
                "[sh, -c, \"exit 42\"]",
            ),
        )
        .await;
    let mut failed = bad_data.begin_rollout().await.unwrap();
    assert!(failed.apply().await.is_err());
    failed.rollback().await.unwrap();
    assert_eq!(
        exec(service(&initial, "redis"), &["redis-cli", "GET", "marker"])
            .await
            .trim(),
        "retained"
    );
    assert_eq!(
        exec(
            service(&initial, "db"),
            &["psql", "-U", "app", "-Atc", "SELECT value FROM marker"]
        )
        .await
        .trim(),
        "retained"
    );
    assert!(http(restarted.ingress().unwrap()).await.ends_with("v2\n"));
    v2.stop_container(&committed_id).await.unwrap();
    assert!(
        inspect_container(&service(&initial, "redis").id, false)
            .await
            .unwrap()
            .unwrap()
            .running()
    );
    v2.stop().await.unwrap();
    assert!(v2.containers().await.unwrap().iter().all(|c| !c.running()));
}

const SMALL_STACK: &str = r#"
services:
  web:
    build: .
    depends_on:
      redis:
        condition: service_healthy
    healthcheck:
      test: [CMD-SHELL, "wget -q -O /dev/null http://127.0.0.1:8080/"]
      interval: 1s
      retries: 2
  redis:
    image: docker.io/library/redis:7-alpine
    command: [redis-server, --appendonly, "yes"]
    volumes: [redis-data:/data]
    healthcheck:
      test: [CMD, redis-cli, ping]
      interval: 1s
      retries: 10
volumes:
  redis-data:
  web-data:
"#;

#[tokio::test]
#[ignore = "requires Docker Compose and shared build paths"]
async fn image_volumes_require_an_exact_explicit_mount() {
    let mut fixture = Fixture::new();
    let parent_volume = SMALL_STACK.replace(
        "    build: .",
        "    build: .\n    volumes: [web-data:/state]",
    );
    let runtime = fixture.unprepared_release("image-volume", &parent_volume);
    let dockerfile_path = runtime.directory.join("Dockerfile");
    let dockerfile = std::fs::read_to_string(&dockerfile_path).unwrap();
    std::fs::write(
        &dockerfile_path,
        format!("{dockerfile}\nVOLUME [\"/state/data\"]\n"),
    )
    .unwrap();
    runtime.prepare("image-volume").await.unwrap();
    let parent_tmpfs = SMALL_STACK.replace("    build: .", "    build: .\n    tmpfs: [/state]");
    for yaml in [&parent_volume, &parent_tmpfs] {
        std::fs::write(runtime.directory.join("compose.yaml"), yaml).unwrap();
        runtime.prepare("image-volume").await.unwrap();
        let error = runtime
            .begin_rollout()
            .await
            .err()
            .expect("a parent mount must not hide an implicit anonymous image volume");
        assert!(error.contains("image volume '/state/data'"), "{error}");
        assert!(runtime.containers().await.unwrap().is_empty());
    }

    let exact_volume = parent_volume.replace("web-data:/state", "web-data:/state/data");
    std::fs::write(runtime.directory.join("compose.yaml"), exact_volume).unwrap();
    runtime.prepare("image-volume").await.unwrap();
    let mut rollout = runtime.begin_rollout().await.unwrap();
    rollout.apply().await.unwrap();
    assert_eq!(
        rollout.ingress().unwrap().writable_volumes,
        BTreeSet::from([format!("{}_web-data", runtime.project)]),
    );
    assert!(
        http(rollout.ingress().unwrap())
            .await
            .ends_with("image-volume\n")
    );
    rollout.finish().await.unwrap();

    let exact_tmpfs = parent_tmpfs.replace("tmpfs: [/state]", "tmpfs: [/state/data]");
    std::fs::write(runtime.directory.join("compose.yaml"), exact_tmpfs).unwrap();
    runtime.prepare("image-volume").await.unwrap();
    runtime.begin_rollout().await.unwrap();
}

#[tokio::test]
#[ignore = "requires Docker Compose and shared build paths"]
async fn readiness_rejects_restarts_between_healthy_observations() {
    let mut fixture = Fixture::new();
    let runtime = fixture.release("restart-monitor", SMALL_STACK).await;
    let mut rollout = runtime.begin_rollout().await.unwrap();
    rollout.apply().await.unwrap();
    let before = rollout.ingress().unwrap().clone();
    assert!(before.healthy());
    let mut command = docker_command(false);
    command.args(["restart", &before.id]);
    engine_output(command, Duration::from_secs(30))
        .await
        .unwrap();
    let after = inspect_container(&before.id, false).await.unwrap().unwrap();
    let after = wait_ready(&after, None, Duration::ZERO).await.unwrap();
    assert!(after.healthy());
    assert_ne!(before.started_at, after.started_at);
    // Both observations are healthy and the ID is unchanged, but the process
    // did not remain stable. A state-only polling loop would accept this.
    let error = wait_ready(&before, None, Duration::from_secs(1))
        .await
        .unwrap_err();
    assert!(error.contains("restarted"), "{error}");
}

#[tokio::test]
#[ignore = "requires Docker Compose and shared build paths"]
async fn rebuilt_image_ids_with_identical_runtime_content_keep_running_containers() {
    let mut fixture = Fixture::new();
    let runtime = fixture.unprepared_release(
        "image-identity",
        r#"
services:
  web:
    image: docker.io/library/busybox:1.37
    command: [sh, -c, 'mkdir -p /www; echo stable > /www/index.html; exec httpd -f -p 8080 -h /www']
    deploy:
      update_config:
        monitor: 0s
"#,
    );
    let archive = fixture.dir.path().join("rootfs.tar");
    let mut create = docker_command(false);
    create.args(["create", "docker.io/library/busybox:1.37", "sh"]);
    let source = String::from_utf8(
        engine_output(create, Duration::from_secs(30))
            .await
            .unwrap(),
    )
    .unwrap();
    let mut export = docker_command(false);
    export
        .args(["export", "--output"])
        .arg(&archive)
        .arg(source.trim());
    engine_output(export, Duration::from_secs(30))
        .await
        .unwrap();
    let mut remove = docker_command(false);
    remove.args(["rm", source.trim()]);
    engine_output(remove, Duration::from_secs(30))
        .await
        .unwrap();
    let mut previous = None;
    for attempt in 0..2 {
        // Import exactly the same filesystem and configuration twice. The
        // creation timestamp changes the image ID, like new attestations do,
        // without changing anything that a running container consumes.
        let mut import = docker_command(false);
        import
            .args(["import", "--change", "CMD [\"sh\"]"])
            .arg(&archive);
        let id = String::from_utf8(
            engine_output(import, Duration::from_secs(30))
                .await
                .unwrap(),
        )
        .unwrap();
        let id = id.trim();
        let mut inspect = docker_command(false);
        inspect.args(["image", "inspect", "--format", "{{json .}}", id]);
        let metadata: Value = serde_json::from_slice(
            &engine_output(inspect, Duration::from_secs(30))
                .await
                .unwrap(),
        )
        .unwrap();
        let identity = crate::image_fingerprint(&metadata).unwrap();
        let tag = format!("tako-compose/{}:{identity}", runtime.project);
        let mut tag_image = docker_command(false);
        tag_image.args(["tag", id, &tag]);
        engine_output(tag_image, Duration::from_secs(30))
            .await
            .unwrap();
        let (_, snapshot) = runtime.snapshot().unwrap();
        runtime
            .commit_preparation(snapshot, BTreeMap::from([("web".into(), tag)]))
            .unwrap();
        let mut rollout = runtime.begin_rollout().await.unwrap();
        assert_eq!(rollout.plan.model["services"]["web"]["image"], id);
        if let Some((old_id, old_identity, _)) = &previous {
            assert_ne!(old_id, id);
            assert_eq!(old_identity, &identity);
            assert!(rollout.changes().is_empty());
        }
        rollout.apply().await.unwrap();
        let container = rollout.ingress().unwrap();
        assert!(http(container).await.ends_with("stable\n"));
        if attempt == 1 {
            assert_eq!(container.id, previous.as_ref().unwrap().2);
        }
        previous = Some((id.to_string(), identity, container.id.clone()));
        rollout.finish().await.unwrap();
    }
}

#[tokio::test]
#[ignore = "requires Docker Compose and shared build paths"]
async fn concurrent_preparations_pin_their_own_images() {
    let mut fixture = Fixture::new();
    let shared_tag = format!(
        "tako-compose-test/shared:{}",
        crate::digest(&fixture.dir.path().display().to_string())
    );
    let yaml = SMALL_STACK.replace(
        "    build: .",
        &format!("    build: .\n    image: {shared_tag}"),
    );
    let left = fixture.unprepared_release("parallel-left", &yaml);
    let right = fixture.unprepared_release("parallel-right", &yaml);
    tokio::try_join!(
        left.prepare("parallel-left"),
        right.prepare("parallel-right")
    )
    .unwrap();
    let mut first = left.begin_rollout().await.unwrap();
    first.apply().await.unwrap();
    assert!(
        http(first.ingress().unwrap())
            .await
            .ends_with("parallel-left\n")
    );
    let mut second = right.begin_rollout().await.unwrap();
    second.apply().await.unwrap();
    assert!(
        http(second.ingress().unwrap())
            .await
            .ends_with("parallel-right\n")
    );
    assert!(
        http(first.ingress().unwrap())
            .await
            .ends_with("parallel-left\n")
    );
    // Build isolation must also preserve identity on repeated preparation.
    right.prepare("parallel-right").await.unwrap();
    let repeated = right.begin_rollout().await.unwrap();
    assert!(repeated.changes().is_empty());
    second.finish().await.unwrap();
}

#[tokio::test]
#[ignore = "requires Docker Compose and shared build paths"]
async fn stateful_ingress_recovers_without_overlapping_volume_writers() {
    let mut fixture = Fixture::new();
    let yaml = SMALL_STACK.replace(
        "    build: .",
        "    build: .\n    volumes: [web-data:/state]",
    );
    let v1 = fixture.release("stateful-v1", &yaml).await;
    let mut first = v1.begin_rollout().await.unwrap();
    assert!(
        first
            .changes()
            .contains(&("web".into(), UpdateOrder::StopFirst))
    );
    first.apply().await.unwrap();
    first.finish().await.unwrap();
    let old = first.ingress().unwrap().clone();
    exec(&old, &["sh", "-c", "echo retained > /state/marker"]).await;
    let v2 = fixture.release("stateful-v2", &yaml).await;
    let mut pending = v2.begin_rollout().await.unwrap();
    pending.apply().await.unwrap();
    assert!(
        !inspect_container(&old.id, false)
            .await
            .unwrap()
            .unwrap()
            .running()
    );
    assert_eq!(
        exec(pending.ingress().unwrap(), &["cat", "/state/marker"])
            .await
            .trim(),
        "retained"
    );
    let candidate = pending.ingress().unwrap().id.clone();
    drop(pending);
    let mut recovered = v1.begin_rollout().await.unwrap();
    recovered.apply().await.unwrap();
    // Restarting the committed writer stops the abandoned writer first, even
    // when recovery has no in-memory handle for the interrupted deployment.
    assert!(
        !inspect_container(&candidate, false)
            .await
            .unwrap()
            .unwrap()
            .running()
    );
    assert_eq!(recovered.ingress().unwrap().id, old.id);
    assert!(
        http(recovered.ingress().unwrap())
            .await
            .ends_with("stateful-v1\n")
    );
    recovered.finish().await.unwrap();
    assert_eq!(
        exec(recovered.ingress().unwrap(), &["cat", "/state/marker"])
            .await
            .trim(),
        "retained"
    );
}

#[tokio::test]
#[ignore = "requires Docker Compose and shared build paths"]
async fn worker_replicas_and_completed_jobs_keep_their_independent_lifecycles() {
    let mut fixture = Fixture::new();
    let job = r#"  migrate:
    image: docker.io/library/redis:7-alpine
    tmpfs: [/data]
    command: [redis-cli, -h, redis, INCR, migrations]
    restart: "no"
    depends_on:
      redis:
        condition: service_healthy
  worker:
    image: docker.io/library/redis:7-alpine
    tmpfs: [/data]
    command: [sh, -c, "while true; do sleep 1; done"]
    environment:
      WORKER_VERSION: one
    deploy:
      replicas: 2
      update_config:
        order: start-first
        monitor: 0s
"#;
    let yaml = SMALL_STACK
        .replacen(
            "    depends_on:",
            "    depends_on:\n      migrate:\n        condition: service_completed_successfully",
            1,
        )
        .replace("\nvolumes:\n", &format!("\n{job}volumes:\n"));
    let v1 = fixture.release("jobs-v1", &yaml).await;
    let mut first = v1.begin_rollout().await.unwrap();
    first.apply().await.unwrap();
    first.finish().await.unwrap();
    let job_id = service(first.containers(), "migrate").id.clone();
    let initial_workers: BTreeSet<_> = first
        .containers()
        .iter()
        .filter(|c| c.service == "worker")
        .map(|c| c.id.clone())
        .collect();
    assert_eq!(initial_workers.len(), 2);
    assert_eq!(
        exec(
            service(first.containers(), "redis"),
            &["redis-cli", "GET", "migrations"]
        )
        .await
        .trim(),
        "1"
    );

    let v2 = fixture
        .release(
            "jobs-v2",
            &yaml.replace("WORKER_VERSION: one", "WORKER_VERSION: two"),
        )
        .await;
    let mut next = v2.begin_rollout().await.unwrap();
    next.apply().await.unwrap();
    assert_eq!(service(next.containers(), "migrate").id, job_id);
    let overlapping = v2
        .containers()
        .await
        .unwrap()
        .into_iter()
        .filter(|c| c.service == "worker" && c.running())
        .count();
    assert_eq!(overlapping, 4);
    next.finish().await.unwrap();
    let workers: BTreeSet<_> = next
        .containers()
        .iter()
        .filter(|c| c.service == "worker")
        .map(|c| c.id.clone())
        .collect();
    assert!(workers.is_disjoint(&initial_workers));
    assert_eq!(
        exec(
            service(next.containers(), "redis"),
            &["redis-cli", "GET", "migrations"]
        )
        .await
        .trim(),
        "1"
    );

    let scaled_yaml = yaml
        .replace("WORKER_VERSION: one", "WORKER_VERSION: two")
        .replace("replicas: 2", "replicas: 3");
    let v3 = fixture.release("jobs-v3", &scaled_yaml).await;
    let mut scaled = v3.begin_rollout().await.unwrap();
    scaled.apply().await.unwrap();
    scaled.finish().await.unwrap();
    let scaled_workers: BTreeSet<_> = scaled
        .containers()
        .iter()
        .filter(|c| c.service == "worker")
        .map(|c| c.id.clone())
        .collect();
    assert_eq!(scaled_workers.len(), 3);
    assert!(workers.is_subset(&scaled_workers));
    assert_eq!(service(scaled.containers(), "migrate").id, job_id);
}
