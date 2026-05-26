use super::super::task_tree::{
    ArtifactBuildGroup, DeployTaskTreeController, UNIFIED_JS_CACHE_TARGET_LABEL, build_deploy_tree,
    deploy_target_task_id, deploy_task_step_id,
};
use super::*;
use crate::ui;
use crate::ui::TaskState;
use std::time::Duration;

#[test]
fn parse_existing_routes_from_ok_response_keeps_empty_routes_and_ignores_malformed_entries() {
    let response = Response::Ok {
        data: serde_json::json!({
            "routes": [
                {"app": "good-a", "routes": ["a.example.com", "*.a.example.com"]},
                {"app": "missing-routes"},
                {"routes": ["missing-app.example.com"]},
                {"app": "good-b", "routes": ["b.example.com/path/*"]}
            ]
        }),
    };

    let parsed = parse_existing_routes_response(response).expect("should parse");
    assert_eq!(parsed.len(), 3);
    assert_eq!(parsed[0].0, "good-a");
    assert_eq!(parsed[1].0, "missing-routes");
    assert!(parsed[1].1.is_empty());
    assert_eq!(parsed[2].0, "good-b");
}

#[test]
fn parse_existing_routes_from_error_response_returns_message() {
    let response = Response::Error {
        message: "boom".to_string(),
    };
    let err = parse_existing_routes_response(response).unwrap_err();
    assert!(err.contains("boom"));
}

#[test]
fn deploy_ssl_binding_for_start_command_omits_provider_token() {
    let ssl = tako_core::SslBinding {
        provider: tako_core::SslProvider::Cloudflare,
        cloudflare_api_token: Some("server-only-token".to_string()),
    };

    let prepared = ssl_binding_for_start_command(&ssl);

    assert_eq!(prepared.provider, tako_core::SslProvider::Cloudflare);
    assert_eq!(prepared.cloudflare_api_token, None);
}

fn sample_shared_build_group() -> ArtifactBuildGroup {
    ArtifactBuildGroup {
        build_target_label: "linux-aarch64-musl".to_string(),
        cache_target_label: UNIFIED_JS_CACHE_TARGET_LABEL.to_string(),
        target_labels: vec!["linux-aarch64-musl".to_string()],
        display_target_label: None,
    }
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
