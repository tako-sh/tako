use std::path::{Path, PathBuf};

const GENERATED_DECLARATION_DIRS: &[&str] = &["app", "src"];

pub(super) fn resolve_declaration_path(project_dir: &Path) -> PathBuf {
    for dir in generated_declaration_parent_dirs(project_dir) {
        let candidate = dir.join("tako.d.ts");
        if candidate.is_file() {
            return candidate;
        }
    }

    for dir in generated_declaration_parent_dirs(project_dir) {
        let legacy = dir.join("tako.gen.ts");
        if legacy.is_file() {
            return dir.join("tako.d.ts");
        }
    }

    for dir in GENERATED_DECLARATION_DIRS {
        let candidate_dir = project_dir.join(dir);
        if candidate_dir.is_dir() {
            return candidate_dir.join("tako.d.ts");
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
        .any(|dir| path == dir.join("tako.d.ts") || path == dir.join("tako.gen.ts"))
}
