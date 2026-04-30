use super::ws::is_websocket_auth_payload;
use super::*;

#[test]
fn extract_credentials_picks_declared_header_and_cookie() {
    let mut headers = HashMap::new();
    headers.insert("authorization".into(), "Bearer abc".into());
    headers.insert("cookie".into(), "session=xyz; other=ignored".into());

    let scheme = ChannelAuthScheme::Required {
        header_name: Some("authorization".into()),
        cookie_name: Some("session".into()),
    };

    let (header, cookie) = extract_credentials(&headers, &scheme);
    assert_eq!(
        header,
        Some(ChannelHeaderValue {
            scheme: Some("Bearer".into()),
            value: "abc".into()
        })
    );
    assert_eq!(cookie, Some("xyz".to_string()));
}

#[test]
fn extract_credentials_normalizes_header_name_lookup() {
    let mut headers = HashMap::new();
    headers.insert("x-session-token".into(), "plain-token".into());

    let scheme = ChannelAuthScheme::Required {
        header_name: Some("X-Session-Token".into()),
        cookie_name: None,
    };

    let (header, cookie) = extract_credentials(&headers, &scheme);
    assert_eq!(
        header,
        Some(ChannelHeaderValue {
            scheme: None,
            value: "plain-token".into()
        })
    );
    assert!(cookie.is_none());
}

#[test]
fn extract_credentials_returns_none_for_public_channels() {
    let mut headers = HashMap::new();
    headers.insert("authorization".into(), "Bearer abc".into());

    let (header, cookie) = extract_credentials(&headers, &ChannelAuthScheme::Public);
    assert!(header.is_none());
    assert!(cookie.is_none());
}

#[test]
fn channel_params_query_excludes_ws_cursor() {
    assert_eq!(
        channel_params_query(Some("roomId=r1&last_message_id=42&limit=10")),
        "roomId=r1&limit=10"
    );
    assert_eq!(channel_params_query(Some("last_message_id=42")), "");
    assert_eq!(channel_params_query(None), "");
}

#[test]
fn auth_scheme_requires_declared_credentials_only_for_declared_fields() {
    assert!(auth_scheme_requires_declared_credentials(
        &ChannelAuthScheme::Required {
            header_name: Some("authorization".into()),
            cookie_name: None,
        },
    ));
    assert!(auth_scheme_requires_declared_credentials(
        &ChannelAuthScheme::Required {
            header_name: None,
            cookie_name: Some("session".into()),
        },
    ));
    assert!(!auth_scheme_requires_declared_credentials(
        &ChannelAuthScheme::Required {
            header_name: None,
            cookie_name: None,
        },
    ));
    assert!(!auth_scheme_requires_declared_credentials(
        &ChannelAuthScheme::Public,
    ));
}

#[test]
fn auth_scheme_requires_header_only_for_declared_header_auth() {
    assert!(auth_scheme_requires_header(&ChannelAuthScheme::Required {
        header_name: Some("authorization".into()),
        cookie_name: None,
    }));
    assert!(!auth_scheme_requires_header(&ChannelAuthScheme::Required {
        header_name: None,
        cookie_name: Some("session".into()),
    }));
    assert!(!auth_scheme_requires_header(&ChannelAuthScheme::Public));
}

#[test]
fn websocket_auth_payload_is_reserved_and_ignored_after_auth() {
    assert!(is_websocket_auth_payload(
        br#"{"type":"tako.auth","token":"Bearer abc"}"#
    ));
    assert!(!is_websocket_auth_payload(
        br#"{"type":"chat.send","data":{"text":"hi"}}"#
    ));
}
