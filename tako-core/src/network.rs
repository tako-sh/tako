//! Calls to app-owned primitives. Caller identity stays on the socket envelope;
//! addresses select a destination within the caller's deployment environment.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const NETWORK_HTTP_PATH: &str = "/_tako/network";
pub const NETWORK_INTERNAL_PATH: &str = "/network";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum NetworkRequest {
    Enqueue {
        payload: Value,
        #[serde(default)]
        opts: crate::EnqueueOpts,
    },
    Publish {
        message: Value,
        #[serde(default = "empty_params")]
        params: Value,
    },
}

fn empty_params() -> Value {
    serde_json::json!({})
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkInvocation {
    pub name: String,
    pub request: NetworkRequest,
}

pub fn valid_primitive_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-:".contains(&c))
}

/// No caller-selected environment, path, URL, or arbitrary host is accepted.
pub fn resolve_address(caller: &str, address: &str) -> Result<(String, String), &'static str> {
    let (namespace, name) = match address.split_once('.') {
        Some((namespace, name)) => (Some(namespace), name),
        None => (None, address),
    };
    if !valid_primitive_name(name) {
        return Err("invalid primitive name");
    }
    let Some(namespace) = namespace else {
        return Ok((caller.to_string(), name.to_string()));
    };
    if namespace.is_empty()
        || namespace.len() > 63
        || !namespace.as_bytes()[0].is_ascii_lowercase()
        || !namespace
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        || namespace.ends_with('-')
    {
        return Err("invalid app namespace");
    }
    let target = match crate::split_deployment_app_id(caller) {
        Some((_, environment)) => crate::deployment_app_id(namespace, environment),
        None => namespace.to_owned(),
    };
    Ok((target, name.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_keep_the_callers_environment() {
        assert_eq!(
            resolve_address("web/staging", "mail.send-email").unwrap(),
            ("mail/staging".into(), "send-email".into())
        );
        assert_eq!(
            resolve_address("web", "mail.progress").unwrap(),
            ("mail".into(), "progress".into())
        );
        assert_eq!(
            resolve_address("web/staging", "send-email").unwrap(),
            ("web/staging".into(), "send-email".into())
        );
        for invalid in [
            "",
            ".x",
            "mail.",
            "mail/production.x",
            "mail.x.y",
            "https://mail/x",
            "mail../x",
        ] {
            assert!(
                resolve_address("web/staging", invalid).is_err(),
                "{invalid}"
            );
        }
    }
}
