use std::path::{Path, PathBuf};

/// Where an app's generated JavaScript files go: its package (the nearest
/// directory with a `package.json` from the app root up to, but not including,
/// the project), or the project itself. Configs for different packages in one
/// project then keep separate generated files.
pub(crate) fn generated_output_dir(project_dir: &Path, app_root_dir: &Path) -> PathBuf {
    app_root_dir
        .ancestors()
        .take_while(|dir| *dir != project_dir && dir.starts_with(project_dir))
        .find(|dir| dir.join("package.json").is_file())
        .unwrap_or(project_dir)
        .to_path_buf()
}

pub(super) fn resolve_declaration_path(project_dir: &Path) -> PathBuf {
    for dir in generated_declaration_parent_dirs(project_dir) {
        let candidate = dir.join("tako.d.ts");
        if candidate.is_file() {
            return candidate;
        }
    }

    project_dir.join("tako.d.ts")
}

pub(crate) fn generated_declaration_parent_dirs(project_dir: &Path) -> Vec<PathBuf> {
    vec![
        project_dir.join("app"),
        project_dir.join("src"),
        project_dir.to_path_buf(),
    ]
}

pub(crate) fn is_generated_declaration_path(project_dir: &Path, path: &Path) -> bool {
    generated_declaration_parent_dirs(project_dir)
        .into_iter()
        .any(|dir| path == dir.join("tako.d.ts"))
}
