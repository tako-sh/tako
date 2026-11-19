//! `.tako/project.json`: project state that Tako writes and the team shares
//! through version control, unlike the ignored `.tako/state.local.json`. It
//! records each environment's network by stable ID, so commands inside a
//! project name only the environment.

use super::{ConfigError, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

pub const PROJECT_FILE_NAME: &str = "project.json";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectFile {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub environments: BTreeMap<String, ProjectEnvironment>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectEnvironment {
    /// Stable ID of the network the environment deploys to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<String>,
}

fn invalid(error: impl std::fmt::Display) -> ConfigError {
    ConfigError::Validation(error.to_string())
}

impl ProjectFile {
    pub fn path(project_dir: &Path) -> PathBuf {
        project_dir.join(".tako").join(PROJECT_FILE_NAME)
    }

    pub fn load(project_dir: &Path) -> Result<Self> {
        let path = Self::path(project_dir);
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(ConfigError::FileRead(path, error)),
        };
        let file: Self = serde_json::from_str(&content).map_err(|error| {
            invalid(format!(
                "{} is not valid: {error}. A newer Tako may have written it; upgrade with `tako upgrade`.",
                path.display()
            ))
        })?;
        for (env, entry) in &file.environments {
            if let Some(id) = &entry.network
                && id.parse::<tako_network::NetworkId>().is_err()
            {
                return Err(invalid(format!(
                    "{}: environment '{env}' has network '{id}', which is not a network ID",
                    path.display()
                )));
            }
        }
        Ok(file)
    }

    /// The stable ID of the network `env` deploys to, once recorded.
    pub fn network(&self, env: &str) -> Option<&str> {
        self.environments.get(env)?.network.as_deref()
    }

    pub fn set_network(&mut self, env: &str, id: &str) {
        self.environments
            .entry(env.to_string())
            .or_default()
            .network = Some(id.to_string());
    }

    /// Write the file and make sure Git can track it while the rest of
    /// `.tako/` stays ignored.
    pub fn save(&self, project_dir: &Path) -> Result<()> {
        let path = Self::path(project_dir);
        let directory = path.parent().unwrap();
        fs::create_dir_all(directory)
            .map_err(|error| ConfigError::FileWrite(path.clone(), error))?;
        ensure_state_gitignore(directory)
            .map_err(|error| ConfigError::FileWrite(directory.join(".gitignore"), error))?;
        let mut bytes = serde_json::to_vec_pretty(self)?;
        bytes.push(b'\n');
        let temporary = path.with_extension("json.tmp");
        fs::write(&temporary, &bytes)
            .and_then(|()| fs::rename(&temporary, &path))
            .map_err(|error| ConfigError::FileWrite(path.clone(), error))?;
        ensure_project_gitignore(project_dir)
            .map_err(|error| ConfigError::FileWrite(project_dir.join(".gitignore"), error))?;
        Ok(())
    }
}

/// Whether Git ignores `.tako/project.json` in a repository, despite the
/// rules Tako adds. `false` outside a repository.
pub fn git_ignores_project_file(project_dir: &Path) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(project_dir)
        .args(["check-ignore", "-q", "--no-index"])
        .arg(Path::new(".tako").join(PROJECT_FILE_NAME))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

const STATE_GITIGNORE: &str = "*\n!project.json\n";

/// `.tako/.gitignore` ignores everything in `.tako/` except `project.json`.
/// Older Tako versions wrote one that ignored everything.
pub fn ensure_state_gitignore(tako_dir: &Path) -> std::io::Result<()> {
    let path = tako_dir.join(".gitignore");
    match fs::read_to_string(&path) {
        Ok(content) if content == STATE_GITIGNORE => Ok(()),
        Ok(content) if content.trim() == "*" => fs::write(&path, STATE_GITIGNORE),
        // Someone wrote their own rules; leave them.
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::write(&path, STATE_GITIGNORE)
        }
        Err(error) => Err(error),
    }
}

const GITIGNORE_COMMENT: &str = "# tako: local state stays ignored; .tako/project.json is shared";
const IGNORE_STATE: &str = "**/.tako/*";
const KEEP_PROJECT_FILE: &str = "!**/.tako/project.json";

