use super::*;

pub(super) fn request_params_query(query: Option<&str>, is_store: bool) -> String {
    if is_store {
        query.unwrap_or_default().to_owned()
    } else {
        channel_params_query(query)
    }
}

/// Cookie credentials are ambient browser authority. Require an explicit,
/// same-origin handshake before forwarding them to the app's auth callback.
pub(super) fn websocket_origin_matches(request: &RequestHeader, is_https: bool) -> bool {
    let mut origins = request.headers.get_all("origin").iter();
    let Some(origin) = origins.next().and_then(|value| value.to_str().ok()) else {
        return false;
    };
    if origins.next().is_some() {
        return false;
    }
    let Ok(origin) = reqwest::Url::parse(origin) else {
        return false;
    };
    if !matches!(origin.scheme(), "http" | "https")
        || !origin.username().is_empty()
        || origin.password().is_some()
        || origin.path() != "/"
        || origin.query().is_some()
        || origin.fragment().is_some()
    {
        return false;
    }
    let scheme = if is_https { "https" } else { "http" };
    let host = crate::proxy::request::request_host(request);
    let Ok(expected) = reqwest::Url::parse(&format!("{scheme}://{host}")) else {
        return false;
    };
    origin.origin() == expected.origin()
}

pub(super) use tako_channels::params::channel_params_query;

pub(super) fn auth_scheme_requires_declared_credentials(scheme: &ChannelAuthScheme) -> bool {
    matches!(
        scheme,
        ChannelAuthScheme::Required {
            header_name: Some(_),
            cookie_name: _
        } | ChannelAuthScheme::Required {
            header_name: _,
            cookie_name: Some(_)
        }
    )
}

pub(super) fn auth_scheme_requires_header(scheme: &ChannelAuthScheme) -> bool {
    matches!(
        scheme,
        ChannelAuthScheme::Required {
            header_name: Some(_),
            ..
        }
    )
}

pub(super) fn request_headers_to_map(request: &RequestHeader) -> HashMap<String, String> {
    request
        .headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_ascii_lowercase(), value.to_string()))
        })
        .collect()
}

pub(crate) fn extract_credentials(
    headers: &HashMap<String, String>,
    scheme: &ChannelAuthScheme,
) -> (Option<ChannelHeaderValue>, Option<String>) {
    let ChannelAuthScheme::Required {
        header_name,
        cookie_name,
    } = scheme
    else {
        return (None, None);
    };

    let header = header_name
        .as_ref()
        .and_then(|name| headers.get(&name.to_ascii_lowercase()))
        .map(|raw| ChannelHeaderValue::parse(raw));

    let cookie = cookie_name.as_ref().and_then(|name| {
        let raw = headers.get("cookie")?;
        raw.split(';').find_map(|pair| {
            let (key, value) = pair.trim().split_once('=')?;
            (key == name).then(|| value.to_string())
        })
    });

    (header, cookie)
}
