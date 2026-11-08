use std::path::{Path, PathBuf};

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
