use super::*;

#[test]
fn request_is_cacheable_for_get_and_head_without_upgrade() {
    let get = RequestHeader::build("GET", b"/assets/app.js", None).expect("build request");
    let head = RequestHeader::build("HEAD", b"/assets/app.js", None).expect("build request");

    assert!(request_is_proxy_cacheable(&get));
    assert!(request_is_proxy_cacheable(&head));
}

#[test]
fn request_is_not_cacheable_for_upgrade_or_non_get_head_methods() {
    let mut post = RequestHeader::build("POST", b"/assets/app.js", None).expect("build request");
    let mut get_upgrade = RequestHeader::build("GET", b"/socket", None).expect("build request");
    get_upgrade
        .insert_header("Upgrade", "websocket")
        .expect("insert upgrade");
    post.insert_header("Content-Type", "application/json")
        .expect("insert content type");

    assert!(!request_is_proxy_cacheable(&post));
    assert!(!request_is_proxy_cacheable(&get_upgrade));
}

#[test]
fn cache_key_includes_host_and_uri() {
    let a = build_proxy_cache_key("app-a.example.com", "/assets/app.js?v=1");
    let b = build_proxy_cache_key("app-b.example.com", "/assets/app.js?v=1");
    let c = build_proxy_cache_key("app-a.example.com", "/assets/app.js?v=2");

    assert_ne!(a.to_compact().primary, b.to_compact().primary);
    assert_ne!(a.to_compact().primary, c.to_compact().primary);
}

#[test]
fn response_cacheability_requires_explicit_cache_directives() {
    let mut without_directive = ResponseHeader::build(200, Some(1)).expect("build response header");
    without_directive
        .insert_header("Content-Type", "text/plain")
        .expect("insert content type");

    let mut with_max_age = ResponseHeader::build(200, Some(2)).expect("build response header");
    with_max_age
        .insert_header("Content-Type", "text/plain")
        .expect("insert content type");
    with_max_age
        .insert_header("Cache-Control", "public, max-age=60")
        .expect("insert cache control");

    assert!(matches!(
        response_cacheability(&without_directive, false),
        pingora_cache::RespCacheable::Uncacheable(_)
    ));
    assert!(matches!(
        response_cacheability(&with_max_age, false),
        pingora_cache::RespCacheable::Cacheable(_)
    ));
}
