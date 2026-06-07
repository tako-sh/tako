use super::*;

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
fn secrets_set_parses_expiry() {
    let cli = Cli::try_parse_from([
        "tako",
        "secrets",
        "set",
        "API_KEY",
        "--env",
        "production",
        "--expires-on",
        "2099-01-01",
    ])
    .unwrap();

    let Some(Commands::Secrets(secret::SecretCommands::Set {
        name,
        env,
        expires_on,
        sync,
    })) = cli.command
    else {
        panic!("expected Secrets::Set");
    };

    assert_eq!(name, "API_KEY");
    assert_eq!(env.as_deref(), Some("production"));
    assert_eq!(expires_on.as_deref(), Some("2099-01-01"));
    assert!(!sync);
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