/// Rules for the repository's `.gitignore`, or the project's outside a
/// repository: ignore each app's `.tako/` contents except `project.json`.
/// Rules that ignore the `.tako` directory itself are replaced, since Git
/// cannot re-include a file inside an ignored directory.
pub fn ensure_project_gitignore(project_dir: &Path) -> std::io::Result<()> {
    let root = find_git_repo_root(project_dir).unwrap_or_else(|| project_dir.to_path_buf());
    let path = root.join(".gitignore");
    let previous = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error),
    };
    let mut lines: Vec<String> = Vec::new();
    for line in previous.lines() {
        let trimmed = line.trim();
        if matches!(
            trimmed,
            "!**/.tako/secrets.json"
                | "# tako: ignore runtime artifacts, keep secrets"
                | "# tako: local runtime state and secret drafts"
        ) {
            continue;
        }
        if matches!(
            trimmed,
            ".tako" | ".tako/" | "/.tako" | "/.tako/" | "**/.tako" | "**/.tako/"
        ) {
            lines.push(IGNORE_STATE.to_string());
            continue;
        }
        lines.push(line.to_string());
    }
    let has = |lines: &[String], rule: &str| lines.iter().any(|line| line.trim() == rule);
    let mut added = Vec::new();
    if !has(&lines, GITIGNORE_COMMENT) {
        added.push(GITIGNORE_COMMENT);
    }
    if !has(&lines, IGNORE_STATE) {
        added.push(IGNORE_STATE);
    }
    // The exception must follow the rule it overrides, so it always goes last.
    lines.retain(|line| line.trim() != KEEP_PROJECT_FILE);
    added.push(KEEP_PROJECT_FILE);
    lines.extend(added.into_iter().map(str::to_string));
    let mut content = lines.join("\n");
    content.push('\n');
    if content != previous {
        fs::write(path, content)?;
    }
    Ok(())
}

pub fn find_git_repo_root(project_dir: &Path) -> Option<PathBuf> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(project_dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(PathBuf::from(String::from_utf8(output.stdout).ok()?.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_networks_by_environment() {
        let directory = tempfile::TempDir::new().unwrap();
        assert_eq!(
            ProjectFile::load(directory.path()).unwrap(),
            ProjectFile::default()
        );
        let mut file = ProjectFile::default();
        file.set_network("production", &"a".repeat(32));
        file.save(directory.path()).unwrap();
        let loaded = ProjectFile::load(directory.path()).unwrap();
        assert_eq!(loaded.network("production"), Some("a".repeat(32).as_str()));
        assert_eq!(loaded.network("staging"), None);
        let written = fs::read_to_string(ProjectFile::path(directory.path())).unwrap();
        assert!(written.contains("\"environments\""), "{written}");
        assert_eq!(
            fs::read_to_string(directory.path().join(".tako/.gitignore")).unwrap(),
            STATE_GITIGNORE
        );
    }

    #[test]
    fn rejects_unknown_fields_and_invalid_ids() {
        let directory = tempfile::TempDir::new().unwrap();
        fs::create_dir_all(directory.path().join(".tako")).unwrap();
        let path = ProjectFile::path(directory.path());
        fs::write(&path, r#"{"environments":{"production":{"network":"x"}}}"#).unwrap();
        assert!(ProjectFile::load(directory.path()).is_err());
        fs::write(&path, r#"{"future":true}"#).unwrap();
        assert!(ProjectFile::load(directory.path()).is_err());
    }

    #[test]
    fn state_gitignore_upgrades_the_old_ignore_everything_rule() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join(".gitignore");
        fs::write(&path, "*\n").unwrap();
        ensure_state_gitignore(directory.path()).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), STATE_GITIGNORE);
        fs::write(&path, "custom\n").unwrap();
        ensure_state_gitignore(directory.path()).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "custom\n");
    }

    #[test]
    fn project_gitignore_keeps_the_project_file_after_the_state_rule() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join(".gitignore");
        fs::write(&path, "node_modules\n.tako/\n!**/.tako/project.json\n").unwrap();
        ensure_project_gitignore(directory.path()).unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(
            content,
            format!("node_modules\n{IGNORE_STATE}\n{GITIGNORE_COMMENT}\n{KEEP_PROJECT_FILE}\n")
        );
        ensure_project_gitignore(directory.path()).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), content);
    }
}
