use std::fs;
use std::path::Path;

use crate::build::adapter::PresetGroup;
use crate::build::preset_cache;

use super::reference::official_preset_repo;
use super::{
    BuildPreset, PresetDefinition, PresetReference, ResolvedPresetSource,
    embedded_group_manifest_content, official_alias_to_path, official_group_manifest_path,
    parse_preset_reference, parse_resolved_preset_from_content,
};

pub(super) const OFFICIAL_PRESET_BRANCH: &str = "master";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PresetResolveMode {
    Deploy,
    Dev,
}

pub async fn load_build_preset(
    project_dir: &Path,
    preset_ref: &str,
) -> Result<(BuildPreset, ResolvedPresetSource), String> {
    load_build_preset_with_mode(project_dir, preset_ref, PresetResolveMode::Deploy).await
}

pub async fn load_dev_build_preset(
    project_dir: &Path,
    preset_ref: &str,
) -> Result<(BuildPreset, ResolvedPresetSource), String> {
    load_build_preset_with_mode(project_dir, preset_ref, PresetResolveMode::Dev).await
}

async fn load_build_preset_with_mode(
    project_dir: &Path,
    preset_ref: &str,
    mode: PresetResolveMode,
) -> Result<(BuildPreset, ResolvedPresetSource), String> {
    let parsed_ref = parse_preset_reference(preset_ref)?;

    let (alias, commit_override) = match &parsed_ref {
        PresetReference::OfficialAlias { name, commit } => (name.as_str(), commit.clone()),
    };
    let path = official_alias_to_path(alias);
    let official_repo = official_preset_repo();

    let (repo, commit, content) = if let Some(commit) = commit_override {
        resolve_by_commit(&official_repo, &path, &commit).await?
    } else {
        resolve_by_branch(&official_repo, &path, OFFICIAL_PRESET_BRANCH, mode).await?
    };

    let preset = parse_resolved_preset_from_content(&parsed_ref, &path, &content)?;
    let resolved = ResolvedPresetSource {
        preset_ref: preset_ref.to_string(),
        repo,
        path,
        commit: commit.clone(),
    };
    remove_legacy_build_lock(project_dir);
    Ok((preset, resolved))
}

async fn resolve_by_commit(
    repo: &str,
    path: &str,
    commit: &str,
) -> Result<(String, String, String), String> {
    if let Some(content) = preset_cache::read_cached(repo, commit, path) {
        return Ok((repo.to_string(), commit.to_string(), content));
    }

    match fetch_preset_content_by_commit(repo, path, commit).await {
        Ok(content) => {
            let _ = preset_cache::write_cached(repo, commit, path, &content);
            Ok((repo.to_string(), commit.to_string(), content))
        }
        Err(fetch_err) => {
            if let Some((_stale_sha, content)) = preset_cache::find_any_cached(repo, path) {
                tracing::warn!(
                    "Preset fetch failed for commit {}, using stale cache: {}",
                    commit,
                    fetch_err
                );
                Ok((repo.to_string(), commit.to_string(), content))
            } else {
                Err(fetch_err)
            }
        }
    }
}

