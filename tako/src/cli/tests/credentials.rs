use super::*;

#[test]
fn credentials_set_parses_provider_credential() {
    let cli = Cli::try_parse_from([
        "tako",
        "credentials",
        "set",
        "ssl.cloudflare",
        "--env",
        "staging",
        "--expires-on",
        "2099-01-01",
    ])
    .unwrap();
    let Commands::Credentials(CredentialCommands::Set {
        name,
        env,
        expires_on,
    }) = cli.command.expect("command")
    else {
        panic!("expected Credentials::Set");
    };
    assert_eq!(name, "ssl.cloudflare");
    assert_eq!(env.as_deref(), Some("staging"));
    assert_eq!(expires_on.as_deref(), Some("2099-01-01"));
}

#[test]
fn creds_alias_parses_credentials_command() {
    let cli =
        Cli::try_parse_from(["tako", "creds", "rm", "ssl.cloudflare", "--env", "staging"]).unwrap();
    let Commands::Credentials(CredentialCommands::Rm { name, env }) = cli.command.expect("command")
    else {
        panic!("expected Credentials::Rm");
    };
    assert_eq!(name, "ssl.cloudflare");
    assert_eq!(env.as_deref(), Some("staging"));
}
