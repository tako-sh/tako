use super::*;

#[test]
fn deploy_without_env_parses_env_as_none() {
    let cli = Cli::try_parse_from(["tako", "deploy"]).unwrap();
    let Some(Commands::Deploy { env, yes, .. }) = cli.command else {
        panic!("expected Deploy");
    };
    assert!(env.is_none());
    assert!(!yes);
}

#[test]
fn deploy_with_env_parses_env_value() {
    let cli = Cli::try_parse_from(["tako", "deploy", "--env", "staging"]).unwrap();
    let Some(Commands::Deploy { env, .. }) = cli.command else {
        panic!("expected Deploy");
    };
    assert_eq!(env.as_deref(), Some("staging"));
}

#[test]
fn scale_parses_instances_and_env() {
    let cli = Cli::try_parse_from(["tako", "scale", "3", "--env", "staging"]).unwrap();
    let Some(Commands::Scale {
        instances,
        env,
        server,
        app,
    }) = cli.command
    else {
        panic!("expected Scale");
    };
    assert_eq!(instances, 3);
    assert_eq!(env.as_deref(), Some("staging"));
    assert!(server.is_none());
    assert!(app.is_none());
}

#[test]
fn scale_parses_server_env_and_app() {
    let cli = Cli::try_parse_from([
        "tako",
        "scale",
        "2",
        "--server",
        "la-1",
        "--env",
        "production",
        "--app",
        "my-app",
    ])
    .unwrap();
    let Some(Commands::Scale {
        instances,
        env,
        server,
        app,
    }) = cli.command
    else {
        panic!("expected Scale");
    };
    assert_eq!(instances, 2);
    assert_eq!(env.as_deref(), Some("production"));
    assert_eq!(server.as_deref(), Some("la-1"));
    assert_eq!(app.as_deref(), Some("my-app"));
}

#[test]
fn deploy_parses_yes_flag() {
    let cli = Cli::try_parse_from(["tako", "deploy", "--yes"]).unwrap();
    let Some(Commands::Deploy { yes, .. }) = cli.command else {
        panic!("expected Deploy");
    };
    assert!(yes);
}

#[test]
fn deploy_parses_yes_short_flag() {
    let cli = Cli::try_parse_from(["tako", "deploy", "-y"]).unwrap();
    let Some(Commands::Deploy { yes, .. }) = cli.command else {
        panic!("expected Deploy");
    };
    assert!(yes);
}

#[test]
fn releases_list_parses() {
    let cli = Cli::try_parse_from(["tako", "releases", "list"]).unwrap();
    let Some(Commands::Releases(releases::ReleaseCommands::List { env })) = cli.command else {
        panic!("expected Releases::List");
    };
    assert!(env.is_none());
}

#[test]
fn releases_list_parses_with_env() {
    let cli = Cli::try_parse_from(["tako", "releases", "list", "--env", "staging"]).unwrap();
    let Some(Commands::Releases(releases::ReleaseCommands::List { env })) = cli.command else {
        panic!("expected Releases::List");
    };
    assert_eq!(env.as_deref(), Some("staging"));
}

#[test]
fn releases_ls_alias_parses() {
    let cli = Cli::try_parse_from(["tako", "releases", "ls"]).unwrap();
    let Some(Commands::Releases(releases::ReleaseCommands::List { env })) = cli.command else {
        panic!("expected Releases::List");
    };
    assert!(env.is_none());
}

#[test]
fn releases_rollback_parses_release_id_and_yes_flag() {
    let cli = Cli::try_parse_from(["tako", "releases", "rollback", "abc1234", "--yes"]).unwrap();
    let Some(Commands::Releases(releases::ReleaseCommands::Rollback { release, env, yes })) =
        cli.command
    else {
        panic!("expected Releases::Rollback");
    };
    assert_eq!(release, "abc1234");
    assert!(env.is_none());
    assert!(yes);
}

#[test]
fn delete_without_env_parses_env_as_none() {
    let cli = Cli::try_parse_from(["tako", "delete"]).unwrap();
    let Some(Commands::Delete {
        env, server, yes, ..
    }) = cli.command
    else {
        panic!("expected Delete");
    };
    assert!(env.is_none());
    assert!(server.is_none());
    assert!(!yes);
}

#[test]
fn delete_aliases_parse() {
    let cli = Cli::try_parse_from(["tako", "rm", "--env", "staging"]).unwrap();
    let Some(Commands::Delete { env, .. }) = cli.command else {
        panic!("expected Delete");
    };
    assert_eq!(env.as_deref(), Some("staging"));

    let cli = Cli::try_parse_from(["tako", "remove", "--env", "staging"]).unwrap();
    let Some(Commands::Delete { env, .. }) = cli.command else {
        panic!("expected Delete");
    };
    assert_eq!(env.as_deref(), Some("staging"));
}

#[test]
fn delete_parses_server_flag() {
    let cli = Cli::try_parse_from(["tako", "delete", "--server", "lax"]).unwrap();
    let Some(Commands::Delete {
        env, server, yes, ..
    }) = cli.command
    else {
        panic!("expected Delete");
    };
    assert!(env.is_none());
    assert_eq!(server.as_deref(), Some("lax"));
    assert!(!yes);
}

#[test]
fn delete_parses_env_and_server_flags_together() {
    let cli =
        Cli::try_parse_from(["tako", "delete", "--env", "production", "--server", "lax"]).unwrap();
    let Some(Commands::Delete {
        env, server, yes, ..
    }) = cli.command
    else {
        panic!("expected Delete");
    };
    assert_eq!(env.as_deref(), Some("production"));
    assert_eq!(server.as_deref(), Some("lax"));
    assert!(!yes);
}

#[test]
fn upgrade_command_parses() {
    let cli = Cli::try_parse_from(["tako", "upgrade"]).unwrap();
    assert!(matches!(cli.command, Some(Commands::Upgrade)));
}

#[test]
fn deploy_rejects_removed_positional_dir_argument() {
    let result = Cli::try_parse_from(["tako", "deploy", "apps/web"]);
    match result {
        Ok(_) => panic!("expected parse failure"),
        Err(err) => assert!(
            err.to_string().contains("unexpected argument 'apps/web'"),
            "unexpected error: {err}"
        ),
    }
}

#[test]
fn delete_rejects_removed_positional_dir_argument() {
    let result = Cli::try_parse_from(["tako", "delete", "apps/web"]);
    match result {
        Ok(_) => panic!("expected parse failure"),
        Err(err) => assert!(
            err.to_string().contains("unexpected argument 'apps/web'"),
            "unexpected error: {err}"
        ),
    }
}
