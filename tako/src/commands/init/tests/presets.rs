use crate::build::{BuildAdapter, PresetDefinition};

use super::super::presets::{build_preset_selection_options, normalize_group_preset_definitions};

#[test]
fn normalize_group_preset_names_excludes_base_and_deduplicates() {
    let names = normalize_group_preset_definitions(
        BuildAdapter::Bun,
        vec![
            PresetDefinition {
                name: "".to_string(),
                main: None,
            },
            PresetDefinition {
                name: "bun".to_string(),
                main: None,
            },
            PresetDefinition {
                name: " tanstack-start ".to_string(),
                main: Some("dist/server/tako-entry.mjs".to_string()),
            },
            PresetDefinition {
                name: "tanstack-start".to_string(),
                main: Some("dist/server/ignored.mjs".to_string()),
            },
            PresetDefinition {
                name: "custom".to_string(),
                main: None,
            },
        ],
    );
    assert_eq!(
        names,
        vec![
            PresetDefinition {
                name: "tanstack-start".to_string(),
                main: Some("dist/server/tako-entry.mjs".to_string()),
            },
            PresetDefinition {
                name: "custom".to_string(),
                main: None,
            },
        ]
    );
}

#[test]
fn build_preset_selection_options_returns_none_when_no_group_presets_found() {
    let options = build_preset_selection_options(BuildAdapter::Bun, &[]);
    assert!(options.is_none());
}

#[test]
fn build_preset_selection_options_includes_presets_and_custom_mode() {
    let options = build_preset_selection_options(
        BuildAdapter::Node,
        &["tanstack-start".to_string(), "nextjs".to_string()],
    )
    .expect("options should be available");

    assert_eq!(options.len(), 3);
    assert_eq!(
        options[0],
        (
            "tanstack-start".to_string(),
            Some("tanstack-start".to_string())
        )
    );
    assert_eq!(
        options[1],
        ("nextjs".to_string(), Some("nextjs".to_string()))
    );
    assert_eq!(options[2], ("custom".to_string(), None));
}
