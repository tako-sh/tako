use super::*;
use crate::commands::secret::SecretKeyCommands;
use crate::commands::storage::{StorageCommands, StorageProviderArg};
use clap::{CommandFactory, Parser};

#[test]
fn servers_add_defaults_to_tako_user() {
    let cli = Cli::try_parse_from(["tako", "servers", "add", "example.com"]).unwrap();
    let Commands::Servers(server::ServerCommands::Add { host, .. }) = cli.command.expect("command")
    else {
        panic!("expected Servers::Add");
    };
    assert_eq!(host.as_deref(), Some("example.com"));
}

#[test]
fn global_ssh_passphrase_parses_for_one_liners() {
    let cli = Cli::try_parse_from([
        "tako",
        "servers",
        "add",
        "example.com",
        "--ssh-passphrase",
        "testpass",
    ])
    .unwrap();

    assert_eq!(cli.ssh_passphrase.as_deref(), Some("testpass"));
}

#[test]
fn generate_parses_with_short_aliases() {
    for subcommand in ["generate", "gen", "g"] {
        let cli = Cli::try_parse_from(["tako", subcommand]).unwrap();
        let Commands::Generate = cli.command.expect("command") else {
            panic!("expected Commands::Generate for {subcommand}");
        };
    }
}

#[test]
fn servers_add_without_host_parses_for_wizard() {
    let cli = Cli::try_parse_from(["tako", "servers", "add"]).unwrap();
    let Commands::Servers(server::ServerCommands::Add { host, .. }) = cli.command.expect("command")
    else {
        panic!("expected Servers::Add");
    };
    assert!(host.is_none());
}

#[test]
fn servers_add_parses_optional_description() {
    let cli = Cli::try_parse_from([
        "tako",
        "servers",
        "add",
        "example.com",
        "--description",
        "Edge node",
    ])
    .unwrap();
    let Commands::Servers(server::ServerCommands::Add { description, .. }) =
        cli.command.expect("command")
    else {
        panic!("expected Servers::Add");
    };
    assert_eq!(description.as_deref(), Some("Edge node"));
}

#[test]
fn servers_add_accepts_admin_user_host_shorthand() {
    let cli = Cli::try_parse_from(["tako", "servers", "add", "ubuntu@example.com"]).unwrap();
    let Commands::Servers(server::ServerCommands::Add { host, .. }) = cli.command.expect("command")
    else {
        panic!("expected Servers::Add");
    };
    assert_eq!(host.as_deref(), Some("ubuntu@example.com"));
}

#[test]
fn servers_add_parses_install_admin_user() {
    let cli = Cli::try_parse_from([
        "tako",
        "servers",
        "add",
        "example.com",
        "--name",
        "prod",
        "--install",
        "--admin-user",
        "ubuntu",
    ])
    .unwrap();
    let Commands::Servers(server::ServerCommands::Add {
        install,
        admin_user,
        ..
    }) = cli.command.expect("command")
    else {
        panic!("expected Servers::Add");
    };
    assert!(install);
    assert_eq!(admin_user.as_deref(), Some("ubuntu"));
}

#[test]
fn servers_add_install_conflicts_with_no_test() {
    let res = Cli::try_parse_from([
        "tako",
        "servers",
        "add",
        "example.com",
        "--name",
        "prod",
        "--install",
        "--no-test",
    ]);
    assert!(res.is_err());
}

#[test]
fn servers_add_rejects_user_flag() {
    let res = Cli::try_parse_from(["tako", "servers", "add", "example.com", "--user", "root"]);
    match res {
        Ok(_) => panic!("expected parse failure"),
        Err(err) => assert!(
            err.to_string().contains("unexpected argument '--user'"),
            "unexpected error: {err}"
        ),
    }
}

#[test]
fn secrets_key_export_parses_with_env() {
    let cli =
        Cli::try_parse_from(["tako", "secrets", "key", "export", "--env", "production"]).unwrap();

    let Some(Commands::Secrets(secret::SecretCommands::Key(SecretKeyCommands::Export { env }))) =
        cli.command
    else {
        panic!("expected Secrets::Key::Export");
    };

    assert_eq!(env.as_deref(), Some("production"));
}

#[test]
fn secrets_key_import_parses() {
    let cli = Cli::try_parse_from(["tako", "secrets", "key", "import"]).unwrap();

    let Some(Commands::Secrets(secret::SecretCommands::Key(SecretKeyCommands::Import {
        passphrase,
        env,
    }))) = cli.command
    else {
        panic!("expected Secrets::Key::Import");
    };

    assert!(!passphrase);
    assert_eq!(env, None);
}

