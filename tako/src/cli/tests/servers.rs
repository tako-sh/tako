use super::*;

#[test]
fn servers_add_rejects_removed_provider_flags() {
    for value in ["tailscale", "netbird"] {
        assert!(
            Cli::try_parse_from([
                "tako",
                "network",
                "server",
                "add",
                "edge.internal",
                "--vpn",
                value
            ])
            .is_err()
        );
    }
}

#[test]
fn servers_add_accepts_native_endpoint_with_automatic_network_selection() {
    let cli = Cli::try_parse_from([
        "tako",
        "network",
        "server",
        "add",
        "admin@edge.internal",
        "--wireguard-endpoint",
        "192.0.2.1:51820",
    ])
    .unwrap();
    let Commands::Network(commands::network::NetworkCommands::Server(
        server::ServerCommands::Add {
            wireguard_endpoint, ..
        },
    )) = cli.command.unwrap()
    else {
        panic!("expected add")
    };
    assert!(cli.network_flag.is_none());
    assert_eq!(wireguard_endpoint.unwrap().port(), 51820);
}

#[test]
fn servers_add_defaults_to_tako_user() {
    let cli = Cli::try_parse_from(["tako", "network", "server", "add", "example.com"]).unwrap();
    let Commands::Network(commands::network::NetworkCommands::Server(
        server::ServerCommands::Add { host, .. },
    )) = cli.command.expect("command")
    else {
        panic!("expected Servers::Add");
    };
    assert_eq!(host.as_deref(), Some("example.com"));
}

#[test]
fn servers_add_without_host_parses_for_wizard() {
    let cli = Cli::try_parse_from(["tako", "network", "server", "add"]).unwrap();
    let Commands::Network(commands::network::NetworkCommands::Server(
        server::ServerCommands::Add { host, .. },
    )) = cli.command.expect("command")
    else {
        panic!("expected Servers::Add");
    };
    assert!(host.is_none());
}

#[test]
fn servers_add_accepts_admin_user_host_shorthand() {
    let cli =
        Cli::try_parse_from(["tako", "network", "server", "add", "ubuntu@example.com"]).unwrap();
    let Commands::Network(commands::network::NetworkCommands::Server(
        server::ServerCommands::Add { host, .. },
    )) = cli.command.expect("command")
    else {
        panic!("expected Servers::Add");
    };
    assert_eq!(host.as_deref(), Some("ubuntu@example.com"));
}

#[test]
fn servers_add_rejects_admin_user_flag() {
    let result = Cli::try_parse_from([
        "tako",
        "network",
        "server",
        "add",
        "example.com",
        "--name",
        "prod",
        "--install",
        "--admin-user",
        "ubuntu",
    ]);

    assert!(result.is_err());
}