async fn resolve_by_branch(
    repo: &str,
    path: &str,
    branch: &str,
    mode: PresetResolveMode,
) -> Result<(String, String, String), String> {
    // For the official preset repo in dev mode, the `include_str!`-baked
    // manifest is the ground truth for the binary at its build commit.
    // Prefer it over any cached content written at an earlier state of the
    // same commit SHA — works in both debug and release builds.
    if mode == PresetResolveMode::Dev
        && repo == official_preset_repo()
        && let Some(content) = embedded_group_manifest_content(path)
    {
        return Ok((
            repo.to_string(),
            "embedded".to_string(),
            content.to_string(),
        ));
    }

    if let Some(sha) = preset_cache::fresh_sha(repo, branch)
        && let Some(content) = preset_cache::read_cached(repo, &sha, path)
    {
        return Ok((repo.to_string(), sha, content));
    }

    if mode == PresetResolveMode::Dev {
        if let Some(sha) = preset_cache::last_known_sha(repo, branch)
            && let Some(content) = preset_cache::read_cached(repo, &sha, path)
        {
            return Ok((repo.to_string(), sha, content));
        }
        if let Some((sha, content)) = preset_cache::find_any_cached(repo, path) {
            return Ok((repo.to_string(), sha, content));
        }
        if let Some(content) = embedded_group_manifest_content(path) {
            return Ok((
                repo.to_string(),
                "embedded".to_string(),
                content.to_string(),
            ));
        }
    }

    match fetch_preset_content_from_master_branch(repo, path).await {
        Ok((sha, content)) => {
            let _ = preset_cache::write_cached(repo, &sha, path, &content);
            let _ = preset_cache::update_freshness(repo, branch, &sha);
            Ok((repo.to_string(), sha, content))
        }
        Err(fetch_err) => {
            if let Some(sha) = preset_cache::last_known_sha(repo, branch)
                && let Some(content) = preset_cache::read_cached(repo, &sha, path)
            {
                tracing::warn!("Preset fetch failed, using stale cache: {}", fetch_err);
                return Ok((repo.to_string(), sha, content));
            }
            if let Some((sha, content)) = preset_cache::find_any_cached(repo, path) {
                tracing::warn!("Preset fetch failed, using stale cache: {}", fetch_err);
                return Ok((repo.to_string(), sha, content));
            }
            if let Some(content) = embedded_group_manifest_content(path) {
                tracing::warn!(
                    "Preset fetch failed, using embedded group manifest for {}: {}",
                    path,
                    fetch_err
                );
                return Ok((
                    repo.to_string(),
                    "embedded".to_string(),
                    content.to_string(),
                ));
            }
            Err(fetch_err)
        }
    }
}

pub async fn load_available_group_preset_definitions(
    group: PresetGroup,
) -> Result<Vec<PresetDefinition>, String> {
    let Some(path) = official_group_manifest_path(group) else {
        return Err(format!(
            "Preset group '{}' is not supported for preset listing.",
            group.id()
        ));
    };

    let official_repo = official_preset_repo();
    let (_repo, _commit, content) = resolve_by_branch(
        &official_repo,
        path,
        OFFICIAL_PRESET_BRANCH,
        PresetResolveMode::Deploy,
    )
    .await?;
    super::parse_group_manifest_preset_definitions(path, &content)
}

pub async fn load_available_group_presets(group: PresetGroup) -> Result<Vec<String>, String> {
    let Some(path) = official_group_manifest_path(group) else {
        return Err(format!(
            "Preset group '{}' is not supported for preset listing.",
            group.id()
        ));
    };
    let official_repo = official_preset_repo();
    let (_repo, _commit, content) = resolve_by_branch(
        &official_repo,
        path,
        OFFICIAL_PRESET_BRANCH,
        PresetResolveMode::Deploy,
    )
    .await?;
    super::parse_group_manifest_preset_names(path, &content)
}

fn remove_legacy_build_lock(project_dir: &Path) {
    let lock_path = project_dir.join(".tako/build.lock.json");
    if !lock_path.exists() {
        return;
    }
    if let Err(error) = fs::remove_file(&lock_path) {
        tracing::warn!(
            "Failed to remove legacy preset lock file {}: {}",
            lock_path.display(),
            error
        );
    }
}

async fn fetch_preset_content_by_commit(
    repo: &str,
    path: &str,
    commit: &str,
) -> Result<String, String> {
    let url = format!("https://raw.githubusercontent.com/{repo}/{commit}/{path}");
    let client = reqwest::Client::new();
    let response = crate::github::apply_auth(client.get(url).header("User-Agent", "tako-cli"))
        .send()
        .await
        .map_err(|_e| "Failed to fetch preset".to_string())?;
    if !response.status().is_success() {
        return Err("Failed to fetch preset".to_string());
    }
    response
        .text()
        .await
        .map_err(|_e| "Failed to fetch preset".to_string())
}

async fn fetch_preset_content_from_master_branch(
    repo: &str,
    path: &str,
) -> Result<(String, String), String> {
    let commit = fetch_github_branch_commit(repo, OFFICIAL_PRESET_BRANCH).await?;
    let content = fetch_preset_content_by_commit(repo, path, &commit).await?;
    Ok((commit, content))
}

