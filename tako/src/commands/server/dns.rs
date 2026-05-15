use crate::output;
use crate::ssh::SshClient;
use std::time::Duration;

const DNS_CREDENTIALS_ENV: &str = "/opt/tako/dns-credentials.env";
const CLOUDFLARE_DNS_PROVIDER: &str = "cloudflare";
const CLOUDFLARE_TOKEN_HINT: &str = "Can read zones and edit DNS records for your zone.";
const INVALID_DNS_CREDENTIALS_MESSAGE: &str =
    "Credentials invalid. Check your API token and try again.";

/// Return a shell command to quickly verify credentials for a provider, if
/// supported. The command should exit 0 on success, non-zero on failure.
fn credential_verify_command(provider: &str, credentials: &[(String, String)]) -> Option<String> {
    match provider {
        CLOUDFLARE_DNS_PROVIDER => {
            let token = credentials.iter().find(|(k, _)| k == "CF_DNS_API_TOKEN")?;
            let header = format!("Authorization: Bearer {}", token.1);
            let escaped = crate::shell::shell_single_quote(&header);
            Some(format!(
                "curl -sf -H {} \
                 https://api.cloudflare.com/client/v4/user/tokens/verify \
                 | grep -q '\"active\"'",
                escaped,
            ))
        }
        _ => None,
    }
}

