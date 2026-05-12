use reqwest::header::{ACCEPT, AUTHORIZATION};

const GITHUB_API_VERSION_HEADER: &str = "X-GitHub-Api-Version";
const GITHUB_API_VERSION: &str = "2022-11-28";

pub(super) fn github_token_from_env() -> Option<String> {
    let gh_token = std::env::var("GH_TOKEN").ok();
    let github_token = std::env::var("GITHUB_TOKEN").ok();
    github_token_from_values([gh_token.as_deref(), github_token.as_deref()])
}

pub(super) fn github_token_from_values<'a>(
    values: impl IntoIterator<Item = Option<&'a str>>,
) -> Option<String> {
    values
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn apply_github_auth_with_token(
    builder: reqwest::RequestBuilder,
    token: Option<&str>,
) -> reqwest::RequestBuilder {
    match token.map(str::trim).filter(|value| !value.is_empty()) {
        Some(token) => builder.header(AUTHORIZATION, format!("Bearer {token}")),
        None => builder,
    }
}

pub(super) fn apply_github_api_headers(
    builder: reqwest::RequestBuilder,
) -> reqwest::RequestBuilder {
    let token = github_token_from_env();
    apply_github_api_headers_with_token(builder, token.as_deref())
}

fn apply_github_api_headers_with_token(
    builder: reqwest::RequestBuilder,
    token: Option<&str>,
) -> reqwest::RequestBuilder {
    apply_github_auth_with_token(builder, token)
        .header(ACCEPT, "application/vnd.github+json")
        .header(GITHUB_API_VERSION_HEADER, GITHUB_API_VERSION)
}

pub(super) fn apply_github_auth_for_url(
    builder: reqwest::RequestBuilder,
    url: &str,
) -> reqwest::RequestBuilder {
    let token = github_token_from_env();
    apply_github_auth_for_url_with_token(builder, url, token.as_deref())
}

pub(super) fn apply_github_auth_for_url_with_token(
    builder: reqwest::RequestBuilder,
    url: &str,
    token: Option<&str>,
) -> reqwest::RequestBuilder {
    if is_github_url(url) {
        apply_github_auth_with_token(builder, token)
    } else {
        builder
    }
}

fn is_github_url(url: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(url) else {
        return false;
    };
    matches!(
        url.host_str(),
        Some("api.github.com" | "github.com" | "raw.githubusercontent.com")
    )
}
