use crate::build::adapter::{BuildAdapter, PresetGroup};

use super::{FALLBACK_OFFICIAL_PRESET_REPO, PACKAGE_REPOSITORY_URL};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresetReference {
    OfficialAlias {
        name: String,
        commit: Option<String>,
    },
}

pub fn parse_preset_reference(value: &str) -> Result<PresetReference, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err("preset cannot be empty".to_string());
    }

    if trimmed.contains(':') {
        return Err(format!(
            "Invalid preset reference '{}'. GitHub preset references are not supported. Use an official alias like 'bun', 'js/tanstack-start', or 'js/tanstack-start@<commit-hash>'.",
            trimmed
        ));
    }

    let (without_at_commit, explicit_commit) = match trimmed.rsplit_once('@') {
        Some((name, commit)) => {
            let commit = commit.trim();
            if commit.is_empty() {
                return Err(format!(
                    "Invalid preset reference '{}': commit hash cannot be empty after '@'.",
                    trimmed
                ));
            }
            validate_commit_hash(trimmed, commit)?;
            (name.trim(), Some(commit.to_string()))
        }
        None => (trimmed, None),
    };

    let (name, commit) = match explicit_commit {
        Some(commit) => (without_at_commit.to_string(), Some(commit)),
        None => (without_at_commit.to_string(), None),
    };

    validate_official_alias(trimmed, &name)?;
    Ok(PresetReference::OfficialAlias { name, commit })
}

pub fn qualify_runtime_local_preset_ref(
    runtime: BuildAdapter,
    preset_ref: &str,
) -> Result<String, String> {
    let trimmed = preset_ref.trim();
    if trimmed.is_empty() {
        return Err("preset cannot be empty".to_string());
    }
    if trimmed.contains('/') {
        return Err(
            "preset must not include namespace (for example `js/tanstack-start`); set top-level `runtime` and use local preset name only."
                .to_string(),
        );
    }

    let preset_group = runtime.preset_group();
    if preset_group == PresetGroup::Unknown {
        return Err(format!(
            "Cannot resolve preset '{}' without a known runtime. Set top-level `runtime` explicitly.",
            trimmed
        ));
    }

    let (name, commit) = match trimmed.rsplit_once('@') {
        Some((name, commit)) if !name.trim().is_empty() && !commit.trim().is_empty() => {
            (name.trim(), Some(commit.trim()))
        }
        Some((_, commit)) if commit.trim().is_empty() => {
            return Err(format!(
                "Invalid preset reference '{}': commit hash cannot be empty after '@'.",
                trimmed
            ));
        }
        _ => (trimmed, None),
    };

    Ok(match commit {
        Some(commit) => format!("{}/{}@{}", preset_group.id(), name, commit),
        None => format!("{}/{}", preset_group.id(), name),
    })
}

fn validate_official_alias(raw_value: &str, alias: &str) -> Result<(), String> {
    if alias.is_empty() {
        return Err(format!(
            "Invalid preset alias '{}'. Alias is empty.",
            raw_value
        ));
    }
    let segments: Vec<&str> = alias.split('/').collect();
    if segments.len() > 2 {
        return Err(format!(
            "Invalid preset alias '{}'. Expected '<name>' or '<group>/<name>'.",
            raw_value
        ));
    }
    if segments.iter().any(|segment| segment.is_empty()) {
        return Err(format!(
            "Invalid preset alias '{}'. Alias segments cannot be empty.",
            raw_value
        ));
    }
    for segment in segments {
        if !segment
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-' || ch == '_')
        {
            return Err(format!(
                "Invalid preset alias '{}'. Alias must use lowercase letters, digits, '-' or '_' (with optional one '/').",
                raw_value
            ));
        }
    }
    Ok(())
}

fn validate_commit_hash(raw_value: &str, commit: &str) -> Result<(), String> {
    if commit.len() < 7 || commit.len() > 64 {
        return Err(format!(
            "Invalid preset reference '{}': commit hash '{}' must be 7-64 hexadecimal characters.",
            raw_value, commit
        ));
    }
    if !commit.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(format!(
            "Invalid preset reference '{}': commit hash '{}' must be hexadecimal.",
            raw_value, commit
        ));
    }
    Ok(())
}

pub(super) fn parse_github_repo_slug(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }

    let without_prefix = if let Some(rest) = trimmed.strip_prefix("https://github.com/") {
        rest
    } else if let Some(rest) = trimmed.strip_prefix("http://github.com/") {
        rest
    } else if let Some(rest) = trimmed.strip_prefix("git@github.com:") {
        rest
    } else {
        trimmed
    };

    let mut parts = without_prefix.trim_matches('/').split('/');
    let owner = parts.next()?.trim();
    let repo = parts.next()?.trim();
    if owner.is_empty() || repo.is_empty() {
        return None;
    }

    let normalized_repo = repo.strip_suffix(".git").unwrap_or(repo).trim();
    if normalized_repo.is_empty() {
        return None;
    }

    Some(format!("{owner}/{normalized_repo}"))
}

