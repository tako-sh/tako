use std::env::current_dir;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectContext {
    pub project_dir: PathBuf,
    pub config_path: PathBuf,
}

impl ProjectContext {
    pub fn load_secrets(&self) -> crate::config::Result<crate::config::SecretsStore> {
        let config = crate::config::TakoToml::load_from_file(&self.config_path)?;
        crate::config::SecretsStore::load_with_config(&self.project_dir, &config)
    }

    pub fn config_key(&self) -> String {
        std::fs::canonicalize(&self.config_path)
            .unwrap_or_else(|_| self.config_path.clone())
            .to_string_lossy()
            .to_string()
    }
}

pub fn resolve(config: Option<&Path>) -> Result<ProjectContext, Box<dyn std::error::Error>> {
    let cwd = current_dir()?;
    resolve_from_cwd(&cwd, config)
}

fn resolve_from_cwd(
    cwd: &Path,
    config: Option<&Path>,
) -> Result<ProjectContext, Box<dyn std::error::Error>> {
    let requested_path = config
        .map(|path| normalize_requested_path(cwd, path))
        .unwrap_or_else(|| cwd.join("tako.toml"));

    if requested_path.exists() && requested_path.is_dir() {
        return Err(format!(
            "--config must point to a file, got directory '{}'",
            requested_path.display()
        )
        .into());
    }

    let raw_project_dir = requested_path.parent().ok_or_else(|| {
        format!(
            "Could not determine a project directory for config '{}'",
            requested_path.display()
        )
    })?;
    if !raw_project_dir.exists() {
        return Err(format!(
            "Config directory does not exist: {}",
            raw_project_dir.display()
        )
        .into());
    }

    let project_dir =
        std::fs::canonicalize(raw_project_dir).unwrap_or_else(|_| raw_project_dir.to_path_buf());
    let file_name = requested_path.file_name().ok_or_else(|| {
        format!(
            "Could not determine a config filename from '{}'",
            requested_path.display()
        )
    })?;
    let config_path = if requested_path.exists() {
        std::fs::canonicalize(&requested_path).unwrap_or(requested_path)
    } else {
        project_dir.join(file_name)
    };

    Ok(ProjectContext {
        project_dir,
        config_path,
    })
}

pub fn resolve_existing(
    config: Option<&Path>,
) -> Result<ProjectContext, Box<dyn std::error::Error>> {
    let cwd = current_dir()?;
    let context = resolve_existing_from_cwd(&cwd, config)?;
    Ok(context)
}

fn resolve_existing_from_cwd(
    cwd: &Path,
    config: Option<&Path>,
) -> Result<ProjectContext, Box<dyn std::error::Error>> {
    let context = resolve_from_cwd(cwd, config)?;
    if !context.config_path.is_file() {
        return Err(format!("Config file not found: {}", context.config_path.display()).into());
    }
    Ok(context)
}

pub fn resolve_optional(
    config: Option<&Path>,
) -> Result<Option<ProjectContext>, Box<dyn std::error::Error>> {
    let cwd = current_dir()?;
    resolve_optional_from_cwd(&cwd, config)
}

fn resolve_optional_from_cwd(
    cwd: &Path,
    config: Option<&Path>,
) -> Result<Option<ProjectContext>, Box<dyn std::error::Error>> {
    let context = resolve_from_cwd(cwd, config)?;
    if context.config_path.is_file() {
        return Ok(Some(context));
    }
    if config.is_some() {
        return Err(format!("Config file not found: {}", context.config_path.display()).into());
    }
    Ok(None)
}

fn normalize_requested_path(cwd: &Path, path: &Path) -> PathBuf {
    let requested = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    ensure_toml_suffix(requested)
}

fn ensure_toml_suffix(path: PathBuf) -> PathBuf {
    if path
        .file_name()
        .is_some_and(|name| name.to_string_lossy().ends_with(".toml"))
    {
        return path;
    }

    let mut with_suffix = path.into_os_string();
    with_suffix.push(".toml");
    PathBuf::from(with_suffix)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn resolve_defaults_to_tako_toml_in_current_directory() {
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("tako.toml"), "name = \"demo\"\n").unwrap();

        let canonical_temp = std::fs::canonicalize(temp.path()).unwrap();
        let resolved = resolve_from_cwd(&canonical_temp, None).unwrap();
        assert_eq!(resolved.project_dir, canonical_temp);
        assert_eq!(resolved.config_path, canonical_temp.join("tako.toml"));
    }

    #[test]
    fn resolve_uses_parent_of_explicit_config_as_project_dir() {
        let temp = TempDir::new().unwrap();
        let config_dir = temp.path().join("configs");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("preview.toml"), "name = \"demo\"\n").unwrap();

        let canonical_config_dir = std::fs::canonicalize(&config_dir).unwrap();
        let resolved =
            resolve_from_cwd(temp.path(), Some(Path::new("configs/preview.toml"))).unwrap();
        assert_eq!(resolved.project_dir, canonical_config_dir);
        assert_eq!(
            resolved.config_path,
            canonical_config_dir.join("preview.toml")
        );
    }

    #[test]
    fn resolve_adds_toml_suffix_for_explicit_config_without_extension() {
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("preview.toml"), "name = \"demo\"\n").unwrap();

        let canonical_temp = std::fs::canonicalize(temp.path()).unwrap();
        let resolved = resolve_from_cwd(temp.path(), Some(Path::new("preview"))).unwrap();

        assert_eq!(resolved.project_dir, canonical_temp);
        assert_eq!(resolved.config_path, canonical_temp.join("preview.toml"));
    }

    #[test]
    fn resolve_existing_errors_when_config_file_is_missing() {
        let temp = TempDir::new().unwrap();
        let missing = temp.path().join("preview.toml");
        let expected = std::fs::canonicalize(temp.path())
            .unwrap()
            .join("preview.toml");

        let err = resolve_existing_from_cwd(temp.path(), Some(&missing)).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("Config file not found: {}", expected.display())
        );
    }

    #[test]
    fn resolve_existing_missing_config_without_extension_reports_toml_path() {
        let temp = TempDir::new().unwrap();
        let expected = std::fs::canonicalize(temp.path())
            .unwrap()
            .join("preview.toml");

        let err = resolve_existing_from_cwd(temp.path(), Some(Path::new("preview"))).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("Config file not found: {}", expected.display())
        );
    }

    #[test]
    fn resolve_optional_returns_none_for_missing_default_config() {
        let temp = TempDir::new().unwrap();

        let resolved = resolve_optional_from_cwd(temp.path(), None).unwrap();
        assert!(resolved.is_none());
    }
}
