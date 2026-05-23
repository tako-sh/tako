use crate::output;

type InitSslToken = Option<(String, Option<String>)>;

pub(super) fn prompt_init_ssl_token(
    production_route: &str,
) -> Result<InitSslToken, Box<dyn std::error::Error>> {
    if !output::is_interactive() || !production_route_needs_wildcard_ssl(production_route) {
        return Ok(None);
    }

    let description = "Let’s Encrypt wildcard certificates use Cloudflare DNS-01. Tako stores the token as an encrypted credential.";
    let should_setup =
        output::confirm_with_description("Set up wildcard HTTPS?", Some(description), true)?;
    if !should_setup {
        return Ok(None);
    }

    let token = crate::commands::credentials::read_credential_value(
        None,
        "Cloudflare API token for wildcard certificates",
    )?;
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

pub(super) fn production_route_needs_wildcard_ssl(route: &str) -> bool {
    route
        .trim()
        .split('/')
        .next()
        .unwrap_or_default()
        .trim()
        .starts_with("*.")
}
