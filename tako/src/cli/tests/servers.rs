use super::*;

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
fn servers_status_is_rejected() {
    let res = Cli::try_parse_from(["tako", "servers", "status"]);
    match res {
        Ok(_) => panic!("expected parse failure"),
        Err(err) => assert!(
            err.to_string().contains("unrecognized subcommand 'status'"),
            "unexpected error: {err}"
        ),
    }
}

#[test]
fn servers_info_alias_is_rejected() {
    let res = Cli::try_parse_from(["tako", "servers", "info"]);
    match res {
        Ok(_) => panic!("expected parse failure"),
        Err(err) => assert!(
            err.to_string().contains("unrecognized subcommand 'info'"),
            "unexpected error: {err}"
        ),
    }
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
