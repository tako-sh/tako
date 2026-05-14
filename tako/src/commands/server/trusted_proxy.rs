use crate::output;
use crate::ssh::{SshClient, SshConfig};

const SERVER_CONFIG_JSON: &str = "/opt/tako/config.json";

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
struct TrustedProxyCliConfig {
    proxy_protocol: bool,
    trusted_cidrs: Vec<String>,
    client_ip_headers: Vec<String>,
}

impl TrustedProxyCliConfig {
    fn proxy_protocol(trusted_cidrs: Vec<String>) -> Self {
        Self {
            proxy_protocol: true,
            trusted_cidrs,
            client_ip_headers: Vec::new(),
        }
    }

    fn headers(trusted_cidrs: Vec<String>, client_ip_headers: Vec<String>) -> Self {
        Self {
            proxy_protocol: false,
            trusted_cidrs,
            client_ip_headers,
        }
    }
}

pub(super) async fn configure_trusted_proxy(name: &str) -> Result<(), Box<dyn std::error::Error>> {
    use crate::config::ServersToml;

    let servers = ServersToml::load()?;
    if servers.is_empty() {
        return Err("No servers configured. Run `tako servers add` first.".into());
    }

    let server = servers
        .get(name)
        .ok_or_else(|| format!("Server '{}' not found.", name))?;

    let config = prompt_trusted_proxy_config()?;

    let ssh_config = SshConfig::from_server(&server.host, server.port);
    let mut ssh = SshClient::new(ssh_config);
    ssh.connect()
        .await
        .map_err(|e| format!("Failed to connect to {name}: {e}"))?;
    apply_trusted_proxy_config(&ssh, name, config.as_ref()).await?;
    let _ = ssh.disconnect().await;

    output::success(&format!("Server {} configured", output::strong(name)));

    Ok(())
}

fn prompt_trusted_proxy_config() -> Result<Option<TrustedProxyCliConfig>, Box<dyn std::error::Error>>
{
    let mode = output::select(
        "Source IP mode",
        None,
        vec![
            ("Direct traffic".to_string(), "direct".to_string()),
            (
                "PROXY protocol from a TCP proxy".to_string(),
                "proxy-protocol".to_string(),
            ),
            (
                "Cloudflare HTTP proxy header".to_string(),
                "cloudflare-header".to_string(),
            ),
            (
                "X-Forwarded-For from an HTTP proxy".to_string(),
                "x-forwarded-for".to_string(),
            ),
        ],
    )?;

    match mode.as_str() {
        "direct" => Ok(None),
        "proxy-protocol" => {
            let cidrs = prompt_trusted_cidrs(Some(&default_loopback_cidrs().join(", ")))?;
            Ok(Some(TrustedProxyCliConfig::proxy_protocol(cidrs)))
        }
        "cloudflare-header" => {
            output::muted("Enter Cloudflare proxy CIDRs from https://www.cloudflare.com/ips/.");
            let cidrs = prompt_trusted_cidrs(None)?;
            Ok(Some(TrustedProxyCliConfig::headers(
                cidrs,
                vec!["cf-connecting-ip".to_string()],
            )))
        }
        "x-forwarded-for" => {
            let cidrs = prompt_trusted_cidrs(Some(&default_loopback_cidrs().join(", ")))?;
            Ok(Some(TrustedProxyCliConfig::headers(
                cidrs,
                vec!["x-forwarded-for".to_string()],
            )))
        }
        _ => Err(output::operation_cancelled_error().into()),
    }
}

fn prompt_trusted_cidrs(default: Option<&str>) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let mut field = output::TextField::new("Trusted proxy CIDRs")
        .with_hint("comma-separated, e.g. 127.0.0.1/32");
    if let Some(default) = default {
        field = field.with_default(default);
    }
    let value = field.prompt_validated(|value| parse_cidr_list(value).map(|_| ()))?;
    parse_cidr_list(&value).map_err(Into::into)
}

fn parse_cidr_list(value: &str) -> Result<Vec<String>, String> {
    let cidrs = value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    if cidrs.is_empty() {
        return Err("Enter at least one trusted proxy CIDR.".to_string());
    }

    for cidr in &cidrs {
        cidr.parse::<ipnet::IpNet>()
            .map_err(|e| format!("Invalid CIDR '{cidr}': {e}"))?;
    }

    Ok(cidrs)
}

