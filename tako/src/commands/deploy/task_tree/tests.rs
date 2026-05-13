use super::super::remote::run_task_tree_deploy_step;
use super::super::remote::run_task_tree_deploy_step_with_detail_and_error_cleanup;
use super::*;
use crate::config::ServerTarget;
use crate::ui;
use std::time::Duration;

fn sample_shared_build_group() -> ArtifactBuildGroup {
    ArtifactBuildGroup {
        build_target_label: "linux-aarch64-musl".to_string(),
        cache_target_label: UNIFIED_JS_CACHE_TARGET_LABEL.to_string(),
        target_labels: vec!["linux-aarch64-musl".to_string()],
        display_target_label: None,
    }
}

fn sample_multi_build_groups() -> Vec<ArtifactBuildGroup> {
    vec![
        ArtifactBuildGroup {
            build_target_label: "linux-aarch64-musl".to_string(),
            cache_target_label: "linux-aarch64-musl".to_string(),
            target_labels: vec!["linux-aarch64-musl".to_string()],
            display_target_label: Some("linux-aarch64-musl".to_string()),
        },
        ArtifactBuildGroup {
            build_target_label: "linux-x86_64-glibc".to_string(),
            cache_target_label: "linux-x86_64-glibc".to_string(),
            target_labels: vec!["linux-x86_64-glibc".to_string()],
            display_target_label: Some("linux-x86_64-glibc".to_string()),
        },
    ]
}

#[test]
fn build_artifact_target_groups_unifies_targets_when_requested() {
    let server_targets = vec![
        (
            "a".to_string(),
            ServerTarget {
                arch: "x86_64".to_string(),
                libc: "glibc".to_string(),
            },
        ),
        (
            "b".to_string(),
            ServerTarget {
                arch: "aarch64".to_string(),
                libc: "musl".to_string(),
            },
        ),
    ];

    let groups = build_artifact_target_groups(&server_targets, true);
    assert_eq!(
        groups,
        vec![ArtifactBuildGroup {
            build_target_label: "linux-aarch64-musl".to_string(),
            cache_target_label: UNIFIED_JS_CACHE_TARGET_LABEL.to_string(),
            target_labels: vec![
                "linux-aarch64-musl".to_string(),
                "linux-x86_64-glibc".to_string()
            ],
            display_target_label: None,
        }]
    );
}

#[test]
fn build_artifact_target_groups_keeps_per_target_groups_when_not_unified() {
    let server_targets = vec![
        (
            "a".to_string(),
            ServerTarget {
                arch: "x86_64".to_string(),
                libc: "glibc".to_string(),
            },
        ),
        (
            "b".to_string(),
            ServerTarget {
                arch: "aarch64".to_string(),
                libc: "musl".to_string(),
            },
        ),
    ];

    let groups = build_artifact_target_groups(&server_targets, false);
    assert_eq!(
        groups,
        vec![
            ArtifactBuildGroup {
                build_target_label: "linux-aarch64-musl".to_string(),
                cache_target_label: "linux-aarch64-musl".to_string(),
                target_labels: vec!["linux-aarch64-musl".to_string()],
                display_target_label: Some("linux-aarch64-musl".to_string()),
            },
            ArtifactBuildGroup {
                build_target_label: "linux-x86_64-glibc".to_string(),
                cache_target_label: "linux-x86_64-glibc".to_string(),
                target_labels: vec!["linux-x86_64-glibc".to_string()],
                display_target_label: Some("linux-x86_64-glibc".to_string()),
            },
        ]
    );
}

#[test]
fn deploy_task_tree_initial_lines_include_known_future_work() {
    let controller =
        DeployTaskTreeController::new(&["tako-demo".to_string()], &[sample_shared_build_group()]);
    let snapshot = controller.snapshot();
    let lines = ui::render_plain_lines(&build_deploy_tree(&snapshot));

    assert_eq!(
        lines,
        vec![
            "○ Building…".to_string(),
            String::new(),
            "○ Deploying to tako-demo…".to_string(),
            "  ○ Preflight…".to_string(),
            "  ○ Uploading…".to_string(),
            "  ○ Preparing…".to_string(),
            "  ○ Starting…".to_string(),
        ]
    );
}