#[test]
fn secrets_key_import_parses_with_env() {
    let cli =
        Cli::try_parse_from(["tako", "secrets", "key", "import", "--env", "production"]).unwrap();

    let Some(Commands::Secrets(secret::SecretCommands::Key(SecretKeyCommands::Import {
        passphrase,
        env,
    }))) = cli.command
    else {
        panic!("expected Secrets::Key::Import");
    };

    assert!(!passphrase);
    assert_eq!(env.as_deref(), Some("production"));
}

#[test]
fn secrets_key_import_passphrase_parses_with_env() {
    let cli = Cli::try_parse_from([
        "tako",
        "secrets",
        "key",
        "import",
        "--passphrase",
        "--env",
        "production",
    ])
    .unwrap();

    let Some(Commands::Secrets(secret::SecretCommands::Key(SecretKeyCommands::Import {
        passphrase,
        env,
    }))) = cli.command
    else {
        panic!("expected Secrets::Key::Import");
    };

    assert!(passphrase);
    assert_eq!(env.as_deref(), Some("production"));
}

#[test]
fn storages_add_parses_required_binding_options() {
    let cli = Cli::try_parse_from([
        "tako",
        "storages",
        "add",
        "uploads",
        "--env",
        "production",
        "--provider",
        "s3",
        "--resource",
        "prod_uploads",
        "--bucket",
        "app-uploads",
        "--endpoint",
        "https://abc.r2.cloudflarestorage.com",
        "--region",
        "auto",
        "--access-key-id",
        "key-id",
        "--secret-access-key",
        "secret",
        "--force-path-style",
        "--public-base-url",
        "https://cdn.example.com",
    ])
    .unwrap();

    let Some(Commands::Storages(StorageCommands::Add {
        name,
        env,
        resource,
        provider,
        bucket,
        endpoint,
        region,
        access_key_id,
        secret_access_key,
        force_path_style,
        public_base_url,
    })) = cli.command
    else {
        panic!("expected Storage::Add");
    };

    assert_eq!(name, "uploads");
    assert_eq!(env, "production");
    assert_eq!(resource.as_deref(), Some("prod_uploads"));
    assert!(matches!(provider, StorageProviderArg::S3));
    assert_eq!(bucket.as_deref(), Some("app-uploads"));
    assert_eq!(
        endpoint.as_deref(),
        Some("https://abc.r2.cloudflarestorage.com")
    );
    assert_eq!(region.as_deref(), Some("auto"));
    assert_eq!(access_key_id.as_deref(), Some("key-id"));
    assert_eq!(secret_access_key.as_deref(), Some("secret"));
    assert!(force_path_style);
    assert_eq!(public_base_url.as_deref(), Some("https://cdn.example.com"));
}

#[test]
fn servers_remove_parses() {
    let cli = Cli::try_parse_from(["tako", "servers", "remove", "prod"]).unwrap();
    let Commands::Servers(server::ServerCommands::Remove { name }) = cli.command.expect("command")
    else {
        panic!("expected Servers::Remove");
    };
    assert_eq!(name.as_deref(), Some("prod"));
}

#[test]
fn servers_remove_aliases_parse() {
    let cli = Cli::try_parse_from(["tako", "servers", "rm", "prod"]).unwrap();
    let Commands::Servers(server::ServerCommands::Remove { name }) = cli.command.expect("command")
    else {
        panic!("expected Servers::Remove");
    };
    assert_eq!(name.as_deref(), Some("prod"));

    let cli = Cli::try_parse_from(["tako", "servers", "delete", "prod"]).unwrap();
    let Commands::Servers(server::ServerCommands::Remove { name }) = cli.command.expect("command")
    else {
        panic!("expected Servers::Remove");
    };
    assert_eq!(name.as_deref(), Some("prod"));
}

#[test]
fn servers_remove_without_name_parses_for_selector() {
    let cli = Cli::try_parse_from(["tako", "servers", "remove"]).unwrap();
    let Commands::Servers(server::ServerCommands::Remove { name }) = cli.command.expect("command")
    else {
        panic!("expected Servers::Remove");
    };
    assert!(name.is_none());
}

