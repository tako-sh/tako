use super::*;

#[test]
fn sanitize_name_segment_lowercases() {
    assert_eq!(sanitize_name_segment("MyApp"), "myapp");
}

#[test]
fn sanitize_name_segment_replaces_special_chars() {
    assert_eq!(sanitize_name_segment("foo_bar.baz"), "foo-bar-baz");
}

#[test]
fn sanitize_name_segment_collapses_consecutive_separators() {
    assert_eq!(sanitize_name_segment("a__b--c..d"), "a-b-c-d");
}

#[test]
fn sanitize_name_segment_strips_leading_trailing_hyphens() {
    assert_eq!(sanitize_name_segment("-abc-"), "abc");
}

#[test]
fn sanitize_name_segment_drops_non_ascii() {
    assert_eq!(sanitize_name_segment("café"), "caf");
}

#[test]
fn short_path_hash_is_deterministic() {
    let a = short_path_hash("/home/user/project");
    let b = short_path_hash("/home/user/project");
    assert_eq!(a, b);
}

#[test]
fn short_path_hash_differs_for_different_paths() {
    let a = short_path_hash("/home/user/project-a");
    let b = short_path_hash("/home/user/project-b");
    assert_ne!(a, b);
}

#[test]
fn short_path_hash_is_4_hex_chars() {
    let h = short_path_hash("/some/path");
    assert_eq!(h.len(), 4);
    assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn no_existing_apps_returns_candidate_unchanged() {
    let result = disambiguate_app_name("my-app", "/proj", &[]);
    assert_eq!(result, "my-app");
}

#[test]
fn same_project_dir_is_not_a_conflict() {
    let existing = vec![("my-app".into(), "/proj/tako.toml".into())];
    let result = disambiguate_app_name("my-app", "/proj/tako.toml", &existing);
    assert_eq!(result, "my-app");
}

#[test]
fn different_name_is_not_a_conflict() {
    let existing = vec![("other-app".into(), "/other/tako.toml".into())];
    let result = disambiguate_app_name("my-app", "/proj/tako.toml", &existing);
    assert_eq!(result, "my-app");
}

#[test]
fn conflict_appends_dir_leaf_name() {
    let existing = vec![("my-app".into(), "/home/user/proj-a/tako.toml".into())];
    let result = disambiguate_app_name("my-app", "/home/user/proj-b/tako.toml", &existing);
    assert_eq!(result, "my-app-proj-b");
}

#[test]
fn conflict_from_variant_matching_existing_app_name() {
    let existing = vec![("app-foo".into(), "/proj/app-foo/tako.toml".into())];
    let result = disambiguate_app_name("app-foo", "/proj/app/tako.toml", &existing);
    assert_eq!(result, "app-foo-app");
}

#[test]
fn conflict_from_non_variant_matching_variant_composite() {
    let existing = vec![("app-foo".into(), "/proj/app/tako.toml".into())];
    let result = disambiguate_app_name("app-foo", "/proj/app-foo/tako.toml", &existing);
    assert_eq!(result, "app-foo-app-foo");
}

#[test]
fn double_conflict_falls_back_to_hash() {
    let existing = vec![
        ("my-app".into(), "/workspace/a/tako.toml".into()),
        ("my-app-b".into(), "/workspace/b/tako.toml".into()),
    ];
    let result = disambiguate_app_name("my-app", "/workspace/c/b/tako.toml", &existing);
    let hash = short_path_hash("/workspace/c/b/tako.toml");
    assert_eq!(result, format!("my-app-{hash}"));
}

#[test]
fn workspace_apps_get_folder_suffix() {
    let existing = vec![("api".into(), "/repo/packages/billing/tako.toml".into())];
    let result = disambiguate_app_name("api", "/repo/packages/payments/tako.toml", &existing);
    assert_eq!(result, "api-payments");
}

#[test]
fn two_checkouts_of_same_repo_get_folder_suffix() {
    let existing = vec![("my-app".into(), "/home/user/my-app-main/tako.toml".into())];
    let result = disambiguate_app_name("my-app", "/home/user/my-app-feature/tako.toml", &existing);
    assert_eq!(result, "my-app-my-app-feature");
}

#[test]
fn no_conflict_among_many_registered_apps() {
    let existing = vec![
        ("alpha".into(), "/a/tako.toml".into()),
        ("beta".into(), "/b/tako.toml".into()),
        ("gamma".into(), "/c/tako.toml".into()),
    ];
    let result = disambiguate_app_name("delta", "/d/tako.toml", &existing);
    assert_eq!(result, "delta");
}

#[test]
fn conflict_detected_among_many_registered_apps() {
    let existing = vec![
        ("alpha".into(), "/a/tako.toml".into()),
        ("beta".into(), "/b/tako.toml".into()),
        ("gamma".into(), "/c/tako.toml".into()),
    ];
    let result = disambiguate_app_name("beta", "/other/tako.toml", &existing);
    assert_eq!(result, "beta-other");
}

#[test]
fn root_path_project_uses_hash_fallback() {
    let existing = vec![("app".into(), "/other/tako.toml".into())];
    let result = disambiguate_app_name("app", "/tako.toml", &existing);
    let hash = short_path_hash("/tako.toml");
    assert_eq!(result, format!("app-{hash}"));
}

#[test]
fn re_registration_after_disambiguation_is_idempotent() {
    let existing = vec![
        ("api".into(), "/repo/packages/billing/tako.toml".into()),
        (
            "api-payments".into(),
            "/repo/packages/payments/tako.toml".into(),
        ),
    ];
    let result = disambiguate_app_name("api", "/repo/packages/payments/tako.toml", &existing);
    assert_eq!(result, "api-payments");
}