#[test]
fn deploy_task_tree_initial_lines_include_multi_target_builds_and_multi_server_children() {
    let controller = DeployTaskTreeController::new(
        &["prod-a".to_string(), "prod-b".to_string()],
        &sample_multi_build_groups(),
    );
    let snapshot = controller.snapshot();
    let lines = ui::render_plain_lines(&build_deploy_tree(&snapshot));

    assert!(lines.iter().any(|line| line == "○ Building…"));
    assert!(lines.iter().any(|line| line == "  ○ linux-aarch64-musl…"));
    assert!(lines.iter().any(|line| line == "  ○ linux-x86_64-glibc…"));
    assert!(lines.iter().any(|line| line == "○ Deploying to prod-a…"));
    assert!(lines.iter().any(|line| line == "○ Deploying to prod-b…"));
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.contains("○ Preflight…"))
            .count(),
        2
    );
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.contains("○ Uploading…"))
            .count(),
        2
    );
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.contains("○ Preparing…"))
            .count(),
        2
    );
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.contains("○ Starting…"))
            .count(),
        2
    );
}

#[test]
fn deploy_task_tree_marks_build_as_running_before_build_steps_start() {
    let controller =
        DeployTaskTreeController::new(&["tako-demo".to_string()], &[sample_shared_build_group()]);

    controller.mark_build_target_running("shared target");

    let snapshot = controller.snapshot();
    let lines = ui::render_plain_lines(&build_deploy_tree(&snapshot));

    assert!(lines.iter().any(|line| line.starts_with("✶ Building")));
    let build = snapshot
        .builds
        .iter()
        .find(|task| task.label == "shared target")
        .unwrap();
    assert!(matches!(build.state, TaskState::Running { .. }));
    assert!(
        build
            .children
            .iter()
            .all(|child| matches!(child.state, TaskState::Pending))
    );
}

#[test]
fn deploy_task_tree_cache_hit_appends_completed_cached_artifact_step() {
    let controller =
        DeployTaskTreeController::new(&["prod-a".to_string()], &[sample_shared_build_group()]);

    controller.succeed_build_step("shared target", "probe-runtime", Some("bun 1.2.3".into()));
    controller.skip_build_step("shared target", "build-artifact", "skipped");
    controller.skip_build_step("shared target", "package-artifact", "skipped");
    controller.append_cached_artifact_step("shared target", Some("72 MB".to_string()));
    controller.succeed_build_target("shared target", Some("72 MB (cached)".to_string()));

    let snapshot = controller.snapshot();
    let cached_step = snapshot
        .builds
        .iter()
        .find_map(|task| task.find(&build_task_step_id("shared target", "use-cached-artifact")))
        .unwrap();
    assert!(matches!(cached_step.state, TaskState::Succeeded { .. }));
    let lines = ui::render_plain_lines(&build_deploy_tree(&snapshot));
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("✔ Built") && line.contains("72 MB (cached)")),
        "built line should show detail: {lines:?}"
    );
    let built_index = lines
        .iter()
        .position(|line| line.starts_with("✔ Built"))
        .unwrap();
    assert_eq!(lines.get(built_index + 1), Some(&String::new()));
    assert_eq!(
        lines.get(built_index + 2),
        Some(&"○ Deploying to prod-a…".to_string())
    );
    assert_eq!(lines.last(), Some(&"  ○ Starting…".to_string()));
}

#[test]
fn deploy_task_tree_can_show_parallel_running_rows() {
    let controller = DeployTaskTreeController::new(
        &["prod-a".to_string(), "prod-b".to_string()],
        &[sample_shared_build_group()],
    );

    controller.mark_deploy_step_running("prod-a", "uploading");
    controller.mark_deploy_step_running("prod-b", "uploading");

    let snapshot = controller.snapshot();
    let lines = ui::render_plain_lines(&build_deploy_tree(&snapshot));
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.starts_with("✶ Deploying to prod-"))
            .count(),
        2
    );
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.starts_with("  ✶ Uploading"))
            .count(),
        2
    );
}

