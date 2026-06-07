use super::*;

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
        "--expires-on",
        "2099-01-01",
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
        expires_on,
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
    assert_eq!(expires_on.as_deref(), Some("2099-01-01"));
    assert!(force_path_style);
    assert_eq!(public_base_url.as_deref(), Some("https://cdn.example.com"));
}

#[test]
fn storages_credentials_parses_resource_options() {
    let cli = Cli::try_parse_from([
        "tako",
        "storages",
        "credentials",
        "backup_r2",
        "--env",
        "production",
        "--access-key-id",
        "key-id",
        "--secret-access-key",
        "secret",
        "--expires-on",
        "2099-01-01",
    ])
    .unwrap();

    let Some(Commands::Storages(StorageCommands::Credentials {
        resource,
        env,
        access_key_id,
        secret_access_key,
        expires_on,
    })) = cli.command
    else {
        panic!("expected Storage::Credentials");
    };

    assert_eq!(resource, "backup_r2");
    assert_eq!(env, "production");
    assert_eq!(access_key_id.as_deref(), Some("key-id"));
    assert_eq!(secret_access_key.as_deref(), Some("secret"));
    assert_eq!(expires_on.as_deref(), Some("2099-01-01"));
}