fn default_loopback_cidrs() -> Vec<String> {
    vec!["127.0.0.1/32".to_string(), "::1/128".to_string()]
}

async fn apply_trusted_proxy_config(
    ssh: &SshClient,
    name: &str,
    config: Option<&TrustedProxyCliConfig>,
) -> Result<(), Box<dyn std::error::Error>> {
    let msg = format!("Configuring source IP on {name}");
    output::with_spinner_async_err(
        &msg,
        &format!("Source IP configured on {name}"),
        &msg,
        apply_trusted_proxy_config_inner(ssh, config),
    )
    .await
}

async fn apply_trusted_proxy_config_inner(
    ssh: &SshClient,
    config: Option<&TrustedProxyCliConfig>,
) -> Result<(), Box<dyn std::error::Error>> {
    let fragment = match config {
        Some(config) => Some(trusted_proxy_config_fragment(config)?),
        None => None,
    };
    let escaped_fragment = fragment
        .as_deref()
        .map(crate::shell::shell_single_quote)
        .unwrap_or_else(|| "''".to_string());
    let mode = if fragment.is_some() { "set" } else { "clear" };

    let merge_cmd = SshClient::run_with_root_or_sudo(&format!(
        r#"CONFIG="{path}"; \
         MODE={mode}; \
         FRAGMENT={fragment}; \
         EXISTING="$(cat "$CONFIG" 2>/dev/null || echo '{{}}')"; \
         if ! command -v python3 >/dev/null 2>&1; then \
           echo "error: python3 required" >&2 && exit 1; \
         fi; \
         python3 -c "import json,sys; d=json.loads(sys.argv[1] or '{{}}'); mode=sys.argv[2]; frag=sys.argv[3]; d.pop('trusted_proxy', None) if mode == 'clear' else d.__setitem__('trusted_proxy', json.loads(frag)); json.dump(d, open(sys.argv[4], 'w'))" "$EXISTING" "$MODE" "$FRAGMENT" "$CONFIG.tmp" && \
         mv "$CONFIG.tmp" "$CONFIG" && chmod 0644 "$CONFIG" && chown tako:tako "$CONFIG""#,
        path = SERVER_CONFIG_JSON,
        mode = crate::shell::shell_single_quote(mode),
        fragment = escaped_fragment,
    ));
    ssh.exec_checked(&merge_cmd).await?;

    let restart_cmd = SshClient::run_with_root_or_sudo(
        "if command -v systemctl >/dev/null 2>&1; then \
           systemctl restart tako-server; \
         elif command -v rc-service >/dev/null 2>&1; then \
           rc-service tako-server restart; \
         else \
           service tako-server restart; \
         fi",
    );
    ssh.exec_checked(&restart_cmd).await?;

    Ok(())
}

fn trusted_proxy_config_fragment(config: &TrustedProxyCliConfig) -> Result<String, String> {
    serde_json::to_string(config).map_err(|e| format!("Failed to encode trusted proxy config: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_fragment_for_proxy_protocol_uses_loopback_default() {
        let config = TrustedProxyCliConfig::proxy_protocol(default_loopback_cidrs());
        let fragment = trusted_proxy_config_fragment(&config).unwrap();

        assert!(fragment.contains(r#""proxy_protocol":true"#));
        assert!(fragment.contains(r#""127.0.0.1/32""#));
        assert!(fragment.contains(r#""::1/128""#));
        assert!(fragment.contains(r#""client_ip_headers":[]"#));
    }

    #[test]
    fn config_fragment_for_cloudflare_headers_uses_cf_connecting_ip() {
        let config = TrustedProxyCliConfig::headers(
            vec!["203.0.113.0/24".to_string()],
            vec!["cf-connecting-ip".to_string()],
        );
        let fragment = trusted_proxy_config_fragment(&config).unwrap();

        assert!(fragment.contains(r#""proxy_protocol":false"#));
        assert!(fragment.contains(r#""client_ip_headers":["cf-connecting-ip"]"#));
    }

    #[test]
    fn cidr_list_parser_trims_commas_and_spaces() {
        assert_eq!(
            parse_cidr_list(" 127.0.0.1/32, ::1/128  ").unwrap(),
            vec!["127.0.0.1/32".to_string(), "::1/128".to_string()]
        );
    }
}
