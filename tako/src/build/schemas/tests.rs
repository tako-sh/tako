use super::*;
use serde_json::json;

fn sample() -> Manifest {
    let mut doc = Manifest::empty("mail".into());
    doc.definitions.insert("Node".into(), json!({"type":"object","properties":{"id":{"type":"string"},"note":{"anyOf":[{"type":"string"},{"type":"null"}]},"next":{"$ref":"#/$defs/Node"}},"required":["id"],"additionalProperties":false}));
    doc.tako.primitives.push(Primitive::Handler {name:"lookup".into(),input:json!({"$ref":"#/$defs/Node"}),result:json!({"type":"object","properties":{"ok":{"const":true}},"required":["ok"],"additionalProperties":false})});
    doc
}

#[test]
fn native_bindings_preserve_recursive_optional_nullable_fields() {
    let docs = [sample()];
    let go = golang(&docs, "main").unwrap();
    assert!(go.contains("Note tako.Optional[*string]"));
    assert!(go.contains("Next tako.Optional[*MailSchemaNode]"));
    assert!(go.contains("MailLookupHandlerAddress"));
    let rs = rust(&docs).unwrap();
    assert!(rs.contains("tako::network::Optional<Option<String>>"));
    assert!(rs.contains("tako::network::Optional<Box<MailSchemaNode>>"));
    assert!(rs.contains("MAIL_LOOKUP_HANDLER_ADDRESS"));
}
#[test]
fn typescript_is_standalone_and_quotes_schema_strings() {
    let mut doc = sample();
    doc.definitions.insert("Injection".into(),json!({"type":"string","x-tako-template":{"texts":["` ${evil}",""],"types":[{"type":"string"}]}}));
    let output = typescript(&[doc]).unwrap();
    assert!(output.contains("\\` \\${evil}${string}"));
    assert!(output.contains("readonly address: \"mail.lookup\""));
    assert!(!output.contains("import("));
}
#[test]
fn local_sources_are_relative_and_remote_lock_is_verified_offline() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("mail.json"),
        serde_json::to_vec(&sample()).unwrap(),
    )
    .unwrap();
    assert_eq!(
        resolve(dir.path(), &["mail.json".into()], false)
            .unwrap()
            .manifests[0]
            .tako
            .app,
        "mail"
    );
    let lock = json!({"version":1,"schemas":{"https://invalid.invalid/mail.json":{"sha256":"wrong","schema":sample()}}});
    std::fs::write(dir.path().join("tako-schema.lock"), lock.to_string()).unwrap();
    assert!(
        resolve(
            dir.path(),
            &["https://invalid.invalid/mail.json".into()],
            false
        )
        .err()
        .unwrap()
        .to_string()
        .contains("hash mismatch")
    );
}
#[test]
fn unsupported_native_types_fail_instead_of_losing_type_information() {
    let mut doc = sample();
    doc.definitions
        .insert("Node".into(), json!({"type":"integer","format":"int64"}));
    assert!(golang(&[doc.clone()], "main").is_err());
    assert!(rust(&[doc.clone()]).is_err());
    assert!(typescript(&[doc]).is_err());
}

#[test]
fn recursive_aliases_need_a_nominal_boundary() {
    let mut doc = sample();
    doc.definitions.insert(
        "Node".into(),
        json!({"type":"array","items":{"$ref":"#/$defs/Node"}}),
    );
    assert!(golang(&[doc.clone()], "main").is_err());
    assert!(rust(&[doc.clone()]).is_err());
    doc.definitions
        .insert("Node".into(), json!({"$ref":"#/$defs/Object"}));
    doc.definitions.insert("Object".into(),json!({"type":"object","properties":{"child":{"$ref":"#/$defs/Node"}},"additionalProperties":false}));
    assert!(
        golang(&[doc.clone()], "main")
            .unwrap()
            .contains("type MailSchemaNode = MailSchemaObject")
    );
    assert!(
        rust(&[doc])
            .unwrap()
            .contains("pub type MailSchemaNode = MailSchemaObject")
    );
}
