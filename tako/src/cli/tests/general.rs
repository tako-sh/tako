use super::*;

#[test]
fn global_ssh_passphrase_parses_for_one_liners() {
    let cli = Cli::try_parse_from([
        "tako",
        "network",
        "server",
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
        let Commands::Generate { .. } = cli.command.expect("command") else {
            panic!("expected Commands::Generate for {subcommand}");
        };
    }
}

#[test]
fn run_parses_env_and_trailing_command() {
    let cli = Cli::try_parse_from([
        "tako",
        "run",
        "--env",
        "staging",
        "--",
        "bun",
        "scripts/foo.ts",
        "--force",
    ])
    .unwrap();

    let Some(Commands::Run { env, eval, command }) = cli.command else {
        panic!("expected Run");
    };
    assert_eq!(env.as_deref(), Some("staging"));
    assert_eq!(eval, None);
    assert_eq!(command, ["bun", "scripts/foo.ts", "--force"]);
}

#[test]
fn run_parses_eval_and_trailing_args() {
    let cli =
        Cli::try_parse_from(["tako", "run", "--eval", "console.log(1)", "--", "--force"]).unwrap();

    let Some(Commands::Run { eval, command, .. }) = cli.command else {
        panic!("expected Run");
    };
    assert_eq!(eval.as_deref(), Some("console.log(1)"));
    assert_eq!(command, ["--force"]);
}

#[test]
fn top_level_status_command_parses() {
    let cli = Cli::try_parse_from(["tako", "status"]).unwrap();
    assert!(matches!(cli.command, Some(Commands::Status)));
}

#[test]
fn telemetry_command_uses_top_level_name() {
    let cases = [
        (&["tako", "init"][..], "init"),
        (&["tako", "dev"][..], "dev"),
        (&["tako", "dev", "vite", "dev"][..], "dev"),
        (&["tako", "dev", "list"][..], "dev list"),
        (&["tako", "dev", "stop"][..], "dev stop"),
        (&["tako", "deploy", "--yes"][..], "deploy"),
        (&["tako", "network", "server", "list"][..], "network"),
        (&["tako", "secrets", "list"][..], "secrets"),
        (&["tako", "version"][..], "version"),
        (&["tako", "scale", "2"][..], "scale"),
        (&["tako", "workflows", "list"][..], "workflows"),
    ];
    for (args, expected) in cases {
        let cli = Cli::try_parse_from(args).unwrap();
        assert_eq!(
            telemetry_command(cli.command.as_ref().expect("command")),
            expected,
            "args={args:?}"
        );
    }
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
fn completions_command_parses() {
    let cli = Cli::try_parse_from(["tako", "completions"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(Commands::Completions { yes: false })
    ));
}

#[test]
fn completions_yes_flag_parses() {
    let cli = Cli::try_parse_from(["tako", "completions", "--yes"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(Commands::Completions { yes: true })
    ));
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
fn json_flag_parses_globally_before_subcommand() {
    let cli = Cli::try_parse_from(["tako", "--json", "deploy"]).unwrap();
    assert!(cli.json);
}

#[test]
fn json_flag_parses_globally_after_subcommand() {
    let cli = Cli::try_parse_from(["tako", "deploy", "--json"]).unwrap();
    assert!(cli.json);
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
fn logs_json_flag_parses() {
    let cli = Cli::try_parse_from(["tako", "logs", "--json"]).unwrap();
    let Some(Commands::Logs { .. }) = cli.command else {
        panic!("expected Logs");
    };
    assert!(cli.json);
}

#[test]
fn logs_tail_json_flag_parses() {
    let cli = Cli::try_parse_from(["tako", "logs", "--tail", "--json"]).unwrap();
    let Some(Commands::Logs { tail, .. }) = cli.command else {
        panic!("expected Logs");
    };
    assert!(cli.json);
    assert!(tail);
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
