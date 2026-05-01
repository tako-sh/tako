use reqwest::header::{ACCEPT, AUTHORIZATION};

const API_VERSION_HEADER: &str = "X-GitHub-Api-Version";
const API_VERSION: &str = "2022-11-28";
const GH_TOKEN_ENV: &str = "GH_TOKEN";
const GITHUB_TOKEN_ENV: &str = "GITHUB_TOKEN";

pub(crate) fn token_from_env() -> Option<String> {
    [GH_TOKEN_ENV, GITHUB_TOKEN_ENV]
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .map(|value| value.trim().to_string())
        .find(|value| !value.is_empty())
}

pub(crate) fn apply_auth(builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    match token_from_env() {
        Some(token) => builder.header(AUTHORIZATION, format!("Bearer {token}")),
        None => builder,
    }
}

pub(crate) fn apply_api_headers(builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    apply_auth(builder)
        .header(ACCEPT, "application/vnd.github+json")
        .header(API_VERSION_HEADER, API_VERSION)
}

pub(crate) fn apply_auth_for_url(
    builder: reqwest::RequestBuilder,
    url: &str,
) -> reqwest::RequestBuilder {
    if is_github_url(url) {
        apply_auth(builder)
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

    fn preserve_token_envs() -> (Option<std::ffi::OsString>, Option<std::ffi::OsString>) {
        (
            std::env::var_os(GH_TOKEN_ENV),
            std::env::var_os(GITHUB_TOKEN_ENV),
        )
    }

    fn restore_token_envs(previous: (Option<std::ffi::OsString>, Option<std::ffi::OsString>)) {
        match previous.0 {
            Some(value) => unsafe { std::env::set_var(GH_TOKEN_ENV, value) },
            None => unsafe { std::env::remove_var(GH_TOKEN_ENV) },
        }
        match previous.1 {
            Some(value) => unsafe { std::env::set_var(GITHUB_TOKEN_ENV, value) },
            None => unsafe { std::env::remove_var(GITHUB_TOKEN_ENV) },
        }
    }

    #[test]
    fn token_from_env_prefers_gh_token_over_github_token() {
        let _lock = crate::paths::test_tako_home_env_lock();
        let previous = preserve_token_envs();
        unsafe {
            std::env::set_var(GH_TOKEN_ENV, "gh-token");
            std::env::set_var(GITHUB_TOKEN_ENV, "github-token");
        }

        let token = token_from_env();

        restore_token_envs(previous);
        assert_eq!(token.as_deref(), Some("gh-token"));
    }

    #[test]
    fn token_from_env_falls_back_when_gh_token_is_empty() {
        let _lock = crate::paths::test_tako_home_env_lock();
        let previous = preserve_token_envs();
        unsafe {
            std::env::set_var(GH_TOKEN_ENV, " ");
            std::env::set_var(GITHUB_TOKEN_ENV, "github-token");
        }

        let token = token_from_env();

        restore_token_envs(previous);
        assert_eq!(token.as_deref(), Some("github-token"));
    }

    #[test]
    fn apply_api_headers_sets_auth_and_github_headers() {
        let _lock = crate::paths::test_tako_home_env_lock();
        let previous = preserve_token_envs();
        unsafe {
            std::env::set_var(GH_TOKEN_ENV, "secret");
            std::env::remove_var(GITHUB_TOKEN_ENV);
        }

        let request =
            apply_api_headers(reqwest::Client::new().get("https://api.github.com/rate_limit"))
                .build()
                .unwrap();

        restore_token_envs(previous);
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
        let _lock = crate::paths::test_tako_home_env_lock();
        let previous = preserve_token_envs();
        unsafe {
            std::env::set_var(GH_TOKEN_ENV, "secret");
        }

        let request = apply_auth_for_url(
            reqwest::Client::new().get("https://downloads.example.com/tako.tar.gz"),
            "https://downloads.example.com/tako.tar.gz",
        )
        .build()
        .unwrap();

        restore_token_envs(previous);
        assert!(request.headers().get(AUTHORIZATION).is_none());
    }
}
