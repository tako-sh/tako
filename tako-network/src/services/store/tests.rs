use super::*;

#[test]
fn query_framing_is_bounded_and_requires_complete_response() {
    let valid = b"{\"columns\":[\"server_id\",\"payload\"]}\n{\"row\":[0,[\"server\",\"payload\"]]}\n{\"eoq\":{}}";
    assert_eq!(
        service_rows(valid).unwrap(),
        vec![(json!("server"), json!("payload"))]
    );
    assert!(service_rows(&valid[..valid.len() - 10]).is_err());
    let extra = format!("{}\n{{\"eoq\":{{}}}}", std::str::from_utf8(valid).unwrap());
    assert!(service_rows(extra.as_bytes()).is_err());
    let mut oversized = String::from("{\"columns\":[\"server_id\",\"payload\"]}\n");
    for n in 0..33 {
        oversized.push_str(&format!("{{\"row\":[{n},[\"s\",\"p\"]]}}\n"));
    }
    oversized.push_str("{\"eoq\":{}}");
    assert!(service_rows(oversized.as_bytes()).is_err());
}
