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
            network,
            wireguard_endpoint,
            ..
        },
    )) = cli.command.unwrap()
    else {
        panic!("expected add")
    };
    assert!(network.is_none());
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
        server::ServerCommands::Remove { name },
    )) = cli.command.expect("command")
    else {
        panic!("expected Servers::Remove");
    };
    assert_eq!(name.as_deref(), Some("prod"));
}

#[test]
fn servers_remove_aliases_parse() {
    let cli = Cli::try_parse_from(["tako", "network", "server", "rm", "prod"]).unwrap();
    let Commands::Network(commands::network::NetworkCommands::Server(
        server::ServerCommands::Remove { name },
    )) = cli.command.expect("command")
    else {
        panic!("expected Servers::Remove");
    };
    assert_eq!(name.as_deref(), Some("prod"));

    let cli = Cli::try_parse_from(["tako", "network", "server", "delete", "prod"]).unwrap();
    let Commands::Network(commands::network::NetworkCommands::Server(
        server::ServerCommands::Remove { name },
    )) = cli.command.expect("command")
    else {
        panic!("expected Servers::Remove");
    };
    assert_eq!(name.as_deref(), Some("prod"));
}

#[test]
fn servers_remove_without_name_parses_for_selector() {
    let cli = Cli::try_parse_from(["tako", "network", "server", "remove"]).unwrap();
    let Commands::Network(commands::network::NetworkCommands::Server(
        server::ServerCommands::Remove { name },
    )) = cli.command.expect("command")
    else {
        panic!("expected Servers::Remove");
    };
    assert!(name.is_none());
}

#[test]
fn servers_help_shows_remove_as_primary_command() {
    let mut cmd = Cli::command();
    let servers = cmd
        .find_subcommand_mut("network")
        .unwrap()
        .find_subcommand_mut("server")
        .unwrap();
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
        server::ServerCommands::Reload { name, force },
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
        server::ServerCommands::Reload { name, force },
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
        server::ServerCommands::Upgrade { name, force },
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
        server::ServerCommands::Upgrade { name, force },
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
        server::ServerCommands::Upgrade { name, force },
    )) = cli.command.expect("command")
    else {
        panic!("expected Servers::Upgrade");
    };
    assert_eq!(name, Some("prod".to_string()));
    assert!(force);
}