async fn fetch_github_branch_commit(repo: &str, branch: &str) -> Result<String, String> {
    let Some((owner, repository)) = repo.split_once('/') else {
        return Err("Failed to fetch preset".to_string());
    };
    let url = format!("https://api.github.com/repos/{owner}/{repository}/git/ref/heads/{branch}");
    let client = reqwest::Client::new();
    let response =
        crate::github::apply_api_headers(client.get(url).header("User-Agent", "tako-cli"))
            .send()
            .await
            .map_err(|_e| "Failed to fetch preset".to_string())?;
    if !response.status().is_success() {
        return Err("Failed to fetch preset".to_string());
    }
    let raw = response
        .text()
        .await
        .map_err(|_e| "Failed to fetch preset".to_string())?;
    parse_github_branch_commit_sha(&raw)
}

fn parse_github_branch_commit_sha(raw: &str) -> Result<String, String> {
    let json: serde_json::Value =
        serde_json::from_str(raw).map_err(|_e| "Failed to fetch preset".to_string())?;
    let object = json
        .get("object")
        .and_then(|value| value.as_object())
        .ok_or_else(|| "Failed to fetch preset".to_string())?;
    if object.get("type").and_then(|value| value.as_str()) != Some("commit") {
        return Err("Failed to fetch preset".to_string());
    }
    object
        .get("sha")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| "Failed to fetch preset".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fetch_preset_content_from_master_branch_returns_generic_fetch_error() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let err = runtime
            .block_on(fetch_preset_content_from_master_branch(
                "invalid-repo-slug",
                "presets/javascript.toml",
            ))
            .unwrap_err();
        assert_eq!(err, "Failed to fetch preset");
    }

    #[test]
    fn resolve_by_branch_prefers_embedded_over_cache_in_dev_for_official_repo() {
        let _lock = crate::paths::test_tako_home_env_lock();
        let previous = std::env::var_os("TAKO_HOME");
        let home = tempfile::TempDir::new().unwrap();
        unsafe {
            std::env::set_var("TAKO_HOME", home.path());
        }

        let repo = official_preset_repo();
        let path = "presets/javascript.toml";
        let sha = "abcdef1234567";
        // Write stale cached content that is missing sections the embedded
        // manifest contains.
        let stale = r#"
[something-old]
dev = ["old", "command"]
"#;
        crate::build::preset_cache::write_cached(&repo, sha, path, stale).unwrap();
        crate::build::preset_cache::update_freshness(&repo, OFFICIAL_PRESET_BRANCH, sha).unwrap();

        let runtime = tokio::runtime::Runtime::new().unwrap();
        let (resolved_repo, resolved_sha, content) = runtime
            .block_on(resolve_by_branch(
                &repo,
                path,
                OFFICIAL_PRESET_BRANCH,
                PresetResolveMode::Dev,
            ))
            .unwrap();

        match previous {
            Some(value) => unsafe { std::env::set_var("TAKO_HOME", value) },
            None => unsafe { std::env::remove_var("TAKO_HOME") },
        }

        assert_eq!(resolved_repo, repo);
        assert_eq!(resolved_sha, "embedded");
        assert_ne!(content, stale);
        assert!(content.contains("[tanstack-start]"));
    }

    #[test]
    fn resolve_by_branch_uses_stale_cache_immediately_in_dev_mode() {
        let _lock = crate::paths::test_tako_home_env_lock();
        let previous = std::env::var_os("TAKO_HOME");
        let home = tempfile::TempDir::new().unwrap();
        unsafe {
            std::env::set_var("TAKO_HOME", home.path());
        }

        let repo = "invalid-repo-slug";
        let path = "presets/javascript.toml";
        let sha = "abc1234567890";
        let manifest = r#"
[nextjs]
main = ".next/tako-entry.mjs"
dev = ["next", "dev"]
"#;
        crate::build::preset_cache::write_cached(repo, sha, path, manifest).unwrap();

        let repo_dir = crate::paths::tako_cache_dir()
            .unwrap()
            .join("presets")
            .join(repo.replace('/', "__"));
        fs::create_dir_all(&repo_dir).unwrap();
        fs::write(
            repo_dir.join("_meta.json"),
            format!(
                r#"{{
  "branches": {{
    "{branch}": {{
      "sha": "{sha}",
      "last_checked": 0
    }}
  }}
}}"#,
                branch = OFFICIAL_PRESET_BRANCH,
                sha = sha
            ),
        )
        .unwrap();

        let runtime = tokio::runtime::Runtime::new().unwrap();
        let resolved = runtime.block_on(resolve_by_branch(
            repo,
            path,
            OFFICIAL_PRESET_BRANCH,
            PresetResolveMode::Dev,
        ));

        match previous {
            Some(value) => unsafe { std::env::set_var("TAKO_HOME", value) },
            None => unsafe { std::env::remove_var("TAKO_HOME") },
        }

        let (resolved_repo, resolved_sha, content) = resolved.unwrap();
        assert_eq!(resolved_repo, repo);
        assert_eq!(resolved_sha, sha);
        assert_eq!(content, manifest);
    }

    #[test]
    fn parse_github_branch_commit_sha_extracts_commit_sha() {
        let sha = parse_github_branch_commit_sha(
            r#"{
  "ref": "refs/heads/master",
  "object": {
    "sha": "d0ff9bec5b3d42a874b1bff544249b3a4c530d9f",
    "type": "commit"
  }
}"#,
        )
        .unwrap();
        assert_eq!(sha, "d0ff9bec5b3d42a874b1bff544249b3a4c530d9f");
    }

    #[test]
    fn parse_github_branch_commit_sha_rejects_non_commit_objects() {
        let err = parse_github_branch_commit_sha(
            r#"{
  "ref": "refs/heads/master",
  "object": {
    "sha": "eb9c0c1dd0b123ce72c29397826966d831617d0a",
    "type": "blob"
  }
}"#,
        )
        .unwrap_err();
        assert_eq!(err, "Failed to fetch preset");
    }

    #[test]
    fn load_build_preset_ignores_and_removes_legacy_build_lock() {
        let _lock = crate::paths::test_tako_home_env_lock();
        let previous = std::env::var_os("TAKO_HOME");
        let home = tempfile::TempDir::new().unwrap();
        let project = tempfile::TempDir::new().unwrap();
        unsafe {
            std::env::set_var("TAKO_HOME", home.path());
        }

        let repo = official_preset_repo();
        let path = "presets/javascript.toml";
        let branch_sha = "d0ff9bec5b3d42a874b1bff544249b3a4c530d9f";
        let manifest = r#"
[nextjs]
main = ".next/tako-entry.mjs"
dev = ["next", "dev"]
"#;
        crate::build::preset_cache::write_cached(&repo, branch_sha, path, manifest).unwrap();
        crate::build::preset_cache::update_freshness(&repo, OFFICIAL_PRESET_BRANCH, branch_sha)
            .unwrap();

        let lock_path = project.path().join(".tako/build.lock.json");
        fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        fs::write(
            &lock_path,
            r#"{
  "schema_version": 1,
  "preset": {
    "preset_ref": "javascript/nextjs",
    "repo": "lilienblum/tako",
    "path": "presets/javascript.toml",
    "commit": "eb9c0c1dd0b123ce72c29397826966d831617d0a"
  }
}"#,
        )
        .unwrap();

        let runtime = tokio::runtime::Runtime::new().unwrap();
        let (preset, resolved) = runtime
            .block_on(load_build_preset(project.path(), "javascript/nextjs"))
            .unwrap();

        match previous {
            Some(value) => unsafe { std::env::set_var("TAKO_HOME", value) },
            None => unsafe { std::env::remove_var("TAKO_HOME") },
        }

        assert_eq!(preset.name, "nextjs");
        assert_eq!(resolved.commit, branch_sha);
        assert!(!lock_path.exists());
    }

    #[test]
    fn load_available_group_presets_rejects_unknown_group() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let err = runtime
            .block_on(load_available_group_presets(PresetGroup::Unknown))
            .unwrap_err();
        assert!(err.contains("not supported"));
    }
}