#[test]
fn servers_help_shows_remove_as_primary_command() {
    let mut cmd = Cli::command();
    let servers = cmd.find_subcommand_mut("servers").unwrap();
    let mut help = Vec::new();
    servers.write_help(&mut help).unwrap();
    let help = String::from_utf8(help).unwrap();
    let remove_line = help
        .lines()
        .find(|line| line.contains("Remove a server"))
        .expect("expected remove command in help");

    assert!(
        remove_line.trim_start().starts_with("remove "),
        "expected remove to be primary command: {remove_line}"
    );
    assert!(
        remove_line.contains("rm") && remove_line.contains("delete"),
        "expected rm and delete aliases: {remove_line}"
    );
}

#[test]
fn servers_list_parses() {
    let cli = Cli::try_parse_from(["tako", "servers", "list"]).unwrap();
    let Commands::Servers(server::ServerCommands::List) = cli.command.expect("command") else {
        panic!("expected Servers::List");
    };
}

#[test]
fn servers_ls_alias_parses() {
    let cli = Cli::try_parse_from(["tako", "servers", "ls"]).unwrap();
    let Commands::Servers(server::ServerCommands::List) = cli.command.expect("command") else {
        panic!("expected Servers::List");
    };
}

#[test]
fn servers_status_without_name_parses() {
    let cli = Cli::try_parse_from(["tako", "servers", "status"]).unwrap();
    let Commands::Servers(server::ServerCommands::Status) = cli.command.expect("command") else {
        panic!("expected Servers::Status");
    };
}

#[test]
fn servers_status_with_name_is_rejected() {
    let res = Cli::try_parse_from(["tako", "servers", "status", "prod"]);
    match res {
        Ok(_) => panic!("expected parse failure"),
        Err(err) => assert!(
            err.to_string().contains("unexpected argument 'prod'"),
            "unexpected error: {err}"
        ),
    }
}

#[test]
fn servers_configure_parses_name() {
    let cli = Cli::try_parse_from(["tako", "servers", "configure", "prod"]).unwrap();
    let Commands::Servers(server::ServerCommands::Configure { name }) =
        cli.command.expect("command")
    else {
        panic!("expected Servers::Configure");
    };
    assert_eq!(name.as_deref(), Some("prod"));
}

#[test]
fn servers_configure_without_name_parses_for_selector() {
    let cli = Cli::try_parse_from(["tako", "servers", "configure"]).unwrap();
    let Commands::Servers(server::ServerCommands::Configure { name }) =
        cli.command.expect("command")
    else {
        panic!("expected Servers::Configure");
    };
    assert!(name.is_none());
}

#[test]
fn servers_reload_parses_without_force() {
    let cli = Cli::try_parse_from(["tako", "servers", "reload", "prod"]).unwrap();
    let Commands::Servers(server::ServerCommands::Reload { name, force }) =
        cli.command.expect("command")
    else {
        panic!("expected Servers::Reload");
    };
    assert_eq!(name, "prod");
    assert!(!force);
}

#[test]
fn servers_reload_parses_with_force() {
    let cli = Cli::try_parse_from(["tako", "servers", "reload", "prod", "--force"]).unwrap();
    let Commands::Servers(server::ServerCommands::Reload { name, force }) =
        cli.command.expect("command")
    else {
        panic!("expected Servers::Reload");
    };
    assert_eq!(name, "prod");
    assert!(force);
}

#[test]
fn servers_upgrade_parses_without_name() {
    let cli = Cli::try_parse_from(["tako", "servers", "upgrade"]).unwrap();
    let Commands::Servers(server::ServerCommands::Upgrade { name }) = cli.command.expect("command")
    else {
        panic!("expected Servers::Upgrade");
    };
    assert_eq!(name, None);
}

#[test]
fn servers_upgrade_parses_with_name() {
    let cli = Cli::try_parse_from(["tako", "servers", "upgrade", "prod"]).unwrap();
    let Commands::Servers(server::ServerCommands::Upgrade { name }) = cli.command.expect("command")
    else {
        panic!("expected Servers::Upgrade");
    };
    assert_eq!(name, Some("prod".to_string()));
}

#[test]
fn top_level_status_command_is_not_available() {
    let res = Cli::try_parse_from(["tako", "status"]);
    match res {
        Ok(_) => panic!("expected parse failure"),
        Err(err) => assert!(
            err.to_string().contains("unrecognized subcommand 'status'"),
            "unexpected error: {err}"
        ),
    }
}

#[test]
fn secrets_remove_aliases_parse() {
    let cli = Cli::try_parse_from(["tako", "secrets", "remove", "API_KEY"]).unwrap();
    let Some(Commands::Secrets(secret::SecretCommands::Rm { name, env, .. })) = cli.command else {
        panic!("expected Secrets::Rm");
    };
    assert_eq!(name, "API_KEY");
    assert!(env.is_none());

    let cli = Cli::try_parse_from(["tako", "secrets", "delete", "API_KEY"]).unwrap();
    let Some(Commands::Secrets(secret::SecretCommands::Rm { name, env, .. })) = cli.command else {
        panic!("expected Secrets::Rm");
    };
    assert_eq!(name, "API_KEY");
    assert!(env.is_none());
}

