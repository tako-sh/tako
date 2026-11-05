use std::fs;
use std::path::Path;
use std::process::Command;

use super::js_app_root_dir;

pub(super) fn generate(
    project_dir: &Path,
    app_root: &str,
    codegen_command: &impl Fn() -> Command,
) -> std::io::Result<Option<String>> {
    let root = js_app_root_dir(project_dir, app_root);
    if !root.join("services").exists() && !root.join("tako.services.ts").exists() {
        return Ok(None);
    }

    let root = fs::canonicalize(root)?;
    let output = match codegen_command()
        .args(["run", "tako-sh-gen-service-types"])
        .arg(root.join("services"))
        .current_dir(&root)
        .output()
    {
        Ok(output) => output,
        // No JavaScript runtime on this machine: keep the committed contract, as
        // channel and store generation do, instead of failing deploy and dev.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(std::io::Error::new(
                err.kind(),
                format!("service generation failed: {err}"),
            ));
        }
    };
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "service generation failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    String::from_utf8(output.stdout)
        .map(Some)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))
}
