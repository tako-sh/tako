use reqwest::header::{ACCEPT, AUTHORIZATION};

const GITHUB_API_VERSION_HEADER: &str = "X-GitHub-Api-Version";
const GITHUB_API_VERSION: &str = "2022-11-28";

pub(super) fn github_token_from_env() -> Option<String> {
    ["GH_TOKEN", "GITHUB_TOKEN"]
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .map(|value| value.trim().to_string())
        .find(|value| !value.is_empty())
}

fn apply_github_auth(builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    match github_token_from_env() {
        Some(token) => builder.header(AUTHORIZATION, format!("Bearer {token}")),
        None => builder,
    }
}

pub(super) fn apply_github_api_headers(
    builder: reqwest::RequestBuilder,
) -> reqwest::RequestBuilder {
    apply_github_auth(builder)
        .header(ACCEPT, "application/vnd.github+json")
        .header(GITHUB_API_VERSION_HEADER, GITHUB_API_VERSION)
}

pub(super) fn apply_github_auth_for_url(
    builder: reqwest::RequestBuilder,
    url: &str,
) -> reqwest::RequestBuilder {
    if is_github_url(url) {
        apply_github_auth(builder)
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