#[tokio::test]
async fn deploy_task_tree_marks_preflight_running_before_complete() {
    let controller =
        DeployTaskTreeController::new(&["prod-a".to_string()], &[sample_shared_build_group()]);
    let worker_controller = controller.clone();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();

    let handle = tokio::spawn(async move {
        run_task_tree_deploy_step(&worker_controller, "prod-a", "connecting", async move {
            rx.await.expect("test signal should arrive");
            Ok::<(), String>(())
        })
        .await
        .expect("preflight step should succeed");
    });

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let snapshot = controller.snapshot();
            let deploy_target = snapshot
                .deploys
                .iter()
                .find(|task| task.id == deploy_target_task_id("prod-a"))
                .expect("deploy target should exist");
            let preflight = deploy_target
                .find(&deploy_task_step_id("prod-a", "connecting"))
                .expect("preflight step should exist");
            if matches!(deploy_target.state, TaskState::Running { .. })
                && matches!(preflight.state, TaskState::Running { .. })
            {
                assert_eq!(preflight.label, "Preflight");
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("preflight step should enter running state");

    tx.send(()).expect("worker should still be waiting");
    handle.await.expect("worker should finish cleanly");

    let snapshot = controller.snapshot();
    let deploy_target = snapshot
        .deploys
        .iter()
        .find(|task| task.id == deploy_target_task_id("prod-a"))
        .expect("deploy target should exist");
    let preflight = deploy_target
        .find(&deploy_task_step_id("prod-a", "connecting"))
        .expect("preflight step should exist");
    assert_eq!(preflight.label, "Preflight");
    assert!(matches!(preflight.state, TaskState::Succeeded { .. }));
}

#[test]
fn deploy_task_tree_shows_preflight_errors_under_deploy_group() {
    let controller =
        DeployTaskTreeController::new(&["prod-a".to_string()], &[sample_shared_build_group()]);

    controller.fail_preflight_check("prod-a", "SSH protocol error");

    let snapshot = controller.snapshot();
    let lines = ui::render_plain_lines(&build_deploy_tree(&snapshot));

    assert!(lines.iter().any(|line| line == "✘ Deploy to prod-a failed"));
    assert!(lines.iter().any(|line| line == "  ✘ Preflight failed"));
    assert!(lines.iter().any(|line| line == "    SSH protocol error"));
}

#[test]
fn deploy_task_tree_preflight_failure_aborts_remaining_deploy_children() {
    let controller =
        DeployTaskTreeController::new(&["prod-a".to_string()], &[sample_shared_build_group()]);

    controller.fail_preflight_check("prod-a", "SSH protocol error");

    let snapshot = controller.snapshot();
    let lines = ui::render_plain_lines(&build_deploy_tree(&snapshot));

    assert!(lines.iter().any(|line| line == "  ✘ Preflight failed"));
    assert!(
        lines
            .iter()
            .any(|line| line.contains("⊘ Uploading") && line.contains("cancelled"))
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("⊘ Preparing") && line.contains("cancelled"))
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("⊘ Starting") && line.contains("cancelled"))
    );
}

#[tokio::test]
async fn deploy_task_tree_step_failure_attaches_detail_to_child_row() {
    let controller =
        DeployTaskTreeController::new(&["prod-a".to_string()], &[sample_shared_build_group()]);

    let err = run_task_tree_deploy_step(&controller, "prod-a", "starting", async {
        Err::<(), String>("Warm instance startup failed".to_string())
    })
    .await
    .unwrap_err();
    assert_eq!(err.to_string(), "Warm instance startup failed");

    let lines = ui::render_plain_lines(&build_deploy_tree(&controller.snapshot()));
    assert!(lines.iter().any(|line| line == "✘ Deploy to prod-a failed"));
    assert!(lines.iter().any(|line| line == "  ✘ Start failed"));
    assert!(
        lines
            .iter()
            .any(|line| line == "    Warm instance startup failed")
    );
}