#[test]
fn secrets_list_parses() {
    let cli = Cli::try_parse_from(["tako", "secrets", "list"]).unwrap();
    let Some(Commands::Secrets(secret::SecretCommands::List)) = cli.command else {
        panic!("expected Secrets::List");
    };
}

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
fn uninstall_command_parses() {
    let cli = Cli::try_parse_from(["tako", "uninstall"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(Commands::Uninstall { yes: false })
    ));
}

#[test]
fn dev_rejects_port_flag() {
    let res = Cli::try_parse_from(["tako", "dev", "--port", "47831"]);
    match res {
        Ok(_) => panic!("expected parse failure"),
        Err(err) => assert!(
            err.to_string().contains("unexpected argument '--port'"),
            "unexpected error: {err}"
        ),
    }
}

#[test]
fn dev_default_parses_without_subcommand() {
    let cli = Cli::try_parse_from(["tako", "dev"]).unwrap();
    let Commands::Dev { command, args } = cli.command.expect("command") else {
        panic!("expected Dev");
    };
    assert!(command.is_none());
    assert!(args.variant.is_none());
}

#[test]
fn dev_parses_variant_flag() {
    let cli = Cli::try_parse_from(["tako", "dev", "--variant", "foo"]).unwrap();
    let Commands::Dev { command, args } = cli.command.expect("command") else {
        panic!("expected Dev");
    };
    assert!(command.is_none());
    assert_eq!(args.variant.as_deref(), Some("foo"));
}

#[test]
fn dev_parses_var_alias() {
    let cli = Cli::try_parse_from(["tako", "dev", "--var", "foo"]).unwrap();
    let Commands::Dev { command, args } = cli.command.expect("command") else {
        panic!("expected Dev");
    };
    assert!(command.is_none());
    assert_eq!(args.variant.as_deref(), Some("foo"));
}

#[test]
fn dev_stop_parses() {
    let cli = Cli::try_parse_from(["tako", "dev", "stop"]).unwrap();
    let Commands::Dev { command, .. } = cli.command.expect("command") else {
        panic!("expected Dev");
    };
    match command {
        Some(DevSubcommands::Stop { name, all }) => {
            assert!(name.is_none());
            assert!(!all);
        }
        other => panic!("expected Stop, got {other:?}"),
    }
}

#[test]
fn dev_stop_with_name_parses() {
    let cli = Cli::try_parse_from(["tako", "dev", "stop", "my-app"]).unwrap();
    let Commands::Dev { command, .. } = cli.command.expect("command") else {
        panic!("expected Dev");
    };
    match command {
        Some(DevSubcommands::Stop { name, all }) => {
            assert_eq!(name.as_deref(), Some("my-app"));
            assert!(!all);
        }
        other => panic!("expected Stop, got {other:?}"),
    }
}

#[test]
fn dev_stop_all_parses() {
    let cli = Cli::try_parse_from(["tako", "dev", "stop", "--all"]).unwrap();
    let Commands::Dev { command, .. } = cli.command.expect("command") else {
        panic!("expected Dev");
    };
    match command {
        Some(DevSubcommands::Stop { name, all }) => {
            assert!(name.is_none());
            assert!(all);
        }
        other => panic!("expected Stop, got {other:?}"),
    }
}

#[test]
fn dev_list_parses() {
    let cli = Cli::try_parse_from(["tako", "dev", "list"]).unwrap();
    let Commands::Dev { command, .. } = cli.command.expect("command") else {
        panic!("expected Dev");
    };
    assert!(matches!(command, Some(DevSubcommands::List)));
}

#[test]
fn dev_ls_alias_parses() {
    let cli = Cli::try_parse_from(["tako", "dev", "ls"]).unwrap();
    let Commands::Dev { command, .. } = cli.command.expect("command") else {
        panic!("expected Dev");
    };
    assert!(matches!(command, Some(DevSubcommands::List)));
}

#[test]
fn init_parses_without_runtime_flag() {
    let cli = Cli::try_parse_from(["tako", "init"]).unwrap();
    assert!(matches!(cli.command, Some(Commands::Init)));
}

