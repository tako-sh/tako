use std::path::Path;

use crate::build::{BuildAdapter, PresetGroup, go, js};
use crate::output;

use super::scaffold::sdk_install_command;

pub(super) fn install_tako_sdk(project_dir: &Path, runtime: BuildAdapter) {
    let Some(cmd) = sdk_install_command(runtime, project_dir) else {
        return;
    };
    if runtime == BuildAdapter::Node {
        ensure_pnpm(project_dir);
    }
    output::info(&format!("Installing tako.sh SDK: {}", output::strong(&cmd)));
    let result = std::process::Command::new("sh")
        .args(["-c", &cmd])
        .current_dir(project_dir)
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status();
    match result {
        Ok(status) if status.success() => {}
        _ => {
            output::info(&format!(
                "Could not install tako.sh automatically. Run {} manually.",
                output::strong(&cmd)
            ));
        }
    }
}

pub(super) fn write_init_generated_file(
    project_dir: &Path,
    adapter: BuildAdapter,
    app_root: &str,
) -> Result<Option<&'static str>, Box<dyn std::error::Error>> {
    match adapter.preset_group() {
        PresetGroup::Js => {
            if js::write_tako_declarations_for_adapter_and_app_root(project_dir, adapter, app_root)?
            {
                Ok(Some("tako.d.ts"))
            } else {
                Ok(None)
            }
        }
        PresetGroup::Go => {
            if go::write_secret_accessors(project_dir)? {
                Ok(Some("tako_secrets.go"))
            } else {
                Ok(None)
            }
        }
        PresetGroup::Unknown => Ok(None),
    }
}

fn ensure_pnpm(project_dir: &Path) {
    let has_pnpm = std::process::Command::new("pnpm")
        .arg("--version")
        .current_dir(project_dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if has_pnpm {
        return;
    }
    output::info("Installing pnpm…");
    let _ = std::process::Command::new("npm")
        .args(["install", "-g", "pnpm"])
        .current_dir(project_dir)
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status();
}