#[tokio::test]
async fn deploy_task_tree_defers_failed_start_state_until_cleanup_finishes() {
    let controller =
        DeployTaskTreeController::new(&["prod-a".to_string()], &[sample_shared_build_group()]);
    let (cleanup_started_tx, cleanup_started_rx) = tokio::sync::oneshot::channel();
    let (cleanup_finish_tx, cleanup_finish_rx) = tokio::sync::oneshot::channel::<()>();

    let task = tokio::spawn({
        let controller = controller.clone();
        async move {
            run_task_tree_deploy_step_with_detail_and_error_cleanup(
                &controller,
                "prod-a",
                "starting",
                None,
                async { Err::<(), String>("Warm instance startup failed".to_string()) },
                move || async move {
                    let _ = cleanup_started_tx.send(());
                    let _ = cleanup_finish_rx.await;
                },
            )
            .await
            .unwrap_err()
        }
    });

    cleanup_started_rx.await.unwrap();

    let lines = ui::render_plain_lines(&build_deploy_tree(&controller.snapshot()));
    assert!(
        lines
            .iter()
            .any(|line| line.ends_with("Deploying to prod-a…"))
    );
    assert!(lines.iter().any(|line| line.contains("Starting…")));
    assert!(!lines.iter().any(|line| line == "  ✘ Start failed"));
    assert!(
        !lines
            .iter()
            .any(|line| line == "    Warm instance startup failed")
    );

    let _ = cleanup_finish_tx.send(());
    let err = task.await.unwrap();
    assert_eq!(err.to_string(), "Warm instance startup failed");

    let lines = ui::render_plain_lines(&build_deploy_tree(&controller.snapshot()));
    assert!(lines.iter().any(|line| line == "✘ Deploy to prod-a failed"));
    assert!(lines.iter().any(|line| line == "  ✘ Start failed"));
    assert!(
        lines
            .iter()
            .any(|line| line == "    Warm instance startup failed")
    );
}

#[test]
fn deploy_task_tree_success_summary_appends_release_and_routes() {
    let controller =
        DeployTaskTreeController::new(&["prod-a".to_string()], &[sample_shared_build_group()]);
    controller.set_success_summary(
        "20260330",
        &["app.test".to_string(), "*.app.test".to_string()],
        None,
    );

    let lines = ui::render_plain_lines(&build_deploy_tree(&controller.snapshot()));
    assert!(
        lines.iter().any(|line| line == "Release 20260330"),
        "expected 'Release 20260330' in {lines:?}"
    );
    assert!(
        lines.iter().any(|line| line == "Routes  https://app.test"),
        "expected 'Routes  https://app.test' in {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line == "        https://*.app.test"),
        "expected continuation route in {lines:?}"
    );
}

#[test]
fn deploy_task_tree_can_append_error_summary_line() {
    let controller =
        DeployTaskTreeController::new(&["prod-a".to_string()], &[sample_shared_build_group()]);
    controller.fail_preflight_check("prod-a", "SSH protocol error");
    controller.set_error_summary("Deployed to 0/1 servers".to_string());

    let lines = ui::render_plain_lines(&build_deploy_tree(&controller.snapshot()));
    assert_eq!(
        lines.get(lines.len().saturating_sub(2)),
        Some(&String::new())
    );
    assert_eq!(lines.last(), Some(&"Deployed to 0/1 servers".to_string()));
}

