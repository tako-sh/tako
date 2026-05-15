use crate::ssh::SshClient;

pub(super) const SERVER_CONFIG_JSON: &str = "/opt/tako/config.json";

#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Deserialize)]
pub(super) struct ServerConfigWithoutSecrets {
    #[serde(default)]
    dns: Option<ServerDnsConfig>,
    #[serde(default)]
    trusted_proxy: Option<ServerTrustedProxyConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
struct ServerDnsConfig {
    provider: String,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Deserialize)]
pub(super) struct ServerTrustedProxyConfig {
    #[serde(default)]
    pub(super) proxy_protocol: bool,
    #[serde(default)]
    pub(super) trusted_cidrs: Vec<String>,
    #[serde(default)]
    pub(super) client_ip_headers: Vec<String>,
}

impl ServerConfigWithoutSecrets {
    pub(super) fn dns_provider(&self) -> Option<&str> {
        self.dns.as_ref().map(|dns| dns.provider.as_str())
    }

    pub(super) fn trusted_proxy(&self) -> Option<&ServerTrustedProxyConfig> {
        self.trusted_proxy.as_ref()
    }
}

pub(super) async fn read_server_config_without_secrets(
    ssh: &SshClient,
) -> Result<ServerConfigWithoutSecrets, Box<dyn std::error::Error>> {
    let output = ssh
        .exec_checked(&read_server_config_without_secrets_command())
        .await?;
    parse_server_config_without_secrets(&output.stdout).map_err(Into::into)
}

fn parse_server_config_without_secrets(input: &str) -> Result<ServerConfigWithoutSecrets, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(ServerConfigWithoutSecrets::default());
    }
    serde_json::from_str(trimmed).map_err(|e| format!("Failed to parse server config: {e}"))
}

fn read_server_config_without_secrets_command() -> String {
    const TEMPLATE: &str = r#"CONFIG=__CONFIG_PATH__
if [ ! -r "$CONFIG" ]; then
  printf '{}'
  exit 0
fi
if command -v python3 >/dev/null 2>&1; then
  python3 -c 'import json,sys
data=json.load(open(sys.argv[1]))
safe={}
dns=data.get("dns") if isinstance(data, dict) else None
provider=dns.get("provider") if isinstance(dns, dict) else None
if isinstance(provider, str):
    safe["dns"]={"provider":provider}
trusted=data.get("trusted_proxy") if isinstance(data, dict) else None
if isinstance(trusted, dict):
    trusted_cidrs=trusted.get("trusted_cidrs")
    client_ip_headers=trusted.get("client_ip_headers")
    safe["trusted_proxy"]={
        "proxy_protocol": trusted.get("proxy_protocol") is True,
        "trusted_cidrs": [value for value in trusted_cidrs if isinstance(value, str)] if isinstance(trusted_cidrs, list) else [],
        "client_ip_headers": [value for value in client_ip_headers if isinstance(value, str)] if isinstance(client_ip_headers, list) else [],
    }
print(json.dumps(safe, separators=(",", ":")))' "$CONFIG"
elif command -v jq >/dev/null 2>&1; then
  jq -c '{dns: (if (.dns.provider? | type) == "string" then {provider: .dns.provider} else null end), trusted_proxy: (if (.trusted_proxy? | type) == "object" then {proxy_protocol: (.trusted_proxy.proxy_protocol == true), trusted_cidrs: [.trusted_proxy.trusted_cidrs[]? | select(type == "string")], client_ip_headers: [.trusted_proxy.client_ip_headers[]? | select(type == "string")]} else null end)} | with_entries(select(.value != null))' "$CONFIG"
else
  echo "error: python3 or jq required to read server config safely" >&2
  exit 1
fi"#;

    TEMPLATE.replace(
        "__CONFIG_PATH__",
        &crate::shell::shell_single_quote(SERVER_CONFIG_JSON),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_server_config_without_secrets_reads_dns_provider() {
        let config = parse_server_config_without_secrets(
            r#"{"dns":{"provider":"cloudflare"},"extra_secret":"ignored"}"#,
        )
        .unwrap();

        assert_eq!(config.dns_provider(), Some("cloudflare"));
    }

    #[test]
    fn parse_server_config_without_secrets_reads_trusted_proxy() {
        let config = parse_server_config_without_secrets(
            r#"{"trusted_proxy":{"proxy_protocol":true,"trusted_cidrs":["127.0.0.1/32"],"client_ip_headers":["cf-connecting-ip"]}}"#,
        )
        .unwrap();

        assert_eq!(
            config.trusted_proxy.as_ref(),
            Some(&ServerTrustedProxyConfig {
                proxy_protocol: true,
                trusted_cidrs: vec!["127.0.0.1/32".to_string()],
                client_ip_headers: vec!["cf-connecting-ip".to_string()],
            }),
        );
    }

    #[test]
    fn parse_server_config_without_secrets_treats_empty_output_as_default() {
        let config = parse_server_config_without_secrets(" \n ").unwrap();

        assert_eq!(config, ServerConfigWithoutSecrets::default());
    }

    #[test]
    fn read_server_config_without_secrets_command_only_reads_config_json() {
        let command = read_server_config_without_secrets_command();

        assert!(command.contains(SERVER_CONFIG_JSON));
        assert!(!command.contains("dns-credentials"));
    }
}
