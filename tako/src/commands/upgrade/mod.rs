mod install;
mod task_tree;
pub(crate) mod version;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::output;

use install::{detect_platform, download_and_install, resolve_install_dir};
use task_tree::LocalUpgradeTask;
use version::{UpdateCheck, check_for_updates, current_version, tarball_url};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CliUpgradeMethod {
    Installer,
    Homebrew,
}

#[derive(Debug, Clone)]
struct CliUpgradeDetectionContext {
    current_exe: PathBuf,
    has_brew: bool,
    brew_formula_installed: bool,
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Runtime::new()?;
    let ver = current_version();
    output::line(&format!("Current version: {}", output::strong(&ver)));
    tracing::info!("Current version: {ver}");
    let task = LocalUpgradeTask::start("Upgrading");

    rt.block_on(run_upgrade(task))
}

async fn run_upgrade(task: LocalUpgradeTask) -> Result<(), Box<dyn std::error::Error>> {
    use crate::dev_server_client::DevServerStop;
    let result = match detect_cli_upgrade_method_runtime() {
        CliUpgradeMethod::Installer => run_installer_upgrade().await,
        CliUpgradeMethod::Homebrew => run_brew_upgrade(),
    };
    let (label, stop) = match result {
        Ok(None) => (
            "Already on the latest version".to_string(),
            DevServerStop::NotNeeded,
        ),
        Ok(Some(label)) => match crate::dev_server_client::stop_server_if_running().await {
            Ok(stop) => (label, stop),
            Err(error) => {
                let message = format!("{label}, but could not stop the local dev server: {error}");
                task.fail("Dev server shutdown failed", &message);
                return Err(message.into());
            }
        },
        Err(error) => {
            task.fail("Upgrade failed", error.to_string());
            return Err(error);
        }
    };
    task.succeed(&label);
    if !task.is_rendered() {
        output::info(&label);
    }
    match stop {
        DevServerStop::Stopped => output::hint(
            "Stopped the local dev server and its apps. Run `tako dev` in each project to start it again.",
        ),
        DevServerStop::StillStopping => output::hint(
            "The local dev server is still shutting down. Let it finish before running `tako dev` again.",
        ),
        DevServerStop::NotNeeded => {}
    }
    Ok(())
}

async fn run_installer_upgrade() -> Result<Option<String>, Box<dyn std::error::Error>> {
    match check_for_updates().await? {
        UpdateCheck::AlreadyCurrent => Ok(None),
        UpdateCheck::Available { version } => {
            let (os, arch) = detect_platform()?;
            download_and_install(&tarball_url(os, arch), &resolve_install_dir()).await?;
            Ok(Some(format!("Upgraded to {version}")))
        }
    }
}

fn run_brew_upgrade() -> Result<Option<String>, Box<dyn std::error::Error>> {
    // `brew upgrade` also succeeds when nothing changed.
    let before = homebrew_formula_versions()?;
    run_local_upgrade_command("brew", &["upgrade", "tako"])?;
    Ok((homebrew_formula_versions()? != before).then(|| "Upgraded via Homebrew".into()))
}

fn run_local_upgrade_command(binary: &str, args: &[&str]) -> Result<String, String> {
    let result = Command::new(binary)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("failed to start {}: {}", binary, e))?;

    if result.status.success() {
        return Ok(String::from_utf8_lossy(&result.stdout).trim().to_owned());
    }

    let stderr = String::from_utf8_lossy(&result.stderr);
    let detail = stderr.trim();
    if detail.is_empty() {
        Err(format!("{binary} exited with a non-zero status"))
    } else {
        Err(format!("{binary}: {detail}"))
    }
}

fn build_cli_upgrade_detection_context() -> CliUpgradeDetectionContext {
    let has_brew = command_exists("brew");

    CliUpgradeDetectionContext {
        current_exe: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("tako")),
        has_brew,
        brew_formula_installed: has_brew
            && homebrew_formula_versions().is_ok_and(|s| !s.is_empty()),
    }
}

fn detect_cli_upgrade_method_runtime() -> CliUpgradeMethod {
    let ctx = build_cli_upgrade_detection_context();
    detect_cli_upgrade_method(&ctx)
}

fn detect_cli_upgrade_method(ctx: &CliUpgradeDetectionContext) -> CliUpgradeMethod {
    if ctx.has_brew && is_homebrew_path(&ctx.current_exe) {
        return CliUpgradeMethod::Homebrew;
    }

    if ctx.has_brew && ctx.brew_formula_installed {
        return CliUpgradeMethod::Homebrew;
    }

    CliUpgradeMethod::Installer
}

fn is_homebrew_path(path: &Path) -> bool {
    let value = path.to_string_lossy();
    value.starts_with("/opt/homebrew/")
        || value.starts_with("/usr/local/Homebrew/")
        || value.starts_with("/home/linuxbrew/.linuxbrew/")
        || value.contains("/Cellar/tako/")
}

fn homebrew_formula_versions() -> Result<String, String> {
    run_local_upgrade_command("brew", &["list", "--formula", "--versions", "tako"])
}

fn command_exists(name: &str) -> bool {
    Command::new(name)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tarball_url_constructs_github_url() {
        let url = tarball_url("darwin", "aarch64");
        assert_eq!(
            url,
            "https://github.com/tako-sh/tako/releases/download/latest/tako-darwin-aarch64.tar.gz"
        );
    }

    #[test]
    fn detect_cli_upgrade_method_prefers_homebrew_path() {
        let ctx = CliUpgradeDetectionContext {
            current_exe: PathBuf::from("/opt/homebrew/bin/tako"),
            has_brew: true,
            brew_formula_installed: true,
        };
        assert_eq!(detect_cli_upgrade_method(&ctx), CliUpgradeMethod::Homebrew);
    }

    #[test]
    fn detect_cli_upgrade_method_uses_formula_presence_when_path_is_generic() {
        let ctx = CliUpgradeDetectionContext {
            current_exe: PathBuf::from("/usr/local/bin/tako"),
            has_brew: true,
            brew_formula_installed: true,
        };
        assert_eq!(detect_cli_upgrade_method(&ctx), CliUpgradeMethod::Homebrew);
    }

    #[test]
    fn detect_cli_upgrade_method_falls_back_to_installer() {
        let ctx = CliUpgradeDetectionContext {
            current_exe: PathBuf::from("/usr/local/bin/tako"),
            has_brew: false,
            brew_formula_installed: false,
        };
        assert_eq!(detect_cli_upgrade_method(&ctx), CliUpgradeMethod::Installer);
    }
}
