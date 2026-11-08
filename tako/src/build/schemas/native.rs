use super::{Manifest, model::invalid, resolve, write_generated};
use crate::config::TakoToml;
use std::{
    io,
    path::{Path, PathBuf},
    process::Command,
};

pub fn native_root(project: &Path, config: &TakoToml) -> PathBuf {
    config
        .build
        .cwd
        .as_ref()
        .map_or_else(|| project.to_path_buf(), |cwd| project.join(cwd))
}

/// Native projects opt into exporting their API by providing the conventional
/// schema entry point. Import generation never needs the producer's toolchain.
pub fn generate_native(
    project: &Path,
    config: &TakoToml,
    rust: bool,
    update: bool,
) -> io::Result<bool> {
    let root = native_root(project, config);
    let app = config
        .name
        .as_ref()
        .ok_or_else(|| invalid("schema generation requires an app name"))?;
    let resolved = resolve(project, &config.schemas, update)?;
    if resolved
        .manifests
        .iter()
        .any(|schema| &schema.tako.app == app)
    {
        return Err(invalid("schemas cannot import the current app"));
    }
    let imports = if rust {
        super::rust(&resolved.manifests)?
    } else {
        super::golang(
            &resolved.manifests,
            &crate::build::go::detect_package_name(&root),
        )?
    };
    let exporter = if rust {
        root.join("examples/tako_schema.rs")
    } else {
        root.join("tako-schema/main.go")
    };
    let imports_path = root.join(if rust {
        "tako_imports.rs"
    } else {
        "tako_imports.go"
    });
    let previous = match std::fs::read(&imports_path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    // The exporting application can itself import generated types. Stage those
    // first so a clean checkout compiles, and restore them if exporting fails.
    let mut staged = StagedImports {
        path: imports_path,
        previous,
        committed: false,
    };
    let mut changed = write_generated(&staged.path, &imports)?;
    let schema = if exporter.is_file() {
        let mut command = if rust {
            let mut c = Command::new("cargo");
            c.args([
                "run",
                "--quiet",
                "--example",
                "tako_schema",
                "--features",
                "tako/schema",
            ]);
            c
        } else {
            let mut c = Command::new("go");
            c.args(["run", "./tako-schema"]);
            c
        };
        let output = command.current_dir(&root).output()?;
        if !output.status.success() {
            return Err(invalid(format!(
                "schema exporter failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        if output.stdout.len() > 4 * 1024 * 1024 {
            return Err(invalid("exported schema exceeds 4 MiB"));
        }
        let schema: Manifest = serde_json::from_slice(&output.stdout)?;
        if &schema.tako.app != app {
            return Err(invalid("exported schema app does not match tako.toml"));
        }
        schema.validate()?;
        // Validate portability before publishing a schema consumers cannot use.
        super::typescript(std::slice::from_ref(&schema))?;
        schema
    } else {
        Manifest::empty(app.clone())
    };
    let schema = serde_json::to_string_pretty(&schema)? + "\n";
    changed |= write_generated(&project.join("tako-schema.json"), &schema)?;
    changed |= resolved.commit()?;
    staged.committed = true;
    Ok(changed)
}

struct StagedImports {
    path: PathBuf,
    previous: Option<Vec<u8>>,
    committed: bool,
}
impl Drop for StagedImports {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if let Some(bytes) = &self.previous {
            let _ = std::fs::write(&self.path, bytes);
        } else {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}
