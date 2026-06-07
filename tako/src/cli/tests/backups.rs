use super::*;

#[test]
fn backups_now_parses_env_and_server() {
    let cli = Cli::try_parse_from([
        "tako",
        "backups",
        "now",
        "--env",
        "production",
        "--server",
        "la",
    ])
    .unwrap();

    let Some(Commands::Backups(BackupCommands::Now { env, server })) = cli.command else {
        panic!("expected Backups::Now");
    };
    assert_eq!(env.as_deref(), Some("production"));
    assert_eq!(server.as_deref(), Some("la"));
}

#[test]
fn backups_list_alias_parses() {
    let cli = Cli::try_parse_from(["tako", "backups", "ls", "--env", "staging"]).unwrap();

    let Some(Commands::Backups(BackupCommands::List { env, server })) = cli.command else {
        panic!("expected Backups::List");
    };
    assert_eq!(env.as_deref(), Some("staging"));
    assert!(server.is_none());
}

#[test]
fn backups_download_parses_output() {
    let cli = Cli::try_parse_from([
        "tako",
        "backups",
        "download",
        "b123",
        "--env",
        "production",
        "--server",
        "la",
        "--output",
        "./backup.tar.zst",
    ])
    .unwrap();

    let Some(Commands::Backups(BackupCommands::Download {
        backup_id,
        env,
        server,
        output,
    })) = cli.command
    else {
        panic!("expected Backups::Download");
    };
    assert_eq!(backup_id, "b123");
    assert_eq!(env.as_deref(), Some("production"));
    assert_eq!(server.as_deref(), Some("la"));
    assert_eq!(
        output.as_deref(),
        Some(std::path::Path::new("./backup.tar.zst"))
    );
}

#[test]
fn backups_restore_parses_yes() {
    let cli = Cli::try_parse_from([
        "tako", "backups", "restore", "b123", "--server", "la", "--yes",
    ])
    .unwrap();

    let Some(Commands::Backups(BackupCommands::Restore {
        backup_id,
        env,
        server,
        yes,
    })) = cli.command
    else {
        panic!("expected Backups::Restore");
    };
    assert_eq!(backup_id, "b123");
    assert!(env.is_none());
    assert_eq!(server.as_deref(), Some("la"));
    assert!(yes);
}
