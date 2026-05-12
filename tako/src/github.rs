use reqwest::header::{ACCEPT, AUTHORIZATION};

const API_VERSION_HEADER: &str = "X-GitHub-Api-Version";
const API_VERSION: &str = "2022-11-28";
const GH_TOKEN_ENV: &str = "GH_TOKEN";
const GITHUB_TOKEN_ENV: &str = "GITHUB_TOKEN";

pub(crate) fn token_from_env() -> Option<String> {
    let gh_token = std::env::var(GH_TOKEN_ENV).ok();
    let github_token = std::env::var(GITHUB_TOKEN_ENV).ok();
    token_from_values([gh_token.as_deref(), github_token.as_deref()])
}

fn token_from_values<'a>(values: impl IntoIterator<Item = Option<&'a str>>) -> Option<String> {
    values
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

pub(crate) fn apply_auth(builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    let token = token_from_env();
    apply_auth_with_token(builder, token.as_deref())
}

fn apply_auth_with_token(
    builder: reqwest::RequestBuilder,
    token: Option<&str>,
) -> reqwest::RequestBuilder {
    match token.map(str::trim).filter(|value| !value.is_empty()) {
        Some(token) => builder.header(AUTHORIZATION, format!("Bearer {token}")),
        None => builder,
    }
}

pub(crate) fn apply_api_headers(builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    let token = token_from_env();
    apply_api_headers_with_token(builder, token.as_deref())
}

fn apply_api_headers_with_token(
    builder: reqwest::RequestBuilder,
    token: Option<&str>,
) -> reqwest::RequestBuilder {
    apply_auth_with_token(builder, token)
        .header(ACCEPT, "application/vnd.github+json")
        .header(API_VERSION_HEADER, API_VERSION)
}

pub(crate) fn apply_auth_for_url(
    builder: reqwest::RequestBuilder,
    url: &str,
) -> reqwest::RequestBuilder {
    let token = token_from_env();
    apply_auth_for_url_with_token(builder, url, token.as_deref())
}

fn apply_auth_for_url_with_token(
    builder: reqwest::RequestBuilder,
    url: &str,
    token: Option<&str>,
) -> reqwest::RequestBuilder {
    if is_github_url(url) {
        apply_auth_with_token(builder, token)
    } else {
        builder
    }
}

pub(crate) fn is_github_url(url: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(url) else {
        return false;
    };
    matches!(
        url.host_str(),
        Some("api.github.com" | "github.com" | "raw.githubusercontent.com")
    )
}

pub(crate) fn remote_curl_auth_header_script(url_var: &str) -> String {
    format!(
        "auth_header=''; \
         case \"${{{url_var}}}\" in \
           https://github.com/*|https://api.github.com/*|https://raw.githubusercontent.com/*) \
             github_token=\"${{GH_TOKEN:-${{GITHUB_TOKEN:-}}}}\"; \
             if [ -n \"$github_token\" ]; then auth_header=\"Authorization: Bearer $github_token\"; fi; \
             ;; \
         esac"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    #[test]
    fn token_from_env_prefers_gh_token_over_github_token() {
        let token = token_from_values([Some("gh-token"), Some("github-token")]);
        assert_eq!(token.as_deref(), Some("gh-token"));
    }

    #[test]
    fn token_from_env_falls_back_when_gh_token_is_empty() {
        let token = token_from_values([Some(" "), Some("github-token")]);
        assert_eq!(token.as_deref(), Some("github-token"));
    }

    #[test]
    fn apply_api_headers_sets_auth_and_github_headers() {
        let request = apply_api_headers_with_token(
            reqwest::Client::new().get("https://api.github.com/rate_limit"),
            Some("secret"),
        )
        .build()
        .unwrap();

        assert_eq!(
            request.headers().get(AUTHORIZATION).unwrap(),
            HeaderValue::from_static("Bearer secret")
        );
        assert_eq!(
            request.headers().get(ACCEPT).unwrap(),
            HeaderValue::from_static("application/vnd.github+json")
        );
        assert_eq!(
            request.headers().get(API_VERSION_HEADER).unwrap(),
            HeaderValue::from_static(API_VERSION)
        );
    }

    #[test]
    fn apply_auth_for_url_does_not_authenticate_non_github_urls() {
        let request = apply_auth_for_url_with_token(
            reqwest::Client::new().get("https://downloads.example.com/tako.tar.gz"),
            "https://downloads.example.com/tako.tar.gz",
            Some("secret"),
        )
        .build()
        .unwrap();

        assert!(request.headers().get(AUTHORIZATION).is_none());
    }
}