#[test]
fn display_version_without_build_sha_uses_base_version() {
    let version = format_display_version("1.2.3", None);
    assert_eq!(version, "1.2.3");
}

#[test]
fn display_version_with_full_build_sha_uses_short_hash() {
    let version = format_display_version("1.2.3", Some("0123456789abcdef"));
    assert_eq!(version, "1.2.3-0123456");
}

#[test]
fn display_version_with_short_build_sha_keeps_full_value() {
    let version = format_display_version("1.2.3", Some("abc"));
    assert_eq!(version, "1.2.3-abc");
}

#[test]
fn display_version_with_blank_build_sha_uses_base_version() {
    let version = format_display_version("1.2.3", Some("   "));
    assert_eq!(version, "1.2.3");
}

#[test]
fn version_subcommand_parses() {
    let cli = Cli::try_parse_from(["tako", "version"]).unwrap();
    assert!(matches!(cli.command, Some(Commands::Version)));
}

#[test]
fn ci_flag_parses_globally() {
    let cli = Cli::try_parse_from(["tako", "--ci", "deploy"]).unwrap();
    assert!(cli.ci);
}

#[test]
fn config_flag_parses_globally_before_subcommand() {
    let cli = Cli::try_parse_from(["tako", "--config", "configs/preview", "deploy"]).unwrap();
    assert_eq!(
        cli.config.as_deref(),
        Some(std::path::Path::new("configs/preview"))
    );
}

#[test]
fn config_flag_parses_globally_after_subcommand() {
    let cli = Cli::try_parse_from(["tako", "deploy", "-c", "configs/preview"]).unwrap();
    assert_eq!(
        cli.config.as_deref(),
        Some(std::path::Path::new("configs/preview"))
    );
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
fn dev_rejects_removed_positional_dir_argument() {
    let result = Cli::try_parse_from(["tako", "dev", "apps/web"]);
    match result {
        Ok(_) => panic!("expected parse failure"),
        Err(err) => assert!(
            err.to_string()
                .contains("unrecognized subcommand 'apps/web'")
                || err.to_string().contains("unexpected argument 'apps/web'"),
            "unexpected error: {err}"
        ),
    }
}

#[test]
fn init_rejects_removed_positional_dir_argument() {
    let result = Cli::try_parse_from(["tako", "init", "apps/web"]);
    match result {
        Ok(_) => panic!("expected parse failure"),
        Err(err) => assert!(
            err.to_string().contains("unexpected argument 'apps/web'"),
            "unexpected error: {err}"
        ),
    }
}

#[test]
fn logs_rejects_removed_positional_dir_argument() {
    let result = Cli::try_parse_from(["tako", "logs", "apps/web"]);
    match result {
        Ok(_) => panic!("expected parse failure"),
        Err(err) => assert!(
            err.to_string().contains("unexpected argument 'apps/web'"),
            "unexpected error: {err}"
        ),
    }
}

#[test]
fn logs_json_flag_parses() {
    let cli = Cli::try_parse_from(["tako", "logs", "--json"]).unwrap();
    let Some(Commands::Logs { json, .. }) = cli.command else {
        panic!("expected Logs");
    };
    assert!(json);
}

#[test]
fn logs_tail_json_flag_parses() {
    let cli = Cli::try_parse_from(["tako", "logs", "--tail", "--json"]).unwrap();
    let Some(Commands::Logs { json, tail, .. }) = cli.command else {
        panic!("expected Logs");
    };
    assert!(json);
    assert!(tail);
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

#[test]
fn ci_and_verbose_flags_combine() {
    let cli = Cli::try_parse_from(["tako", "--ci", "-v", "deploy"]).unwrap();
    assert!(cli.ci);
    assert!(cli.verbose);
}

#[test]
fn ci_flag_after_subcommand_parses() {
    let cli = Cli::try_parse_from(["tako", "deploy", "--ci"]).unwrap();
    assert!(cli.ci);
}

#[test]
fn dry_run_flag_parses_globally() {
    let cli = Cli::try_parse_from(["tako", "--dry-run", "deploy"]).unwrap();
    assert!(cli.dry_run);
}

#[test]
fn dry_run_flag_after_subcommand() {
    let cli = Cli::try_parse_from(["tako", "deploy", "--dry-run"]).unwrap();
    assert!(cli.dry_run);
}

#[test]
fn dry_run_combines_with_ci_and_verbose() {
    let cli = Cli::try_parse_from(["tako", "--dry-run", "--ci", "-v", "deploy"]).unwrap();
    assert!(cli.dry_run);
    assert!(cli.ci);
    assert!(cli.verbose);
}
