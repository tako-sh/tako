pub(super) fn request_params_query(query: Option<&str>, is_store: bool) -> String {
    if is_store {
        query.unwrap_or_default().to_owned()
    } else {
        channel_params_query(query)
    }
}

pub(super) use tako_channels::params::channel_params_query;

#[cfg(test)]
pub(super) use tako_channels::authorize::{
    auth_scheme_requires_declared_credentials, auth_scheme_requires_header, extract_credentials,
    websocket_origin_matches,
};