#[test]
fn servers_add_install_conflicts_with_no_test() {
    let res = Cli::try_parse_from([
        "tako",
        "network",
        "server",
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
    let res = Cli::try_parse_from([
        "tako",
        "network",
        "server",
        "add",
        "example.com",
        "--user",
        "root",
    ]);
    match res {
        Ok(_) => panic!("expected parse failure"),
        Err(err) => assert!(
            err.to_string().contains("unexpected argument '--user'"),
            "unexpected error: {err}"
        ),
    }
}

#[test]
fn servers_remove_parses() {
    let cli = Cli::try_parse_from(["tako", "network", "server", "remove", "prod"]).unwrap();
    let Commands::Network(commands::network::NetworkCommands::Server(
        server::ServerCommands::Remove {
            name,
            uninstall,
            yes,
            ..
        },
    )) = cli.command.expect("command")
    else {
        panic!("expected Servers::Remove");
    };
    assert_eq!(name.as_deref(), Some("prod"));
    assert!(!uninstall && !yes);

    let cli = Cli::try_parse_from([
        "tako",
        "network",
        "server",
        "rm",
        "prod",
        "--uninstall",
        "-y",
    ])
    .unwrap();
    let Commands::Network(commands::network::NetworkCommands::Server(
        server::ServerCommands::Remove {
            name,
            uninstall,
            yes,
            ..
        },
    )) = cli.command.expect("command")
    else {
        panic!("expected Servers::Remove");
    };
    assert_eq!(name.as_deref(), Some("prod"));
    assert!(uninstall && yes);
}

#[test]
fn servers_remove_without_name_parses_for_selector() {
    let cli = Cli::try_parse_from(["tako", "network", "server", "remove"]).unwrap();
    let Commands::Network(commands::network::NetworkCommands::Server(
        server::ServerCommands::Remove { name, .. },
    )) = cli.command.expect("command")
    else {
        panic!("expected Servers::Remove");
    };
    assert!(name.is_none());
}

#[test]
fn servers_revoke_is_membership_only_and_uninstall_is_gone() {
    let cli = Cli::try_parse_from(["tako", "network", "server", "revoke", "prod"]).unwrap();
    let Commands::Network(commands::network::NetworkCommands::Server(
        server::ServerCommands::Revoke { name, .. },
    )) = cli.command.expect("command")
    else {
        panic!("expected Servers::Revoke");
    };
    assert_eq!(name.as_deref(), Some("prod"));

    for args in [
        ["tako", "network", "server", "delete", "prod"],
        ["tako", "network", "server", "uninstall", "prod"],
    ] {
        assert!(Cli::try_parse_from(args).is_err(), "{args:?}");
    }
}

#[test]
fn servers_help_lists_remove_with_its_alias_and_revoke_without_any() {
    let mut cmd = Cli::command();
    let servers = cmd
        .find_subcommand_mut("network")
        .unwrap()
        .find_subcommand_mut("server")
        .unwrap();
    let mut help = Vec::new();
    servers.write_help(&mut help).unwrap();
    let help = String::from_utf8(help).unwrap();
    let line = |command: &str| {
        help.lines()
            .find(|line| line.trim_start().starts_with(&format!("{command} ")))
            .unwrap_or_else(|| panic!("expected {command} in help:\n{help}"))
            .to_string()
    };

    assert!(line("remove").contains("[alias: rm]"), "{}", line("remove"));
    assert!(!line("revoke").contains("alias"), "{}", line("revoke"));
}

#[test]
fn servers_list_parses() {
    let cli = Cli::try_parse_from(["tako", "network", "server", "list"]).unwrap();
    let Commands::Network(commands::network::NetworkCommands::Server(server::ServerCommands::List)) =
        cli.command.expect("command")
    else {
        panic!("expected Servers::List");
    };
}

#[test]
fn servers_ls_alias_parses() {
    let cli = Cli::try_parse_from(["tako", "network", "server", "ls"]).unwrap();
    let Commands::Network(commands::network::NetworkCommands::Server(server::ServerCommands::List)) =
        cli.command.expect("command")
    else {
        panic!("expected Servers::List");
    };
}

#[test]
fn servers_reload_parses_without_force() {
    let cli = Cli::try_parse_from(["tako", "network", "server", "reload", "prod"]).unwrap();
    let Commands::Network(commands::network::NetworkCommands::Server(
        server::ServerCommands::Reload { name, force, .. },
    )) = cli.command.expect("command")
    else {
        panic!("expected Servers::Reload");
    };
    assert_eq!(name, "prod");
    assert!(!force);
}

#[test]
fn servers_reload_parses_with_force() {
    let cli =
        Cli::try_parse_from(["tako", "network", "server", "reload", "prod", "--force"]).unwrap();
    let Commands::Network(commands::network::NetworkCommands::Server(
        server::ServerCommands::Reload { name, force, .. },
    )) = cli.command.expect("command")
    else {
        panic!("expected Servers::Reload");
    };
    assert_eq!(name, "prod");
    assert!(force);
}

#[test]
fn servers_upgrade_parses_without_name() {
    let cli = Cli::try_parse_from(["tako", "network", "server", "upgrade"]).unwrap();
    let Commands::Network(commands::network::NetworkCommands::Server(
        server::ServerCommands::Upgrade { name, force, .. },
    )) = cli.command.expect("command")
    else {
        panic!("expected Servers::Upgrade");
    };
    assert_eq!(name, None);
    assert!(!force);
}

#[test]
fn servers_upgrade_parses_with_name() {
    let cli = Cli::try_parse_from(["tako", "network", "server", "upgrade", "prod"]).unwrap();
    let Commands::Network(commands::network::NetworkCommands::Server(
        server::ServerCommands::Upgrade { name, force, .. },
    )) = cli.command.expect("command")
    else {
        panic!("expected Servers::Upgrade");
    };
    assert_eq!(name, Some("prod".to_string()));
    assert!(!force);
}

#[test]
fn servers_upgrade_parses_force() {
    let cli =
        Cli::try_parse_from(["tako", "network", "server", "upgrade", "prod", "--force"]).unwrap();
    let Commands::Network(commands::network::NetworkCommands::Server(
        server::ServerCommands::Upgrade { name, force, .. },
    )) = cli.command.expect("command")
    else {
        panic!("expected Servers::Upgrade");
    };
    assert_eq!(name, Some("prod".to_string()));
    assert!(force);
}

#[test]
fn every_command_accepts_a_network() {
    for args in [
        vec![
            "network",
            "server",
            "revoke",
            "edge",
            "--network",
            "staging",
        ],
        vec![
            "network",
            "server",
            "remove",
            "edge",
            "--network",
            "staging",
        ],
        vec![
            "network",
            "server",
            "reload",
            "edge",
            "--network",
            "staging",
        ],
        vec!["network", "server", "upgrade", "--network", "staging"],
        vec!["network", "client", "add", "ci", "--network", "staging"],
        vec!["--network", "staging", "deploy", "--env", "production"],
        vec![
            "secret",
            "set",
            "KEY",
            "--env",
            "production",
            "--network",
            "staging",
        ],
        vec!["ssh", "edge", "--network", "staging"],
        vec!["db", "list", "--network", "staging"],
    ] {
        let cli = Cli::try_parse_from(std::iter::once("tako").chain(args.iter().copied()))
            .unwrap_or_else(|error| panic!("{args:?}: {error}"));
        assert_eq!(cli.network_flag.as_deref(), Some("staging"), "{args:?}");
    }
}

#[test]
fn network_rename_parses_and_use_is_gone() {
    let cli = Cli::try_parse_from(["tako", "network", "rename", "work", "acme"]).unwrap();
    let Commands::Network(commands::network::NetworkCommands::Rename { network, alias }) =
        cli.command.expect("command")
    else {
        panic!("expected Network::Rename");
    };
    assert_eq!((network.as_str(), alias.as_str()), ("work", "acme"));
    assert!(Cli::try_parse_from(["tako", "network", "use", "staging"]).is_err());
}
