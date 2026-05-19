use crate::output;

type InitDnsToken = Option<(String, Option<String>)>;

pub(super) fn prompt_init_dns_token(
    production_route: &str,
) -> Result<InitDnsToken, Box<dyn std::error::Error>> {
    if !output::is_interactive() || !production_route_needs_dns(production_route) {
        return Ok(None);
    }

    let description = "Wildcard routes need DNS-01 certificates. Tako stores the token encrypted in .tako/secrets.json.";
    let should_configure = output::confirm_with_description(
        "Set up Cloudflare DNS for wildcard HTTPS?",
        Some(description),
        true,
    )?;
    if !should_configure {
        return Ok(None);
    }

    let token = crate::commands::dns::read_dns_credential(None, "Cloudflare API token")?;
    let expires_on = output::TextField::new("Expires on")
        .with_hint(crate::config::secret_expires_on_prompt_hint())
        .prompt_validated(|value| {
            crate::config::normalize_secret_expires_on(value)
                .map(|_| ())
                .map_err(|e| e.to_string())
        })?;
    Ok(Some((
        token,
        crate::config::normalize_secret_expires_on(&expires_on)?,
    )))
}

pub(super) fn production_route_needs_dns(route: &str) -> bool {
    route
        .trim()
        .split('/')
        .next()
        .unwrap_or_default()
        .trim()
        .starts_with("*.")
}
