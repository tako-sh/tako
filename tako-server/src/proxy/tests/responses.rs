use super::*;

#[test]
fn production_error_bodies_are_generic_reason_phrases() {
    assert_eq!(production_error_body(500), "Internal Server Error");
    assert_eq!(production_error_body(502), "Bad Gateway");
    assert_eq!(production_error_body(503), "Service Unavailable");
    assert_eq!(production_error_body(504), "Gateway Timeout");
}

#[test]
fn body_headers_include_content_type_and_length() {
    let mut header = ResponseHeader::build(404, None).expect("build header");
    insert_body_headers(&mut header, "text/plain", "Not Found").expect("insert headers");

    assert_eq!(
        header
            .headers
            .get("Content-Type")
            .and_then(|v| v.to_str().ok()),
        Some("text/plain")
    );
    assert_eq!(
        header
            .headers
            .get("Content-Length")
            .and_then(|v| v.to_str().ok()),
        Some("9")
    );
}

#[test]
fn body_headers_use_utf8_byte_length() {
    let mut header = ResponseHeader::build(200, None).expect("build header");
    insert_body_headers(&mut header, "text/plain", "✓").expect("insert headers");

    assert_eq!(
        header
            .headers
            .get("Content-Length")
            .and_then(|v| v.to_str().ok()),
        Some("3")
    );
}
