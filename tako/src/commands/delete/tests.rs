use super::*;

fn deployment(app: &str, env: &str, server: &str) -> RemoteDeployment {
    RemoteDeployment {
        remote_app_id: tako_core::deployment_app_id(app, env),
        app: app.to_string(),
        env: env.to_string(),
        server_name: server.to_string(),
    }
}

#[test]
fn delete_targets_filter_to_per_server_entries_for_app() {
    let deployments = vec![
        deployment("web", "production", "hkg"),
        deployment("web", "production", "lax"),
        deployment("web", "staging", "ord"),
        deployment("api", "production", "lax"),
    ];

    let targets = delete_targets(&deployments, Some("web"), None, None);
    assert_eq!(
        targets,
        vec![
            deployment("web", "production", "hkg"),
            deployment("web", "production", "lax"),
            deployment("web", "staging", "ord"),
        ]
    );
}

#[test]
fn delete_targets_filter_by_environment_and_server() {
    let deployments = vec![
        deployment("web", "production", "hkg"),
        deployment("web", "production", "lax"),
        deployment("web", "staging", "lax"),
        deployment("api", "production", "lax"),
    ];

    let targets = delete_targets(&deployments, Some("web"), Some("production"), Some("lax"));
    assert_eq!(targets, vec![deployment("web", "production", "lax")]);
}

#[test]
fn select_delete_target_prompt_without_flags_uses_env_from_server_labels() {
    let targets = vec![
        deployment("web", "production", "hkg"),
        deployment("web", "production", "lax"),
        deployment("web", "staging", "ord"),
    ];

    let options = delete_target_selection_options(&targets, true, None, None);
    assert_eq!(options.title, "Select deployment to delete");
    assert_eq!(
        options
            .choices
            .iter()
            .map(|(label, _)| label.clone())
            .collect::<Vec<_>>(),
        vec![
            "production from hkg".to_string(),
            "production from lax".to_string(),
            "staging from ord".to_string(),
        ]
    );
}

#[test]
fn select_delete_target_prompt_for_env_uses_server_labels() {
    let targets = vec![
        deployment("web", "production", "hkg"),
        deployment("web", "production", "lax"),
    ];

    let options = delete_target_selection_options(&targets, true, Some("production"), None);
    assert_eq!(options.title, "Select server to delete from");
    assert_eq!(
        options
            .choices
            .iter()
            .map(|(label, _)| label.clone())
            .collect::<Vec<_>>(),
        vec!["hkg".to_string(), "lax".to_string()]
    );
}

#[test]
fn select_delete_target_prompt_for_server_uses_environment_labels() {
    let targets = vec![
        deployment("web", "production", "lax"),
        deployment("web", "staging", "lax"),
    ];

    let options = delete_target_selection_options(&targets, true, None, Some("lax"));
    assert_eq!(options.title, "Select environment to delete");
    assert_eq!(
        options
            .choices
            .iter()
            .map(|(label, _)| label.clone())
            .collect::<Vec<_>>(),
        vec!["production".to_string(), "staging".to_string()]
    );
}

#[test]
fn resolve_delete_target_from_candidates_returns_exact_match() {
    let deployments = vec![
        deployment("web", "production", "hkg"),
        deployment("web", "production", "lax"),
        deployment("web", "staging", "lax"),
    ];

    let target = resolve_delete_target_from_candidates(
        &deployments,
        Some("web"),
        Some("production"),
        Some("lax"),
        false,
    )
    .unwrap();
    assert_eq!(target, deployment("web", "production", "lax"));
}

#[test]
fn resolve_delete_target_from_candidates_errors_when_non_interactive_selection_is_needed() {
    let deployments = vec![
        deployment("web", "production", "hkg"),
        deployment("web", "production", "lax"),
    ];

    let err = resolve_delete_target_from_candidates(
        &deployments,
        Some("web"),
        Some("production"),
        None,
        false,
    )
    .unwrap_err();
    assert!(err.contains("Multiple deployments match"));
    assert!(err.contains("--server"));
}

#[test]
fn resolve_delete_target_from_candidates_returns_not_found_error_for_unknown_server() {
    let deployments = vec![
        deployment("web", "production", "hkg"),
        deployment("web", "production", "lax"),
    ];

    let err = resolve_delete_target_from_candidates(
        &deployments,
        Some("web"),
        Some("production"),
        Some("ord"),
        false,
    )
    .unwrap_err();
    assert!(err.contains("not deployed"));
}

#[test]
fn validate_project_delete_env_rejects_development() {
    let config = TakoToml::default();
    let err = validate_project_delete_env("development", &config).unwrap_err();
    assert!(err.contains("reserved"));
}

#[test]
fn validate_project_delete_env_requires_known_env() {
    let mut config = TakoToml::default();
    config
        .envs
        .insert("production".to_string(), Default::default());

    let err = validate_project_delete_env("staging", &config).unwrap_err();
    assert!(err.contains("Environment 'staging' not found"));
}

#[test]
fn parse_delete_response_converts_error_response() {
    let err = parse_delete_response(Response::error("boom")).unwrap_err();
    assert!(err.contains("boom"));
}

#[test]
fn format_delete_confirm_hint_uses_app_and_single_server() {
    let hint = format_delete_confirm_hint("bun-example", "prod-1");
    assert_eq!(hint, "This removes application bun-example from prod-1.");
}

#[test]
fn format_delete_confirm_hint_clarifies_single_deployment_scope() {
    let hint = format_delete_confirm_hint("bun-example", "prod-1");
    assert_eq!(hint, "This removes application bun-example from prod-1.");
}

#[test]
fn format_delete_confirm_prompt_uses_single_server_wording() {
    let prompt = format_delete_confirm_prompt("bun-example", "production", "prod-1");
    assert_eq!(
        prompt,
        "Please confirm you want to remove application bun-example from production on prod-1."
    );
}

#[test]
fn validate_confirmation_mode_requires_yes_for_non_interactive() {
    let err = validate_confirmation_mode(false, false).unwrap_err();
    assert!(err.contains("--yes"));
}

#[test]
fn validate_target_flags_for_mode_requires_env_and_server_non_interactive() {
    let err = validate_target_flags_for_mode(Some("production"), None, false).unwrap_err();
    assert!(err.contains("--env"));
    assert!(err.contains("--server"));
}
