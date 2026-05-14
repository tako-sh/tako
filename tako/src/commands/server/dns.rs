use crate::output;
use crate::ssh::SshClient;
use std::time::Duration;

const SERVER_CONFIG_JSON: &str = "/opt/tako/config.json";
const DNS_CREDENTIALS_ENV: &str = "/opt/tako/dns-credentials.env";

/// Return a shell command to quickly verify credentials for a provider, if
/// supported. The command should exit 0 on success, non-zero on failure.
fn credential_verify_command(provider: &str, credentials: &[(String, String)]) -> Option<String> {
    match provider {
        "cloudflare" => {
            let token = credentials.iter().find(|(k, _)| k == "CF_DNS_API_TOKEN")?;
            let escaped = crate::shell::shell_single_quote(&token.1);
            Some(format!(
                "curl -sf -H 'Authorization: Bearer '{} \
                 https://api.cloudflare.com/client/v4/user/tokens/verify \
                 | grep -q '\"active\"'",
                escaped,
            ))
        }
        "digitalocean" => {
            let token = credentials.iter().find(|(k, _)| k == "DO_AUTH_TOKEN")?;
            let escaped = crate::shell::shell_single_quote(&token.1);
            Some(format!(
                "curl -sf -H 'Authorization: Bearer '{} \
                 https://api.digitalocean.com/v2/account \
                 | grep -q '\"account\"'",
                escaped,
            ))
        }
        "hetzner" => {
            let token = credentials.iter().find(|(k, _)| k == "HETZNER_API_KEY")?;
            let escaped = crate::shell::shell_single_quote(&token.1);
            Some(format!(
                "curl -sf -H 'Auth-API-Token: '{} \
                 https://dns.hetzner.com/api/v1/zones >/dev/null",
                escaped,
            ))
        }
        "vultr" => {
            let token = credentials.iter().find(|(k, _)| k == "VULTR_API_KEY")?;
            let escaped = crate::shell::shell_single_quote(&token.1);
            Some(format!(
                "curl -sf -H 'Authorization: Bearer '{} \
                 https://api.vultr.com/v2/account >/dev/null",
                escaped,
            ))
        }
        "linode" => {
            let token = credentials.iter().find(|(k, _)| k == "LINODE_TOKEN")?;
            let escaped = crate::shell::shell_single_quote(&token.1);
            Some(format!(
                "curl -sf -H 'Authorization: Bearer '{} \
                 https://api.linode.com/v4/profile >/dev/null",
                escaped,
            ))
        }
        _ => None,
    }
}

/// Well-known DNS providers and their required environment variables.
fn dns_provider_env_vars(provider: &str) -> &'static [(&'static str, &'static str)] {
    match provider {
        "cloudflare" => &[(
            "CF_DNS_API_TOKEN",
            "Cloudflare API token (DNS edit permission)",
        )],
        "route53" => &[
            ("AWS_ACCESS_KEY_ID", "AWS access key ID"),
            ("AWS_SECRET_ACCESS_KEY", "AWS secret access key"),
            ("AWS_REGION", "AWS region (e.g. us-east-1)"),
        ],
        "digitalocean" => &[("DO_AUTH_TOKEN", "DigitalOcean API token")],
        "hetzner" => &[("HETZNER_API_KEY", "Hetzner DNS API token")],
        "vultr" => &[("VULTR_API_KEY", "Vultr API key")],
        "linode" => &[("LINODE_TOKEN", "Linode API token")],
        "namecheap" => &[
            ("NAMECHEAP_API_USER", "Namecheap API user"),
            ("NAMECHEAP_API_KEY", "Namecheap API key"),
        ],
        "gcloud" => &[
            ("GCE_PROJECT", "Google Cloud project ID"),
            (
                "GCE_SERVICE_ACCOUNT_FILE",
                "Path to service account JSON key file",
            ),
        ],
        _ => &[],
    }
}

struct DnsConfig {
    provider: String,
    credentials_env: String, // KEY=VALUE\n content
}