pub(super) fn official_preset_repo() -> String {
    parse_github_repo_slug(PACKAGE_REPOSITORY_URL)
        .unwrap_or_else(|| FALLBACK_OFFICIAL_PRESET_REPO.to_string())
}

pub fn infer_adapter_from_preset_reference(preset_ref: &str) -> BuildAdapter {
    let Ok(reference) = parse_preset_reference(preset_ref) else {
        return BuildAdapter::Unknown;
    };
    match reference {
        PresetReference::OfficialAlias { name, .. } => {
            infer_adapter_from_official_alias_name(&name)
        }
    }
}

fn infer_adapter_from_official_alias_name(alias: &str) -> BuildAdapter {
    let group_or_name = alias.split('/').next().unwrap_or(alias);
    BuildAdapter::from_id(group_or_name).unwrap_or(BuildAdapter::Unknown)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_github_repo_slug_accepts_https_ssh_and_slug_formats() {
        assert_eq!(
            parse_github_repo_slug("https://github.com/tako-sh/tako"),
            Some("tako-sh/tako".to_string())
        );
        assert_eq!(
            parse_github_repo_slug("git@github.com:tako-sh/tako.git"),
            Some("tako-sh/tako".to_string())
        );
        assert_eq!(
            parse_github_repo_slug("tako-sh/tako"),
            Some("tako-sh/tako".to_string())
        );
    }

    #[test]
    fn parse_github_repo_slug_rejects_invalid_values() {
        assert_eq!(parse_github_repo_slug(""), None);
        assert_eq!(parse_github_repo_slug("lilienblum"), None);
        assert_eq!(
            parse_github_repo_slug("https://example.com/tako-sh/tako"),
            None
        );
    }

    #[test]
    fn official_preset_repo_uses_package_repository_slug() {
        let expected = parse_github_repo_slug(PACKAGE_REPOSITORY_URL).unwrap();
        assert_eq!(official_preset_repo(), expected);
    }

    #[test]
    fn parse_preset_reference_accepts_official_alias() {
        let parsed = parse_preset_reference("bun").unwrap();
        assert_eq!(
            parsed,
            PresetReference::OfficialAlias {
                name: "bun".to_string(),
                commit: None,
            }
        );
    }

    #[test]
    fn parse_preset_reference_accepts_official_alias_with_commit() {
        let parsed = parse_preset_reference("bun@abc1234").unwrap();
        assert_eq!(
            parsed,
            PresetReference::OfficialAlias {
                name: "bun".to_string(),
                commit: Some("abc1234".to_string()),
            }
        );
    }

    #[test]
    fn parse_preset_reference_accepts_namespaced_official_alias() {
        let parsed = parse_preset_reference("javascript/tanstack-start").unwrap();
        assert_eq!(
            parsed,
            PresetReference::OfficialAlias {
                name: "javascript/tanstack-start".to_string(),
                commit: None,
            }
        );
    }

    #[test]
    fn parse_preset_reference_accepts_namespaced_official_alias_with_commit() {
        let parsed = parse_preset_reference("js/tanstack-start@abc1234").unwrap();
        assert_eq!(
            parsed,
            PresetReference::OfficialAlias {
                name: "js/tanstack-start".to_string(),
                commit: Some("abc1234".to_string()),
            }
        );
    }

    #[test]
    fn parse_preset_reference_rejects_invalid_values() {
        assert!(parse_preset_reference("").is_err());
        assert!(parse_preset_reference("github:owner/repo").is_err());
        assert!(parse_preset_reference("github:owner/repo/path.jsonc").is_err());
        assert!(parse_preset_reference("github:owner/repo/path.toml").is_err());
        assert!(parse_preset_reference("bun/abc12345/extra").is_err());
        assert!(parse_preset_reference("bun@").is_err());
        assert!(parse_preset_reference("js/tanstack-start@").is_err());
        assert!(parse_preset_reference("Bun").is_err());
    }

    #[test]
    fn infer_adapter_from_preset_reference_supports_official_aliases() {
        assert_eq!(
            infer_adapter_from_preset_reference("bun"),
            BuildAdapter::Bun
        );
        assert_eq!(
            infer_adapter_from_preset_reference("javascript/tanstack-start"),
            BuildAdapter::Unknown
        );
        assert_eq!(
            infer_adapter_from_preset_reference("node"),
            BuildAdapter::Node
        );
        assert_eq!(
            infer_adapter_from_preset_reference("python"),
            BuildAdapter::Unknown
        );
        assert_eq!(
            infer_adapter_from_preset_reference("github:owner/repo/presets/custom.toml"),
            BuildAdapter::Unknown
        );
        assert_eq!(
            infer_adapter_from_preset_reference("bun-tanstack-start"),
            BuildAdapter::Unknown
        );
    }
}
