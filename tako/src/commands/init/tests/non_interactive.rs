use crate::build::BuildAdapter;
use crate::config::TakoToml;

use super::super::non_interactive::resolve_adapter;

#[test]
fn resolve_adapter_uses_existing_config_runtime() {
    let existing = TakoToml {
        runtime: Some("node".to_string()),
        ..Default::default()
    };
    assert_eq!(
        resolve_adapter(BuildAdapter::Bun, Some(&existing)),
        BuildAdapter::Node
    );
}

#[test]
fn resolve_adapter_defaults_unknown_detection_to_bun() {
    assert_eq!(
        resolve_adapter(BuildAdapter::Unknown, None),
        BuildAdapter::Bun
    );
}