#[test]
fn deploy_task_tree_build_failure_aborts_deploy_work() {
    let controller =
        DeployTaskTreeController::new(&["prod-a".to_string()], &[sample_shared_build_group()]);

    controller.mark_deploy_step_running("prod-a", "connecting");
    controller.fail_build_step("shared target", "build-artifact", "Local build failed");
    controller.fail_build_target("shared target", "Local build failed");
    controller.cancel_pending_build_children("shared target", "cancelled");
    controller.abort_incomplete("Aborted");

    let snapshot = controller.snapshot();
    let lines = ui::render_plain_lines(&build_deploy_tree(&snapshot));

    assert!(lines.iter().any(|line| line == "✘ Building"));
    assert!(lines.iter().any(|line| line == "  Local build failed"));
    assert!(lines.iter().any(|line| line == "  ⊘ Preflight…"));
    assert!(lines.iter().any(|line| line == "  ⊘ Uploading…"));
    assert!(lines.iter().any(|line| line == "  ⊘ Preparing…"));
    assert!(lines.iter().any(|line| line == "  ⊘ Starting…"));
}

#[test]
fn deploy_task_tree_omits_startup_summary_lines() {
    let controller =
        DeployTaskTreeController::new(&["prod-a".to_string()], &[sample_shared_build_group()]);
    let snapshot = controller.snapshot();
    let lines = ui::render_plain_lines(&build_deploy_tree(&snapshot));

    assert!(!lines.iter().any(|line| line.contains("https://")));
    assert!(!lines.iter().any(|line| line.contains("App")));
    assert!(!lines.iter().any(|line| line.contains("Env")));
}

#[test]
fn release_step_renders_under_preparing() {
    let controller = DeployTaskTreeController::new(&["la".to_string(), "nyc".to_string()], &[]);
    controller.add_release_step("la", /* leader */ true);
    controller.add_release_step("nyc", /* leader */ false);

    let state = controller.state.lock().unwrap();
    let la_preparing =
        find_deploy_step(&state.deploys, "la", "preparing").expect("preparing step for la");
    let release_la = la_preparing
        .children
        .iter()
        .find(|c| c.id == deploy_task_step_id("la", "release"))
        .expect("release step under la preparing");
    assert_eq!(release_la.label, "Running release command");

    let nyc_preparing =
        find_deploy_step(&state.deploys, "nyc", "preparing").expect("preparing step for nyc");
    let release_nyc = nyc_preparing
        .children
        .iter()
        .find(|c| c.id == deploy_task_step_id("nyc", "release"))
        .expect("release step under nyc preparing");
    assert_eq!(release_nyc.label, "Waiting for release command");
}

#[test]
fn add_release_step_is_idempotent() {
    let controller = DeployTaskTreeController::new(&["la".to_string()], &[]);
    controller.add_release_step("la", true);
    controller.add_release_step("la", true);

    let state = controller.state.lock().unwrap();
    let preparing = find_deploy_step(&state.deploys, "la", "preparing").expect("preparing step");
    let release_count = preparing
        .children
        .iter()
        .filter(|c| c.id == deploy_task_step_id("la", "release"))
        .count();
    assert_eq!(release_count, 1, "release sub-step should not duplicate");
}

#[test]
fn cancel_release_step_sets_cancelled_state() {
    let controller = DeployTaskTreeController::new(&["la".to_string()], &[]);
    controller.add_release_step("la", false);
    controller.cancel_release_step("la", "leader failed");

    let state = controller.state.lock().unwrap();
    let preparing = find_deploy_step(&state.deploys, "la", "preparing").unwrap();
    let release = preparing
        .children
        .iter()
        .find(|c| c.id == deploy_task_step_id("la", "release"))
        .expect("release step");
    assert!(
        matches!(release.state, crate::ui::TaskState::Cancelled { .. }),
        "expected Cancelled, got {:?}",
        release.state
    );
    assert_eq!(release.detail.as_deref(), Some("leader failed"));
}

fn find_deploy_step<'a>(
    deploys: &'a [crate::ui::TaskItemState],
    server_name: &str,
    step: &str,
) -> Option<&'a crate::ui::TaskItemState> {
    let server_id = deploy_target_task_id(server_name);
    let step_id = deploy_task_step_id(server_name, step);
    deploys
        .iter()
        .find(|t| t.id == server_id)?
        .children
        .iter()
        .find(|c| c.id == step_id)
}