fn run_credential_verify_command(command: &str) -> Result<(), String> {
    let output = std::process::Command::new("sh")
        .args(["-c", command])
        .output()
        .map_err(|e| format!("Failed to run credential verification: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(INVALID_DNS_CREDENTIALS_MESSAGE.to_string())
    }
}

fn verify_dns_credentials(provider: &str, credentials: &[(String, String)]) -> Result<(), String> {
    if let Some(command) = credential_verify_command(provider, credentials) {
        let _t = output::timed(&format!("Verify DNS credentials ({provider})"));
        run_credential_verify_command(&command)?;
    }
    Ok(())
}

fn supported_dns_provider_options() -> Vec<(String, &'static str)> {
    vec![("cloudflare".to_string(), CLOUDFLARE_DNS_PROVIDER)]
}

/// Supported DNS providers and their required environment variables.
fn dns_provider_env_vars(provider: &str) -> &'static [(&'static str, &'static str)] {
    match provider {
        CLOUDFLARE_DNS_PROVIDER => &[("CF_DNS_API_TOKEN", "Cloudflare API token")],
        _ => &[],
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DnsConfig {
    provider: String,
    credentials_env: String, // KEY=VALUE\n content
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DnsConfigChange {
    Enable(DnsConfig),
    Disable,
    Unchanged,
}

#[cfg(test)]
fn dns_current_status(current_provider: Option<&str>) -> String {
    match current_provider {
        None => "currently disabled".to_string(),
        Some(CLOUDFLARE_DNS_PROVIDER) => "currently enabled with Cloudflare".to_string(),
        Some(provider) => format!("currently configured for unsupported provider '{provider}'"),
    }
}

fn dns_wildcard_prompt_default(current_provider: Option<&str>) -> bool {
    current_provider.is_some()
}

fn dns_wildcard_prompt_description(current_provider: Option<&str>) -> String {
    match current_provider {
        None => "Use this for wildcard routes such as *.example.com.".to_string(),
        Some(CLOUDFLARE_DNS_PROVIDER) => {
            "Cloudflare DNS-01 is already enabled. Answer no to disable it.".to_string()
        }
        Some(provider) => {
            format!(
                "DNS-01 is configured for unsupported provider '{provider}'. Answer yes to switch to Cloudflare."
            )
        }
    }
}

fn dns_no_wildcard_change(current_provider: Option<&str>) -> DnsConfigChange {
    if current_provider.is_some() {
        DnsConfigChange::Disable
    } else {
        DnsConfigChange::Unchanged
    }
}

/// Interactively prompt for the DNS wildcard mode and Cloudflare credentials.
/// Does not write anything to any server.
pub(super) fn prompt_dns_setup(
    current_provider: Option<&str>,
) -> Result<DnsConfigChange, Box<dyn std::error::Error>> {
    let description = dns_wildcard_prompt_description(current_provider);
    let needs_wildcards = output::confirm_with_description(
        "Need DNS wildcard certificates?",
        Some(&description),
        dns_wildcard_prompt_default(current_provider),
    )?;

    if !needs_wildcards {
        return Ok(dns_no_wildcard_change(current_provider));
    }

    if current_provider == Some(CLOUDFLARE_DNS_PROVIDER)
        && !output::confirm("Update Cloudflare API token?", false)?
    {
        return Ok(DnsConfigChange::Unchanged);
    }

    let provider = supported_dns_provider_options()
        .into_iter()
        .next()
        .map(|(_, provider)| provider.to_string())
        .expect("Cloudflare DNS provider is configured");

    let mut wizard = output::Wizard::new().with_fields(&[("Cloudflare API token", false)]);

    // Validate before the prompt completes, so invalid input stays on the
    // current wizard step with the error shown under the field.
    let known_vars = dns_provider_env_vars(&provider);
    let mut credentials: Vec<(String, String)> = Vec::new();

    for &(var_name, description) in known_vars {
        let provider_for_validation = provider.clone();
        let var_name_for_validation = var_name.to_string();
        let value = wizard.text_field_named_validated_with_spinner(
            "Cloudflare API token",
            output::TextField::new(description)
                .password()
                .with_hint(CLOUDFLARE_TOKEN_HINT),
            move |value| {
                let candidate = [(var_name_for_validation.clone(), value)];
                verify_dns_credentials(&provider_for_validation, &candidate)
            },
        )?;
        credentials.push((var_name.to_string(), value));
    }

    if credentials.is_empty() {
        return Err(output::operation_cancelled_error().into());
    }

    let mut env_content = String::new();
    for (key, value) in &credentials {
        env_content.push_str(&format!("{}={}\n", key, value));
    }

    Ok(DnsConfigChange::Enable(DnsConfig {
        provider,
        credentials_env: env_content,
    }))
}

pub(super) async fn configure_dns(
    name: &str,
    ssh: &SshClient,
    current_config: &super::remote_config::ServerConfigWithoutSecrets,
) -> Result<(), Box<dyn std::error::Error>> {
    match prompt_dns_setup(current_config.dns_provider())? {
        DnsConfigChange::Enable(dns_config) => {
            apply_dns_config(ssh, name, &dns_config).await?;
            output::success(&format!("Server {} configured", output::strong(name)));
        }
        DnsConfigChange::Disable => {
            disable_dns_config(ssh, name).await?;
            output::success(&format!("Server {} configured", output::strong(name)));
        }
        DnsConfigChange::Unchanged => {
            let state = if current_config.dns_provider().is_some() {
                "DNS wildcard certificates unchanged on"
            } else {
                "DNS wildcard certificates disabled on"
            };
            output::success(&format!("{state} {}", output::strong(name)));
        }
    }

    Ok(())
}

/// Apply a DNS config to a server: write credentials, configure systemd, and
/// reload tako-server.
/// All intermediate output is suppressed — shows a single spinner line.
async fn apply_dns_config(
    ssh: &SshClient,
    name: &str,
    config: &DnsConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let msg = format!("Configuring DNS on {name}");
    output::with_spinner_async_err(
        &msg,
        &format!("DNS configured on {name}"),
        &msg,
        apply_dns_config_inner(ssh, name, config),
    )
    .await
}

async fn apply_dns_config_inner(
    ssh: &SshClient,
    name: &str,
    config: &DnsConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let provider = &config.provider;

    // Write credentials env file
    let _t = output::timed(&format!("[{name}] Write DNS credentials ({provider})"));
    ssh.exec_checked(&write_dns_credentials_command(config))
        .await?;
    drop(_t);

    // Merge dns.provider into config.json
    ssh.exec_checked(&merge_dns_provider_command(provider))
        .await?;

    // Write systemd drop-in for EnvironmentFile (credentials only), reload, and restart
    let _t = output::timed("Systemd reload + restart");
    ssh.exec_checked(&write_dns_systemd_dropin_command(true))
        .await?;
    drop(_t);
    tracing::debug!("DNS configured, tako-server restarted");

    // Verify the new config took effect (retry up to 5 times)
    for attempt in 0..5 {
        tokio::time::sleep(Duration::from_secs(if attempt == 0 { 2 } else { 3 })).await;
        match ssh.tako_server_info().await {
            Ok(info) if info.dns_provider.as_deref() == Some(provider.as_str()) => {
                return Ok(());
            }
            Ok(_) if attempt < 4 => continue,
            Ok(info) => {
                return Err(format!(
                    "DNS provider on {} is {:?} after restart (expected '{}').\n\
                     Try: tako servers reload {}",
                    name, info.dns_provider, provider, name,
                )
                .into());
            }
            Err(_) if attempt < 4 => continue,
            Err(e) => {
                return Err(format!(
                    "Could not verify DNS config on {} after restart: {}",
                    name, e,
                )
                .into());
            }
        }
    }
    Ok(())
}

pub(super) async fn apply_dns_config_before_start(
    ssh: &SshClient,
    name: &str,
    change: &DnsConfigChange,
) -> Result<(), Box<dyn std::error::Error>> {
    match change {
        DnsConfigChange::Enable(config) => {
            let provider = &config.provider;
            let _t = output::timed(&format!("[{name}] Write DNS credentials ({provider})"));
            ssh.exec_checked(&write_dns_credentials_command(config))
                .await?;
            drop(_t);

            ssh.exec_checked(&merge_dns_provider_command(provider))
                .await?;
            ssh.exec_checked(&write_dns_systemd_dropin_command(false))
                .await?;
        }
        DnsConfigChange::Disable => {
            let disable_cmd = SshClient::run_with_root_or_sudo(&disable_dns_config_command(false));
            ssh.exec_checked(&disable_cmd).await?;
        }
        DnsConfigChange::Unchanged => {}
    }

    Ok(())
}

fn write_dns_credentials_command(config: &DnsConfig) -> String {
    let escaped_content = crate::shell::shell_single_quote(&config.credentials_env);
    SshClient::run_with_root_or_sudo(&format!(
        "printf '%s' {} > {} && chmod 0600 {} && chown tako:tako {}",
        escaped_content, DNS_CREDENTIALS_ENV, DNS_CREDENTIALS_ENV, DNS_CREDENTIALS_ENV,
    ))
}

fn merge_dns_provider_command(provider: &str) -> String {
    let escaped_provider = crate::shell::shell_single_quote(provider);
    SshClient::run_with_root_or_sudo(&format!(
        r#"CONFIG="{path}"; \
         PROVIDER={provider}; \
         EXISTING="$(cat "$CONFIG" 2>/dev/null || echo '{{}}')"; \
         if command -v jq >/dev/null 2>&1; then \
           echo "$EXISTING" | jq --arg p "$PROVIDER" '.dns.provider = $p' > "$CONFIG.tmp"; \
         elif command -v python3 >/dev/null 2>&1; then \
           python3 -c "import json,sys; d=json.loads(sys.argv[1]); d.setdefault('dns',{{}}); d['dns']['provider']=sys.argv[2]; json.dump(d,open(sys.argv[3],'w'))" "$EXISTING" "$PROVIDER" "$CONFIG.tmp"; \
         else \
           echo "error: jq or python3 required" >&2 && exit 1; \
         fi && \
         mv "$CONFIG.tmp" "$CONFIG" && chmod 0644 "$CONFIG" && chown tako:tako "$CONFIG""#,
        path = super::remote_config::SERVER_CONFIG_JSON,
        provider = escaped_provider,
    ))
}

fn write_dns_systemd_dropin_command(restart: bool) -> String {
    let dropin = format!("[Service]\nEnvironmentFile={}\n", DNS_CREDENTIALS_ENV,);
    let escaped_dropin = dropin.replace('\'', "'\\''");
    let fallback_restart = if restart {
        "elif command -v rc-service >/dev/null 2>&1; then \
           rc-service tako-server restart; \
         else \
           service tako-server restart; \
         fi"
    } else {
        "fi"
    };
    let systemd_action = if restart {
        "systemctl daemon-reload && systemctl restart tako-server"
    } else {
        ":"
    };
    SshClient::run_with_root_or_sudo(&format!(
        "if command -v systemctl >/dev/null 2>&1; then \
         mkdir -p /etc/systemd/system/tako-server.service.d && \
         printf '%s' '{}' > /etc/systemd/system/tako-server.service.d/dns.conf && \
         {}; \
         {}",
        escaped_dropin, systemd_action, fallback_restart,
    ))
}

async fn disable_dns_config(ssh: &SshClient, name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let msg = format!("Disabling DNS on {name}");
    output::with_spinner_async_err(
        &msg,
        &format!("DNS disabled on {name}"),
        &msg,
        disable_dns_config_inner(ssh, name),
    )
    .await
}

async fn disable_dns_config_inner(
    ssh: &SshClient,
    name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let _t = output::timed(&format!("[{name}] Disable DNS config"));
    let disable_cmd = SshClient::run_with_root_or_sudo(&disable_dns_config_command(true));
    ssh.exec_checked(&disable_cmd).await?;
    drop(_t);

    for attempt in 0..5 {
        tokio::time::sleep(Duration::from_secs(if attempt == 0 { 2 } else { 3 })).await;
        match ssh.tako_server_info().await {
            Ok(info) if info.dns_provider.is_none() => return Ok(()),
            Ok(_) if attempt < 4 => continue,
            Ok(info) => {
                return Err(format!(
                    "DNS provider on {} is {:?} after restart (expected disabled).\n\
                     Try: tako servers reload {}",
                    name, info.dns_provider, name,
                )
                .into());
            }
            Err(_) if attempt < 4 => continue,
            Err(e) => {
                return Err(format!(
                    "Could not verify DNS config on {} after restart: {}",
                    name, e,
                )
                .into());
            }
        }
    }

    Ok(())
}

fn disable_dns_config_command(restart: bool) -> String {
    let restart_command = if restart {
        "if command -v systemctl >/dev/null 2>&1; then \
           systemctl daemon-reload && systemctl restart tako-server; \
         elif command -v rc-service >/dev/null 2>&1; then \
           rc-service tako-server restart; \
         else \
           service tako-server restart; \
         fi"
    } else {
        ":"
    };
    format!(
        r#"CONFIG="{config_path}"; \
         EXISTING="$(cat "$CONFIG" 2>/dev/null || echo '{{}}')"; \
         if ! command -v python3 >/dev/null 2>&1; then \
           echo "error: python3 required" >&2 && exit 1; \
         fi; \
         python3 -c "import json,sys; d=json.loads(sys.argv[1] or '{{}}'); d.pop('dns', None); json.dump(d, open(sys.argv[2], 'w'))" "$EXISTING" "$CONFIG.tmp" && \
         mv "$CONFIG.tmp" "$CONFIG" && chmod 0644 "$CONFIG" && chown tako:tako "$CONFIG" && \
         rm -f {credentials_path} /etc/systemd/system/tako-server.service.d/dns.conf && \
         {restart_command}"#,
        config_path = super::remote_config::SERVER_CONFIG_JSON,
        credentials_path = DNS_CREDENTIALS_ENV,
        restart_command = restart_command,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_verification_accepts_successful_command() {
        assert_eq!(run_credential_verify_command("true"), Ok(()));
    }

    #[test]
    fn credential_verification_rejects_failed_command() {
        assert_eq!(
            run_credential_verify_command("false").unwrap_err(),
            INVALID_DNS_CREDENTIALS_MESSAGE,
        );
    }

    #[test]
    fn dns_provider_env_vars_returns_cloudflare_vars() {
        let vars = dns_provider_env_vars("cloudflare");
        assert_eq!(vars.len(), 1);
        assert_eq!(vars[0].0, "CF_DNS_API_TOKEN");
    }

    #[test]
    fn dns_provider_env_vars_returns_empty_for_legacy_provider() {
        let vars = dns_provider_env_vars("route53");
        assert!(vars.is_empty());
    }

    #[test]
    fn supported_dns_provider_options_only_include_cloudflare() {
        assert_eq!(
            supported_dns_provider_options(),
            vec![("cloudflare".to_string(), "cloudflare")],
        );
    }

    #[test]
    fn cloudflare_token_hint_describes_required_actions() {
        assert_eq!(
            CLOUDFLARE_TOKEN_HINT,
            "Can read zones and edit DNS records for your zone.",
        );
    }

    #[test]
    fn dns_current_status_describes_known_states() {
        assert_eq!(dns_current_status(None), "currently disabled");
        assert_eq!(
            dns_current_status(Some("cloudflare")),
            "currently enabled with Cloudflare"
        );
        assert_eq!(
            dns_current_status(Some("route53")),
            "currently configured for unsupported provider 'route53'"
        );
    }

    #[test]
    fn dns_wildcard_prompt_defaults_to_current_state() {
        assert!(!dns_wildcard_prompt_default(None));
        assert!(dns_wildcard_prompt_default(Some("cloudflare")));
        assert!(dns_wildcard_prompt_default(Some("route53")));
    }

    #[test]
    fn dns_wildcard_prompt_description_explains_disabled_state() {
        assert_eq!(
            dns_wildcard_prompt_description(None),
            "Use this for wildcard routes such as *.example.com.",
        );
    }

    #[test]
    fn no_wildcard_choice_disables_existing_dns() {
        assert_eq!(dns_no_wildcard_change(None), DnsConfigChange::Unchanged,);
        assert_eq!(
            dns_no_wildcard_change(Some("cloudflare")),
            DnsConfigChange::Disable,
        );
    }

    #[test]
    fn disable_dns_config_command_removes_dns_provider_and_credentials() {
        let command = disable_dns_config_command(true);
        assert!(command.contains("d.pop('dns', None)"));
        assert!(command.contains(DNS_CREDENTIALS_ENV));
        assert!(command.contains("dns.conf"));
    }

    #[test]
    fn pre_start_dns_command_writes_config_without_restarting_service() {
        let command = write_dns_systemd_dropin_command(false);

        assert!(command.contains("EnvironmentFile=/opt/tako/dns-credentials.env"));
        assert!(!command.contains("systemctl restart tako-server"));
        assert!(!command.contains("rc-service tako-server restart"));
    }

    #[test]
    fn dns_restart_command_supports_openrc_after_writing_config() {
        let command = write_dns_systemd_dropin_command(true);

        assert!(command.contains("systemctl restart tako-server"));
        assert!(command.contains("rc-service tako-server restart"));
    }
}