/// Interactively prompt for DNS provider and credentials, verify them locally,
/// and return the config. Does not write anything to any server.
async fn prompt_dns_setup() -> Result<DnsConfig, Box<dyn std::error::Error>> {
    // Select DNS provider
    let provider_options = vec![
        ("cloudflare".to_string(), "cloudflare"),
        ("route53 (AWS)".to_string(), "route53"),
        ("digitalocean".to_string(), "digitalocean"),
        ("hetzner".to_string(), "hetzner"),
        ("vultr".to_string(), "vultr"),
        ("linode".to_string(), "linode"),
        ("namecheap".to_string(), "namecheap"),
        ("gcloud (Google Cloud DNS)".to_string(), "gcloud"),
        ("other (enter manually)".to_string(), "other"),
    ];

    let provider = output::select(
        "Choose your DNS provider for Let's Encrypt DNS-01 challenges",
        None,
        provider_options,
    )?;

    let provider = if provider == "other" {
        output::TextField::new("DNS provider name")
            .with_hint("lego provider code")
            .prompt()?
    } else {
        provider.to_string()
    };

    // Collect credentials
    let known_vars = dns_provider_env_vars(&provider);
    let mut credentials: Vec<(String, String)> = Vec::new();

    if known_vars.is_empty() {
        output::muted(&format!(
            "Provider '{}' — enter environment variables required by lego.",
            provider,
        ));
        output::muted("See https://go-acme.github.io/lego/dns/ for provider docs.");
        output::muted("Enter variable name, then value. Empty name to finish.");

        loop {
            let key = output::TextField::new("Variable name")
                .optional()
                .prompt()?;
            let key = key.trim().to_string();
            if key.is_empty() {
                break;
            }
            let value = output::password_field(&format!("{key} value"))?;
            credentials.push((key, value));
        }
    } else {
        for (var_name, description) in known_vars {
            let value = output::password_field(description)?;
            credentials.push((var_name.to_string(), value));
        }
    }

    if credentials.is_empty() {
        return Err(output::operation_cancelled_error().into());
    }

    // Quick credential validation via provider API (runs locally).
    if let Some(verify_cmd) = credential_verify_command(&provider, &credentials) {
        let _t = output::timed(&format!("Verify DNS credentials ({provider})"));
        let verify_future = async {
            let out = tokio::process::Command::new("sh")
                .args(["-c", &verify_cmd])
                .output()
                .await
                .map_err(|e| -> Box<dyn std::error::Error> {
                    format!("Failed to run credential verification: {e}").into()
                })?;
            if !out.status.success() {
                return Err::<(), Box<dyn std::error::Error>>(
                    "Credentials invalid. Check your API token and try again.".into(),
                );
            }
            Ok(())
        };
        output::with_spinner_async_err(
            "Verifying credentials",
            "Credentials valid",
            "Verifying credentials",
            verify_future,
        )
        .await?;
    }

    let mut env_content = String::new();
    for (key, value) in &credentials {
        env_content.push_str(&format!("{}={}\n", key, value));
    }

    Ok(DnsConfig {
        provider,
        credentials_env: env_content,
    })
}

pub(super) async fn configure_server(name: &str) -> Result<(), Box<dyn std::error::Error>> {
    use crate::config::ServersToml;
    use crate::ssh::SshConfig;

    let servers = ServersToml::load()?;
    if servers.is_empty() {
        return Err("No servers configured. Run `tako servers add` first.".into());
    }

    let server = servers
        .get(name)
        .ok_or_else(|| format!("Server '{}' not found.", name))?;

    let dns_config = prompt_dns_setup().await?;

    let ssh_config = SshConfig::from_server(&server.host, server.port);
    let mut ssh = SshClient::new(ssh_config);
    ssh.connect()
        .await
        .map_err(|e| format!("Failed to connect to {name}: {e}"))?;
    apply_dns_config(&ssh, name, &dns_config).await?;
    let _ = ssh.disconnect().await;

    output::success(&format!("Server {} configured", output::strong(name)));

    Ok(())
}

/// Apply a DNS config to a server: write credentials, configure systemd, and
/// reload tako-server. The server installs lego on-demand when it needs it.
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
    let escaped_content = crate::shell::shell_single_quote(&config.credentials_env);
    let write_creds_cmd = SshClient::run_with_root_or_sudo(&format!(
        "printf '%s' {} > {} && chmod 0600 {} && chown tako:tako {}",
        escaped_content, DNS_CREDENTIALS_ENV, DNS_CREDENTIALS_ENV, DNS_CREDENTIALS_ENV,
    ));
    ssh.exec_checked(&write_creds_cmd).await?;
    drop(_t);

    // Merge dns.provider into config.json
    let escaped_provider = crate::shell::shell_single_quote(provider);
    let merge_config_cmd = SshClient::run_with_root_or_sudo(&format!(
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
        path = SERVER_CONFIG_JSON,
        provider = escaped_provider,
    ));
    ssh.exec_checked(&merge_config_cmd).await?;

    // Write systemd drop-in for EnvironmentFile (credentials only), reload, and restart
    let _t = output::timed("Systemd reload + restart");
    let dropin = format!("[Service]\nEnvironmentFile={}\n", DNS_CREDENTIALS_ENV,);
    let escaped_dropin = dropin.replace('\'', "'\\''");
    let write_dropin_cmd = SshClient::run_with_root_or_sudo(&format!(
        "mkdir -p /etc/systemd/system/tako-server.service.d && \
         printf '%s' '{}' > /etc/systemd/system/tako-server.service.d/dns.conf && \
         systemctl daemon-reload && \
         systemctl restart tako-server",
        escaped_dropin,
    ));
    ssh.exec_checked(&write_dropin_cmd).await?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dns_provider_env_vars_returns_cloudflare_vars() {
        let vars = dns_provider_env_vars("cloudflare");
        assert_eq!(vars.len(), 1);
        assert_eq!(vars[0].0, "CF_DNS_API_TOKEN");
    }

    #[test]
    fn dns_provider_env_vars_returns_empty_for_unknown() {
        let vars = dns_provider_env_vars("some-obscure-provider");
        assert!(vars.is_empty());
    }
}
