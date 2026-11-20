use std::fs;
use std::path::{Path, PathBuf};

pub(super) fn ensure_project_gitignore_ignores_state(project_dir: &Path) -> std::io::Result<()> {
    let root = find_git_repo_root(project_dir).unwrap_or_else(|| project_dir.to_path_buf());
    let path = root.join(".gitignore");
    let previous = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error),
    };
    let mut lines: Vec<&str> = previous
        .lines()
        .filter(|line| {
            !matches!(
                line.trim(),
                "!**/.tako/secrets.json" | "# tako: ignore runtime artifacts, keep secrets"
            )
        })
        .collect();
    for rule in [
        "# tako: local runtime state and secret drafts",
        "**/.tako/*",
    ] {
        if !lines.iter().any(|line| line.trim() == rule) {
            lines.push(rule);
        }
    }
    let content = format!("{}\n", lines.join("\n"));
    if content != previous {
        fs::write(path, content)?;
    }
    Ok(())
}

fn find_git_repo_root(project_dir: &Path) -> Option<PathBuf> {
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

pub(super) fn display_config_path_for_prompt(config_path: &Path, cwd: &Path) -> String {
    let canonical_cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());

    config_path
        .strip_prefix(&canonical_cwd)
        .ok()
        .filter(|path| !path.as_os_str().is_empty())
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| config_path.display().to_string())
}
