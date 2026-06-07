use super::*;

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
