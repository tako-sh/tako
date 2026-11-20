use std::path::Path;

pub(super) fn ensure_project_gitignore_ignores_state(project_dir: &Path) -> std::io::Result<()> {
    crate::config::ensure_project_gitignore(project_dir)
}

pub(super) fn display_config_path_for_prompt(config_path: &Path, cwd: &Path) -> String {
    let canonical_cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());

    config_path
        .strip_prefix(&canonical_cwd)
        .ok()
        .filter(|path| !path.as_os_str().is_empty())
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| config_path.display().to_string())
}
